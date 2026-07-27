//! Background-threaded window animator.
//!
//! [`WindowAnimator`] owns a single worker thread that drives the animation
//! loop. The public API sends commands over a [`crossbeam_channel`] and
//! reads the [`AtomicBool`] flag to report animation status without locking.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::animation::backend::WindowBackend;
use crate::animation::batch::{AnimationBatch, build_tweens};
use crate::animation::config::{AnimatorConfig, InterruptPolicy};
use crate::animation::types::{
    AnimationError, AnimationHandle, Rect, Result, WindowRef, WindowTarget,
};

// ---------------------------------------------------------------------------
// Internal command type
// ---------------------------------------------------------------------------

/// Commands sent from [`WindowAnimator`] to the worker thread.
enum AnimatorCmd {
    /// Start or interrupt an animation with the given targets.
    Animate {
        targets: Vec<WindowTarget>,
        config: AnimatorConfig,
    },
    /// Cancel any in-flight animation and clear the queue.
    Cancel,
    /// Replace the config for future animations.
    UpdateConfig(AnimatorConfig),
    /// Ask the worker to exit cleanly.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Internal worker-state types
// ---------------------------------------------------------------------------

/// An animation batch currently being driven by the worker.
struct ActiveBatch {
    batch: AnimationBatch,
    /// Configuration that was active when this batch was started.
    /// Frozen at start time so mid-animation `update_config` calls cannot
    /// alter the easing curve of an in-flight animation.
    config: AnimatorConfig,
}

/// An animation request waiting in the queue (used by [`InterruptPolicy::QueueAfterCurrent`]).
struct PendingBatch {
    targets: Vec<WindowTarget>,
    config: AnimatorConfig,
}

/// All mutable state owned exclusively by the worker thread.
struct WorkerState {
    /// Last config seen by the worker. Informational only — all animation
    /// decisions use `active.config` (frozen per batch) or the config
    /// embedded in `AnimatorCmd::Animate`. Updated by `UpdateConfig` and
    /// by `start_batch` so it stays in sync for potential future introspection.
    config: AnimatorConfig,
    active: Option<ActiveBatch>,
    queue: VecDeque<PendingBatch>,
}

// ---------------------------------------------------------------------------
// WindowAnimator — public API
// ---------------------------------------------------------------------------

/// Rect-based window animator backed by a dedicated worker thread.
///
/// Submit animation batches with [`Self::animate`]; the worker thread drives
/// the frame loop using the configured backend (Win32 on Windows, mock in
/// tests). Configuration can be updated at any time via [`Self::update_config`].
///
/// # Thread Safety
///
/// `WindowAnimator` is not `Sync` — all methods take `&mut self` to prevent
/// concurrent use from multiple threads. The worker thread communicates
/// exclusively through the internal channel and an [`AtomicBool`].
pub struct WindowAnimator {
    /// Current config; cloned into commands so the worker always receives it.
    config: AnimatorConfig,
    /// Sends commands to the worker thread.
    cmd_tx: crossbeam_channel::Sender<AnimatorCmd>,
    /// Set to `true` by the worker when an animation starts; cleared on completion or cancel.
    animating: Arc<AtomicBool>,
    /// Monotonically-increasing counter used to generate [`AnimationHandle`] IDs.
    next_id: Arc<AtomicU64>,
    /// Worker thread handle. Kept for its drop behaviour (detaches thread on drop).
    _worker: std::thread::JoinHandle<()>,
}

impl WindowAnimator {
    /// Create a new animator with the given backend and initial configuration.
    ///
    /// Spawns the worker thread immediately. The thread exits when this
    /// `WindowAnimator` is dropped.
    pub fn new(backend: impl WindowBackend, config: AnimatorConfig) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AnimatorCmd>();
        let animating = Arc::new(AtomicBool::new(false));
        let next_id = Arc::new(AtomicU64::new(1));

        let animating_worker = Arc::clone(&animating);
        let initial_config = config.clone();

        // SAFETY / rationale: thread spawn can fail only if the OS has
        // exhausted thread resources, which is unrecoverable for this crate.
        // `.expect()` is the documented exception to the no-unwrap rule.
        let _worker = std::thread::Builder::new()
            .name("window-animation-worker".into())
            .spawn(move || {
                run_worker(Box::new(backend), cmd_rx, animating_worker, initial_config);
            })
            .expect("failed to spawn animation worker thread");

        Self {
            config,
            cmd_tx,
            animating,
            next_id,
            _worker,
        }
    }

