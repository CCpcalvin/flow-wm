//! Pure TOPMOST-toggle decision for the float layer.
//!
//! This is the test seam for the "floats stay above the focused tile" feature.
//! It is pure logic: given the foreground window's stacking kind and the float
//! layer's current state, it decides whether every float must be pinned
//! `WS_EX_TOPMOST` or dropped to non-topmost. It touches no Win32, the daemon,
//! or the layout engine — so every rule below is a hermetic unit test, mirroring
//! the codebase precedent of [`crate::hover::HoverController`] and
//! [`crate::workspace::FloatingSpace`].
//!
//! The contract: a float is kept on top **iff the foreground is a flow-managed
//! window that is not fullscreen**; otherwise the floats are dropped so they do
//! not cover a fullscreen app or another application's windows. The wiring
//! ([`crate::daemon::FlowWM::reconcile_float_topmost`]) evaluates this in the
//! single focus sink on every foreground change. See
//! (`docs/src/dev-guide/floating-space.md`).

/// Stacking-relevant kind of the current foreground window.
///
/// Reduces the foreground to exactly what the TOPMOST decision needs: is it a
/// window flow is actively managing and that is not fullscreen?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundKind {
    /// Flow-managed (actively tiled or floated) and not fullscreen.
    ///
    /// The floats stay on top of this foreground.
    Flow,
    /// The foreground covers the full screen with no chrome.
    ///
    /// Detected live (geometry + style) on the foreground window — fullscreen
    /// video, a borderless game, or a flow window the user sent fullscreen.
    /// Floats drop below it so they never cover fullscreen content.
    Fullscreen,
    /// A window flow does not manage (untracked, ignored, or foreign).
    ///
    /// Floats drop so they do not cover another application's windows.
    NonFlow,
}

/// Classify the live foreground window into a [`ForegroundKind`].
///
/// `is_flow_managed` is "actively tiling or floating" (a tracked window that is
/// NOT ignored); `is_fullscreen` is the **live** geometry+style check on the
/// foreground, never the stored registry classification. Fullscreen takes
/// precedence: a flow-managed window that is currently fullscreen still drops
/// the floats.
#[must_use]
pub fn classify_foreground(is_flow_managed: bool, is_fullscreen: bool) -> ForegroundKind {
    if is_fullscreen {
        ForegroundKind::Fullscreen
    } else if is_flow_managed {
        ForegroundKind::Flow
    } else {
        ForegroundKind::NonFlow
    }
}

/// The TOPMOST-toggle action to apply to every float for one foreground change.
///
/// The wiring translates each variant into `SetWindowPos(HWND_TOPMOST …)` /
/// `SetWindowPos(HWND_NOTOPMOST …)` calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopmostAction {
    /// Pin every float `WS_EX_TOPMOST`.
    Pin,
    /// Drop every float to non-topmost.
    Drop,
    /// No Win32 calls: either no floats are present, or the desired state
    /// already holds.
    NoOp,
}

/// A snapshot of the foreground and the float layer taken at one foreground
/// change.
///
/// All inputs the decision needs, with no Win32 types so it is hermetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatTopmostSnapshot {
    /// Stacking kind of the current foreground window.
    pub foreground: ForegroundKind,
    /// Whether any floats exist on the active workspace.
    pub has_floats: bool,
    /// Whether the floats are currently believed to be `WS_EX_TOPMOST`
    /// (the last state the wiring applied).
    pub currently_topmost: bool,
}

