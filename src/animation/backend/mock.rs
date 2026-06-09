//! In-process mock backend for unit testing the animation engine.
//!
//! [`MockBackend`] records every call made through the [`WindowBackend`] trait
//! so tests can assert on call sequences and inject controlled return values
//! without touching real Win32 windows.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crate::animation::{
    backend::WindowBackend,
    types::{AnimationError, Rect, Result, WindowRef},
};

/// A record of one call made to a [`MockBackend`].
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // Used by tests; not yet consumed by other crate modules.
pub enum MockCall {
    /// [`WindowBackend::get_window_rect`] was called for the given window.
    GetWindowRect(WindowRef),
    /// [`WindowBackend::apply_batch`] was called with the given updates.
    ApplyBatch(Vec<(WindowRef, Rect)>),
    /// [`WindowBackend::dwm_flush`] was called.
    DwmFlush,
}

/// Shared mutable state for a [`MockBackend`].
///
/// Tests hold an `Arc<Mutex<MockState>>` (obtained via
/// [`MockBackend::with_state`] or [`MockBackend::state`]) and inspect it after
/// driving the animator.
#[derive(Debug, Default)]
#[allow(dead_code)] // Used by tests; not yet consumed by other crate modules.
pub struct MockState {
    /// Ordered list of every call recorded by the backend.
    pub calls: Vec<MockCall>,

    /// Pre-seeded window rectangles returned by [`WindowBackend::get_window_rect`].
    pub rects: HashMap<WindowRef, Rect>,

    /// When `Some`, [`WindowBackend::apply_batch`] returns this error.
    /// When `None`, `apply_batch` returns `Ok(())`.
    pub apply_batch_result: Option<AnimationError>,

    /// When non-zero, [`WindowBackend::dwm_flush`] sleeps for this many
    /// milliseconds before returning, simulating DWM composition latency.
    pub dwm_flush_delay_ms: u64,
}

/// A [`WindowBackend`] implementation suitable for unit tests.
///
/// All calls are recorded in [`MockState::calls`] and return values are
/// controlled via the fields of [`MockState`].
///
/// # Example
///
/// ```rust,ignore
/// // The mock backend is `pub(crate)` — used in tests within the animation module.
/// ```
#[allow(dead_code)] // Used by tests; not yet consumed by other crate modules.
pub struct MockBackend {
    state: Arc<Mutex<MockState>>,
}

#[allow(dead_code)] // Used by tests; not yet consumed by other crate modules.
impl MockBackend {
    /// Create a [`MockBackend`] with default (empty) state.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState::default())),
        }
    }

    /// Create a [`MockBackend`] that shares the given `state` with the caller.
    ///
    /// The test thread retains its `Arc` clone and can inspect or mutate
    /// `MockState` between calls.
    pub fn with_state(state: Arc<Mutex<MockState>>) -> Self {
        Self { state }
    }

    /// Return a clone of the `Arc` pointing at this backend's shared state.
    pub fn state(&self) -> Arc<Mutex<MockState>> {
        Arc::clone(&self.state)
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowBackend for MockBackend {
    /// Look up `window` in [`MockState::rects`] and record the call.
    ///
    /// Returns [`AnimationError::Backend`] if the window is not present in the
    /// pre-seeded rect map.
    fn get_window_rect(&self, window: WindowRef) -> Result<Rect> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AnimationError::Backend("mock state mutex poisoned".to_string()))?;

        state.calls.push(MockCall::GetWindowRect(window));

        state
            .rects
            .get(&window)
            .copied()
            .ok_or_else(|| AnimationError::Backend("window not found".to_string()))
    }

    /// Record the batch and return the configured result.
    ///
    /// Pushes [`MockCall::ApplyBatch`] with a copy of all `(WindowRef, Rect)`
    /// pairs, then returns `Ok(())` unless [`MockState::apply_batch_result`]
    /// is `Some(error)`.
    fn apply_batch(&self, updates: &[(WindowRef, Rect)]) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AnimationError::Backend("mock state mutex poisoned".to_string()))?;

        state.calls.push(MockCall::ApplyBatch(updates.to_vec()));

        match &state.apply_batch_result {
            None => Ok(()),
            Some(e) => Err(e.clone()),
        }
    }

    /// Record the call and optionally sleep to simulate DWM latency.
    ///
    /// When [`MockState::dwm_flush_delay_ms`] is non-zero the thread sleeps for
    /// that duration, mimicking the blocking behaviour of the real `DwmFlush`.
    fn dwm_flush(&self) -> Result<()> {
        let delay_ms = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| AnimationError::Backend("mock state mutex poisoned".to_string()))?;
            state.calls.push(MockCall::DwmFlush);
            state.dwm_flush_delay_ms
        };

        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_backend_has_empty_calls() {
        let backend = MockBackend::new();
        let state = backend.state.lock().unwrap();
        assert!(state.calls.is_empty());
    }

    #[test]
    fn get_window_rect_records_call_for_known_window() {
        let state = Arc::new(Mutex::new(MockState {
            rects: [(WindowRef(10), Rect::new(0, 0, 100, 200))].into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));

        let rect = backend.get_window_rect(WindowRef(10)).unwrap();
        assert_eq!(rect, Rect::new(0, 0, 100, 200));

        let locked = state.lock().unwrap();
        assert_eq!(locked.calls, vec![MockCall::GetWindowRect(WindowRef(10))]);
    }

    #[test]
    fn get_window_rect_returns_err_for_unknown_window() {
        let backend = MockBackend::new();
        let result = backend.get_window_rect(WindowRef(99));
        assert!(matches!(result, Err(AnimationError::Backend(_))));
    }

    #[test]
    fn apply_batch_records_all_pairs() {
        let backend = MockBackend::new();
        let updates = vec![
            (WindowRef(1), Rect::new(0, 0, 800, 600)),
            (WindowRef(2), Rect::new(800, 0, 800, 600)),
        ];
        backend.apply_batch(&updates).unwrap();

        let state = backend.state.lock().unwrap();
        assert_eq!(state.calls, vec![MockCall::ApplyBatch(updates)]);
    }

    #[test]
    fn dwm_flush_records_the_call() {
        let backend = MockBackend::new();
        backend.dwm_flush().unwrap();

        let state = backend.state.lock().unwrap();
        assert_eq!(state.calls, vec![MockCall::DwmFlush]);
    }
}