    /// Submit a new animation batch.
    ///
    /// Behaviour when an animation is already playing depends on
    /// `config.interrupt_policy`:
    /// - [`InterruptPolicy::RetargetFromCurrent`]: cancel the active batch and
    ///   start the new one from the current interpolated positions.
    /// - [`InterruptPolicy::QueueAfterCurrent`]: finish the current batch then
    ///   play the new one.
    /// - [`InterruptPolicy::DropNew`]: discard the new request silently.
    ///
    /// # Errors
    ///
    /// Returns [`AnimationError::EmptyBatch`] if `targets` is empty.
    /// Returns [`AnimationError::WorkerDead`] if the worker thread has exited.
    pub fn animate(&mut self, targets: Vec<WindowTarget>) -> Result<AnimationHandle> {
        if targets.is_empty() {
            return Err(AnimationError::EmptyBatch);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cmd = AnimatorCmd::Animate {
            targets,
            config: self.config.clone(),
        };
        self.cmd_tx
            .send(cmd)
            .map_err(|_| AnimationError::WorkerDead)?;
        Ok(AnimationHandle(id))
    }

    /// Replace the configuration.
    ///
    /// Takes effect at the start of the next animation. Any animation currently
    /// in flight continues under its original configuration.
    pub fn update_config(&mut self, config: AnimatorConfig) {
        self.config = config.clone();
        // Best-effort: ignore send error if the worker has already exited.
        let _ = self.cmd_tx.send(AnimatorCmd::UpdateConfig(config));
    }

    /// Returns `true` while an animation is in progress.
    pub fn is_animating(&self) -> bool {
        self.animating.load(Ordering::Acquire)
    }

    /// Cancel any in-flight animation immediately and clear the pending queue.
    pub fn cancel(&mut self) {
        // Best-effort: ignore send error if the worker has already exited.
        let _ = self.cmd_tx.send(AnimatorCmd::Cancel);
    }
}

impl Drop for WindowAnimator {
    fn drop(&mut self) {
        // Ask the worker to exit cleanly.
        // Note: JoinHandle::join() requires ownership, which cannot be taken
        // from &mut self, so we only signal — the worker exits on its own.
        let _ = self.cmd_tx.send(AnimatorCmd::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

/// Entry-point for the background worker thread.
///
/// Drives the animation loop: when idle it blocks on the channel; when
/// animating it polls commands between frames.
fn run_worker(
    backend: Box<dyn WindowBackend>,
    rx: crossbeam_channel::Receiver<AnimatorCmd>,
    animating: Arc<AtomicBool>,
    initial_config: AnimatorConfig,
) {
    let mut state = WorkerState {
        config: initial_config,
        active: None,
        queue: VecDeque::new(),
    };

    'main: loop {
        // When animating: non-blocking poll so we can tick the frame loop.
        // When idle: block on the channel to avoid busy-spinning.
        let cmd_opt: Option<AnimatorCmd> = if state.active.is_some() {
            rx.try_recv().ok()
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break 'main, // channel disconnected — exit cleanly
            }
        };

        if let Some(cmd) = cmd_opt {
            match cmd {
                AnimatorCmd::Shutdown => break 'main,
                AnimatorCmd::Cancel => {
                    state.active = None;
                    state.queue.clear();
                    animating.store(false, Ordering::Release);
                }
                AnimatorCmd::UpdateConfig(cfg) => {
                    state.config = cfg;
                }
                AnimatorCmd::Animate { targets, config } => {
                    handle_animate(&mut state, &*backend, targets, config, &animating);
                }
            }
        }

        // Tick the active animation, if there is one.
        if let Some(active) = &state.active {
            let t = active.batch.progress();

            // Gather interpolated positions for this frame.
            let updates: Vec<(WindowRef, Rect)> = active
                .batch
                .tweens
                .iter()
                .map(|tween| {
                    // Use the config frozen at batch-start time, not the current
                    // state.config, so mid-animation update_config() has no effect.
                    let r = AnimationBatch::interpolated_rect(tween, &active.config, t);
                    (tween.window_ref, r)
                })
                .collect();

            // Apply the batch — best-effort; backend errors are non-fatal here.
            if let Err(e) = backend.apply_batch(&updates) {
                log::warn!("apply_batch error at t={t:.3}: {e}");
            }

            // Use the same clamped `t` we just applied — NOT a fresh
            // `is_complete()` / `progress()` re-read. The `apply_batch` call
            // above (SetWindowPos round-trip) consumes wall-clock time, so a
            // second progress() sample can tick past 1.0 *while this frame was
            // interpolated at `t < 1.0`*. That would clear the batch without
            // ever applying the precise t = 1.0 frame, leaving windows a few
            // pixels short of their target geometry (the "resizing column
            // width comes out wrong" bug). Since `progress()` already clamps
            // to 1.0, gating completion on this same `t` guarantees the batch
            // is only cleared on the frame that was applied at exactly the
            // target rect.
            if t >= 1.0 {
                // Animation complete — clear the active batch.
                log::trace!("animation complete: batch finished at t={t:.3}");
                state.active = None;
                animating.store(false, Ordering::Release);

                // Start the next queued batch if one is waiting.
                if let Some(pending) = state.queue.pop_front() {
                    start_batch(&mut state, &*backend, pending, &animating);
                }
            } else {
                // Pace with DwmFlush — blocks until the next DWM composition
                // cycle (~16 ms at 60 Hz). Non-fatal if it fails.
                let _ = backend.dwm_flush();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// handle_animate — interrupt policy dispatcher
// ---------------------------------------------------------------------------

/// Dispatch a new [`AnimatorCmd::Animate`] command according to the configured
/// interrupt policy.
fn handle_animate(
    state: &mut WorkerState,
    backend: &dyn WindowBackend,
    targets: Vec<WindowTarget>,
    config: AnimatorConfig,
    animating: &Arc<AtomicBool>,
) {
    match config.interrupt_policy {
        InterruptPolicy::DropNew => {
            if state.active.is_some() {
                // Silently discard the incoming request.
                return;
            }
        }
        InterruptPolicy::QueueAfterCurrent => {
            if state.active.is_some() {
                state.queue.push_back(PendingBatch { targets, config });
                return;
            }
        }
        InterruptPolicy::RetargetFromCurrent => {
            // Fall through: start_batch will sample current interpolated
            // positions and replace the active batch.
        }
    }

    let pending = PendingBatch { targets, config };
    start_batch(state, backend, pending, animating);
}

// ---------------------------------------------------------------------------
// start_batch — build tweens and activate
// ---------------------------------------------------------------------------

/// Build an [`AnimationBatch`] from the current window geometry and make it
/// the active animation.
///
/// For windows that are mid-animation, the current interpolated position is
/// used as the starting rect so the transition appears continuous. For all
/// other windows the backend is queried.
///
/// If all tweens are no-ops (source rect == target rect) the function returns
/// early without starting a batch.
fn start_batch(
    state: &mut WorkerState,
    backend: &dyn WindowBackend,
    pending: PendingBatch,
    animating: &Arc<AtomicBool>,
) {
    // Sample current progress of the active batch, if any.
    // `progress()` already clamps to [0.0, 1.0] internally.
    let current_t = state.active.as_ref().map(|a| a.batch.progress());

    // Resolve the current rect for each target window.
    let from_rects: Vec<(WindowRef, Rect)> = pending
        .targets
        .iter()
        .map(|target| {
            let rect = if let (Some(t), Some(active)) = (current_t, &state.active) {
                // Prefer the interpolated position from the active batch when
                // this window is part of it (RetargetFromCurrent case).
                active
                    .batch
                    .tweens
                    .iter()
                    .find(|tw| tw.window_ref == target.window_ref)
                    // Use the *active* batch's frozen config so retarget sampling
                    // is consistent with what the frame-tick was rendering.
                    .map(|tw| AnimationBatch::interpolated_rect(tw, &active.config, t))
                    .unwrap_or_else(|| {
                        // If the window's rect can't be queried (e.g., it was destroyed or hidden),
                        // start from a zero rect. The animation still runs; the visual may be jarring.
                        backend
                            .get_window_rect(target.window_ref)
                            .unwrap_or_default()
                    })
            } else {
                // No active animation — query the backend directly.
                // If the window's rect can't be queried (e.g., it was destroyed or hidden),
                // start from a zero rect. The animation still runs; the visual may be jarring.
                backend
                    .get_window_rect(target.window_ref)
                    .unwrap_or_default()
            };
            (target.window_ref, rect)
        })
        .collect();

    let tweens = build_tweens(&pending.targets, &from_rects, &pending.config);

    log::debug!(
        "start_batch: {} targets, {} from_rects, {} tweens built, duration={:?}",
        pending.targets.len(),
        from_rects.len(),
        tweens.len(),
        pending.config.duration,
    );
    for (i, tw) in tweens.iter().enumerate() {
        log::trace!(
            "  tween[{}]: win={:?} from=({},{},{},{}) to=({},{},{},{})",
            i,
            tw.window_ref,
            tw.from_rect.x,
            tw.from_rect.y,
            tw.from_rect.w,
            tw.from_rect.h,
            tw.to_rect.x,
            tw.to_rect.y,
            tw.to_rect.w,
            tw.to_rect.h,
        );
    }

    // All tweens are no-ops — nothing to animate. This is normal during
    // continuous drag-preview (resubmitting an unchanged layout), so trace.
    if tweens.is_empty() {
        log::trace!("start_batch: all tweens are no-ops, skipping animation");
        return;
    }

    let batch = AnimationBatch::new(tweens, pending.config.duration);
    state.active = Some(ActiveBatch {
        batch,
        config: pending.config.clone(),
    });
    state.config = pending.config;
    animating.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::backend::mock::{MockBackend, MockCall, MockState};
    use crate::animation::config::AnimatorConfig;
    use crate::animation::types::{IVec2, WindowRef, WindowTarget};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Build an animator backed by a [`MockBackend`] with two pre-seeded windows.
    fn make_animator_with_mock() -> (WindowAnimator, Arc<Mutex<MockState>>) {
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let config = AnimatorConfig {
            duration: Duration::from_millis(50),
            ..AnimatorConfig::default()
        };
        let animator = WindowAnimator::new(backend, config);
        (animator, state)
    }

    /// Convenient one-window target pointing at a position different from the seeded rect.
    fn target_w1() -> Vec<WindowTarget> {
        vec![WindowTarget::new(
            WindowRef(1),
            IVec2::new(500, 500),
            IVec2::new(200, 200),
        )]
    }

    // -----------------------------------------------------------------------
    // Test 1: new animator starts idle
    // -----------------------------------------------------------------------

    #[test]
    fn animator_starts_idle() {
        let (animator, _state) = make_animator_with_mock();
        assert!(
            !animator.is_animating(),
            "new animator should not be animating"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: empty batch returns EmptyBatch error
    // -----------------------------------------------------------------------

    #[test]
    fn animate_empty_batch_returns_error() {
        let (mut animator, _state) = make_animator_with_mock();
        let result = animator.animate(vec![]);
        assert!(
            matches!(result, Err(AnimationError::EmptyBatch)),
            "expected EmptyBatch, got {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: valid batch returns an AnimationHandle
    // -----------------------------------------------------------------------

    #[test]
    fn animate_returns_handle() {
        let (mut animator, _state) = make_animator_with_mock();
        let result = animator.animate(target_w1());
        assert!(result.is_ok(), "expected Ok(handle), got {result:?}");
        let handle = result.unwrap();
        assert!(handle.0 >= 1, "handle ID should be >= 1");
    }

    // -----------------------------------------------------------------------
    // Test 4: cancel stops the animation
    // -----------------------------------------------------------------------

    #[test]
    fn cancel_stops_animation() {
        let (mut animator, _state) = make_animator_with_mock();

        animator.animate(target_w1()).unwrap();
        // Give the worker a moment to pick up the command and set the flag.
        std::thread::sleep(Duration::from_millis(10));

        animator.cancel();
        // Give the worker time to process the Cancel command.
        std::thread::sleep(Duration::from_millis(20));

        assert!(
            !animator.is_animating(),
            "animator should not be animating after cancel"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: DropNew policy — second animate() is silently accepted at the
    // API level (returns Ok) but does not replace the first animation.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Test 6 (W2 fix verification): zero-duration animation completes
    // instantly — the animator is not animating after a zero-duration
    // batch finishes. This validates that the daemon's initial snap
    // (constructed with Duration::ZERO) does not leave the animator
    // in an "animating" state.
    // -----------------------------------------------------------------------

    #[test]
    fn zero_duration_animation_completes_instantly() {
        // Arrange: animator with Duration::ZERO snap config.
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let snap_config = AnimatorConfig {
            duration: Duration::ZERO,
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, snap_config);

        // Act: submit an animation batch with the zero-duration config.
        let targets = vec![WindowTarget::new(
            WindowRef(1),
            IVec2::new(500, 500),
            IVec2::new(200, 200),
        )];
        animator.animate(targets).unwrap();

        // Allow the worker thread a small amount of time to process the
        // command. With zero duration, progress() returns 1.0 immediately
        // so the worker should finish in one tick.
        std::thread::sleep(Duration::from_millis(20));

        // Assert: animator should NOT be animating — zero duration means
        // the batch completed on the very first tick.
        assert!(
            !animator.is_animating(),
            "animator with zero-duration config should complete instantly \
             and not be animating after a short delay"
        );

        // Assert: the backend received an ApplyBatch call (windows were moved).
        let locked = state.lock().unwrap();
        let batch_calls: Vec<_> = locked
            .calls
            .iter()
            .filter(|c| matches!(c, MockCall::ApplyBatch(_)))
            .collect();
        assert!(
            !batch_calls.is_empty(),
            "zero-duration animation should still have applied at least one batch"
        );
    }

    // -----------------------------------------------------------------------
    // Test 7 (W3 fix verification): after update_config switches from
    // zero-duration to a non-zero duration, subsequent animations run at
    // the new (non-zero) duration and actually animate (is_animating true).
    // -----------------------------------------------------------------------

    #[test]
    fn update_config_switches_to_runtime_duration() {
        // Arrange: animator starts with zero-duration (initial snap config),
        // simulating the daemon's construction-time behaviour.
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let snap_config = AnimatorConfig {
            duration: Duration::ZERO,
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, snap_config);

        // Act: initial snap (instant, zero-duration) — same as daemon init step 8.
        let snap_targets = vec![WindowTarget::new(
            WindowRef(1),
            IVec2::new(500, 500),
            IVec2::new(200, 200),
        )];
        animator.animate(snap_targets).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !animator.is_animating(),
            "after zero-duration snap, animator should be idle"
        );

        // Act (W3): switch to runtime config — 250ms animation enabled.
        // This mirrors daemon init step 9: derive_animator_config + update_config.
        let runtime_config = AnimatorConfig {
            duration: Duration::from_millis(250),
            ..AnimatorConfig::default()
        };
        animator.update_config(runtime_config);

        // Act: trigger a runtime layout change that should animate.
        let runtime_targets = vec![WindowTarget::new(
            WindowRef(1),
            IVec2::new(0, 0),
            IVec2::new(960, 1080),
        )];
        animator.animate(runtime_targets).unwrap();

        // Give the worker time to pick up the command.
        std::thread::sleep(Duration::from_millis(15));

        // Assert: animator IS animating — the runtime config has 250ms duration.
        assert!(
            animator.is_animating(),
            "after update_config to 250ms, subsequent animate should be in-progress \
             (not instant)"
        );

        // Cancel to clean up (avoid worker spinning for 250ms).
        animator.cancel();
    }

    // -----------------------------------------------------------------------
    // Test 8: update_config does not affect an animation already in flight.
    // The currently-active batch keeps its original config.
    // -----------------------------------------------------------------------

    #[test]
    fn update_config_does_not_affect_inflight_animation() {
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let config = AnimatorConfig {
            duration: Duration::from_millis(200),
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, config);

        // Start a long animation.
        animator.animate(target_w1()).unwrap();
        std::thread::sleep(Duration::from_millis(15));
        assert!(animator.is_animating());

        // Update config mid-flight — should NOT affect the running batch.
        animator.update_config(AnimatorConfig {
            duration: Duration::ZERO,
            ..AnimatorConfig::default()
        });

        // The animation should still be running (original 200ms config).
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            animator.is_animating(),
            "changing config mid-flight should not cancel the active animation"
        );

        animator.cancel();
    }

    // -----------------------------------------------------------------------
    // Test 5: DropNew policy — second animate() is silently accepted at the
    // API level (returns Ok) but does not replace the first animation.
    // -----------------------------------------------------------------------

    #[test]
    fn drop_new_policy_discards_second_animate() {
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let config = AnimatorConfig {
            duration: Duration::from_millis(200), // long enough to stay active
            interrupt_policy: InterruptPolicy::DropNew,
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, config);

        // First animate — should start.
        let h1 = animator
            .animate(target_w1())
            .expect("first animate should succeed");

        // Brief pause so the worker processes the first command and sets animating=true.
        std::thread::sleep(Duration::from_millis(15));
        assert!(
            animator.is_animating(),
            "should be animating after first call"
        );

        // Second animate — API returns Ok (the request is accepted into the channel),
        // but the worker will silently drop it.
        let second_targets = vec![WindowTarget::new(
            WindowRef(2),
            IVec2::new(0, 0),
            IVec2::new(50, 50),
        )];
        let h2 = animator
            .animate(second_targets)
            .expect("second animate should return Ok");

        // The handles must be distinct.
        assert_ne!(h1.0, h2.0, "handles should have distinct IDs");

        // Animation should still be running (not interrupted by the second request).
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            animator.is_animating(),
            "animation should still be running under DropNew policy"
        );
    }
}