/// The TOPMOST-toggle decision for one foreground change.
///
/// Pure: no Win32, no side effects. Returns [`TopmostAction::NoOp`] when there
/// are no floats or the desired state already holds, so the wiring performs no
/// `SetWindowPos` churn in those cases.
#[must_use]
pub fn decide_float_topmost(snapshot: FloatTopmostSnapshot) -> TopmostAction {
    // No floats ⇒ nothing to toggle and no work to do (the no-overhead no-op).
    if !snapshot.has_floats {
        return TopmostAction::NoOp;
    }
    // TOPMOST iff the foreground is flow-managed and not fullscreen.
    let desired_topmost = matches!(snapshot.foreground, ForegroundKind::Flow);
    match (desired_topmost, snapshot.currently_topmost) {
        (true, false) => TopmostAction::Pin,
        (false, true) => TopmostAction::Drop,
        // Desired state already holds ⇒ no Win32 churn.
        _ => TopmostAction::NoOp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_foreground ──────────────────────────────────────────

    #[test]
    fn classify_flow_managed_non_fullscreen_is_flow() {
        assert_eq!(classify_foreground(true, false), ForegroundKind::Flow);
    }

    #[test]
    fn classify_non_flow_non_fullscreen_is_non_flow() {
        assert_eq!(classify_foreground(false, false), ForegroundKind::NonFlow);
    }

    #[test]
    fn classify_non_flow_fullscreen_is_fullscreen() {
        assert_eq!(classify_foreground(false, true), ForegroundKind::Fullscreen);
    }

    /// Criterion 6: a flow-managed window that is *live* fullscreen still
    /// drops the floats — fullscreen takes precedence over flow-managed, and
    /// the fullscreen fact is the live geometry+style check, not the stored
    /// registry classification.
    #[test]
    fn classify_fullscreen_takes_precedence_over_flow_managed() {
        assert_eq!(classify_foreground(true, true), ForegroundKind::Fullscreen);
    }

    // ── decide_float_topmost: pin / drop / no-op ─────────────────────

    /// Criterion 1 & 4: focusing a flow-managed window (a tile, via FFM /
    /// keyboard / click) pins the floats on top.
    #[test]
    fn flow_foreground_pins_floats_when_not_already_topmost() {
        let action = decide_float_topmost(FloatTopmostSnapshot {
            foreground: ForegroundKind::Flow,
            has_floats: true,
            currently_topmost: false,
        });
        assert_eq!(action, TopmostAction::Pin);
    }

    /// No redundant re-pin when the floats are already on top — avoids
    /// `SetWindowPos` churn (and float mutual z-order disturbance) on every
    /// foreground change.
    #[test]
    fn flow_foreground_already_topmost_is_noop() {
        let action = decide_float_topmost(FloatTopmostSnapshot {
            foreground: ForegroundKind::Flow,
            has_floats: true,
            currently_topmost: true,
        });
        assert_eq!(action, TopmostAction::NoOp);
    }

    /// Criterion 2: a fullscreen foreground drops the floats below it.
    #[test]
    fn fullscreen_foreground_drops_floats() {
        let action = decide_float_topmost(FloatTopmostSnapshot {
            foreground: ForegroundKind::Fullscreen,
            has_floats: true,
            currently_topmost: true,
        });
        assert_eq!(action, TopmostAction::Drop);
    }

    /// Criterion 3: a non-flow foreground (Settings, UWP, unmanaged app)
    /// drops the floats.
    #[test]
    fn non_flow_foreground_drops_floats() {
        let action = decide_float_topmost(FloatTopmostSnapshot {
            foreground: ForegroundKind::NonFlow,
            has_floats: true,
            currently_topmost: true,
        });
        assert_eq!(action, TopmostAction::Drop);
    }

    /// Already-dropped floats stay dropped under a dropping foreground — no
    /// redundant `SetWindowPos` churn.
    #[test]
    fn dropping_foreground_already_dropped_is_noop() {
        assert_eq!(
            decide_float_topmost(FloatTopmostSnapshot {
                foreground: ForegroundKind::Fullscreen,
                has_floats: true,
                currently_topmost: false,
            }),
            TopmostAction::NoOp
        );
        assert_eq!(
            decide_float_topmost(FloatTopmostSnapshot {
                foreground: ForegroundKind::NonFlow,
                has_floats: true,
                currently_topmost: false,
            }),
            TopmostAction::NoOp
        );
    }

    /// Criterion 5: with no floats present the decision is always a no-op,
    /// regardless of the foreground kind.
    #[test]
    fn no_floats_is_noop_for_every_foreground_kind() {
        for foreground in [ForegroundKind::Flow, ForegroundKind::Fullscreen, ForegroundKind::NonFlow]
        {
            assert_eq!(
                decide_float_topmost(FloatTopmostSnapshot {
                    foreground,
                    has_floats: false,
                    currently_topmost: false,
                }),
                TopmostAction::NoOp
            );
            assert_eq!(
                decide_float_topmost(FloatTopmostSnapshot {
                    foreground,
                    has_floats: false,
                    currently_topmost: true,
                }),
                TopmostAction::NoOp
            );
        }
    }

    /// Criterion 4 as a transition sequence: focus a flow tile (pin) → focus a
    /// fullscreen app (drop) → focus the flow tile again (re-pin).
    #[test]
    fn drops_then_re_pins_when_flow_focus_returns() {
        // Start: floats not topmost, focus moves to a flow tile → pin.
        let pin = decide_float_topmost(FloatTopmostSnapshot {
            foreground: ForegroundKind::Flow,
            has_floats: true,
            currently_topmost: false,
        });
        assert_eq!(pin, TopmostAction::Pin);

        // Floats are now topmost; focus moves to a fullscreen app → drop.
        let drop = decide_float_topmost(FloatTopmostSnapshot {
            foreground: ForegroundKind::Fullscreen,
            has_floats: true,
            currently_topmost: true,
        });
        assert_eq!(drop, TopmostAction::Drop);

        // Floats are now dropped; focus returns to the flow tile → re-pin.
        let re_pin = decide_float_topmost(FloatTopmostSnapshot {
            foreground: ForegroundKind::Flow,
            has_floats: true,
            currently_topmost: false,
        });
        assert_eq!(re_pin, TopmostAction::Pin);
    }
}
