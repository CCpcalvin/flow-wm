//! Pure TOPMOST-toggle decision for the float layer.
//!
//! Hermetic decision logic for the "floats stay above the focused tile"
//! invariant — no Win32, daemon, or layout coupling. The daemon wiring is
//! [`crate::daemon::FlowWM::reconcile_float_topmost`].
//! (`docs/src/dev-guide/floating-space.md`)

/// Stacking-relevant kind of the current foreground window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundKind {
    /// Flow-managed (actively tiled or floated) and not fullscreen.
    Flow,
    /// The foreground covers the full screen with no chrome.
    Fullscreen,
    /// A window flow does not manage (untracked, ignored, or foreign).
    NonFlow,
}

/// Classify the foreground into a [`ForegroundKind`].
///
/// `is_fullscreen` takes precedence over `is_flow_managed`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopmostAction {
    /// Pin every float `WS_EX_TOPMOST`.
    Pin,
    /// Drop every float to non-topmost.
    Drop,
    /// No Win32 calls: no floats are present, or the desired state already holds.
    NoOp,
}

/// Inputs to [`decide_float_topmost`] for one foreground change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatTopmostSnapshot {
    /// Stacking kind of the current foreground window.
    pub foreground: ForegroundKind,
    /// Whether any floats exist on the active workspace.
    pub has_floats: bool,
    /// Whether the floats are currently `WS_EX_TOPMOST` (last applied state).
    pub currently_topmost: bool,
}

// ── Re-assertion against drift (#7) ── (`docs/src/dev-guide/floating-space.md`)

/// One float's observed `WS_EX_TOPMOST` state for a re-assertion pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatObserved {
    /// Opaque float identity (the HWND value).
    pub id: isize,
    /// The live `WS_EX_TOPMOST` reading for this float.
    pub observed_topmost: bool,
}

/// Per-float re-assertion verdict, output of [`decide_float_reapply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopmostReapply {
    /// The float's real flag already matches the target — no `SetWindowPos`.
    Aligned,
    /// The float lost its real TOPMOST flag — re-apply `HWND_TOPMOST`.
    Repin,
}

/// Re-apply verdict for one float under a `target_topmost`.
///
/// Returns [`TopmostReapply::Repin`] iff `target_topmost` is `true` and
/// `observed_topmost` is `false`; a non-topmost target is never re-pinned here.
/// (`docs/src/dev-guide/floating-space.md`)
#[must_use]
pub fn decide_float_reapply(target_topmost: bool, observed_topmost: bool) -> TopmostReapply {
    if target_topmost && !observed_topmost {
        TopmostReapply::Repin
    } else {
        TopmostReapply::Aligned
    }
}

/// Ids of floats whose real `WS_EX_TOPMOST` flag must be re-applied.
///
/// Empty when `target_topmost` is `false`. (`docs/src/dev-guide/floating-space.md`)
#[must_use]
pub fn reassert_float_topmost(target_topmost: bool, floats: &[FloatObserved]) -> Vec<isize> {
    floats
        .iter()
        .filter(|f| {
            decide_float_reapply(target_topmost, f.observed_topmost) == TopmostReapply::Repin
        })
        .map(|f| f.id)
        .collect()
}

/// The TOPMOST-toggle decision for one foreground change.
///
/// Returns [`TopmostAction::NoOp`] when there are no floats or the desired
/// state already holds.
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

// ── Foreground location-change trigger (#6) ── (`docs/src/dev-guide/floating-space.md`)

/// Whether a location-change event is for the foreground window (and so should
/// re-run float TOPMOST reconciliation).
///
/// Returns `false` when `foreground_hwnd` is `0`. (`docs/src/dev-guide/floating-space.md`)
#[must_use]
pub fn is_foreground_location_change(event_hwnd: isize, foreground_hwnd: isize) -> bool {
    foreground_hwnd != 0 && event_hwnd == foreground_hwnd
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
        for foreground in [
            ForegroundKind::Flow,
            ForegroundKind::Fullscreen,
            ForegroundKind::NonFlow,
        ] {
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

    // ── re-assertion (ticket #7): per-float reality check ───────────

    /// Criterion 1: a float that SHOULD be topmost but whose real flag was
    /// lost (another app grabbed topmost / an event cleared it) is flagged for
    /// re-apply. We feed the OBSERVED flag — never the cache — and assert the
    /// DECISION, not any Win32 call.
    #[test]
    fn decide_reapply_flags_topmost_target_float_that_lost_its_flag() {
        assert_eq!(
            decide_float_reapply(true, false),
            TopmostReapply::Repin,
            "a topmost-target float whose real flag reads non-topmost must be re-pinned"
        );
    }

    /// Criterion 1 (no-drift complement): a topmost-target float whose real flag
    /// is still set is already aligned — no re-apply, no churn.
    #[test]
    fn decide_reapply_aligned_when_topmost_target_float_keeps_its_flag() {
        assert_eq!(
            decide_float_reapply(true, true),
            TopmostReapply::Aligned,
            "a topmost-target float whose real flag reads topmost needs no re-apply"
        );
    }

    /// Criterion 2: a non-topmost-target float is NEVER forced back up by
    /// re-assertion, no matter its observed state — dropping is the Drop
    /// path's job, and re-assertion must not undo it.
    #[test]
    fn decide_reapply_never_pins_non_topmost_target_float() {
        // Observed still topmost (stale) but target dropped → leave it for the
        // Drop path, never re-pin.
        assert_eq!(decide_float_reapply(false, true), TopmostReapply::Aligned);
        // Observed already dropped → nothing to do.
        assert_eq!(decide_float_reapply(false, false), TopmostReapply::Aligned);
    }

    /// Criterion 1 at the layer: only the drifted float is returned, while an
    /// already-aligned sibling is left alone (the no-churn guarantee survives
    /// drift detection).
    #[test]
    fn reassert_returns_only_drifted_floats_under_topmost_target() {
        let floats = [
            FloatObserved {
                id: 1,
                observed_topmost: true,
            }, // aligned
            FloatObserved {
                id: 2,
                observed_topmost: false,
            }, // drifted
            FloatObserved {
                id: 3,
                observed_topmost: true,
            }, // aligned
        ];
        assert_eq!(reassert_float_topmost(true, &floats), vec![2]);
    }

    /// Criterion 2 at the layer: under a non-topmost target the result is
    /// ALWAYS empty, even when every float's real flag reads topmost.
    #[test]
    fn reassert_never_returns_floats_under_non_topmost_target() {
        let floats = [
            FloatObserved {
                id: 1,
                observed_topmost: true,
            },
            FloatObserved {
                id: 2,
                observed_topmost: false,
            },
        ];
        assert!(reassert_float_topmost(false, &floats).is_empty());
    }

    /// Cheap / common case: no drift under a topmost target does no work (the
    /// empty-result fast path the wiring relies on to avoid SetWindowPos).
    #[test]
    fn reassert_no_drift_is_empty_under_topmost_target() {
        let floats = [
            FloatObserved {
                id: 1,
                observed_topmost: true,
            },
            FloatObserved {
                id: 2,
                observed_topmost: true,
            },
        ];
        assert!(reassert_float_topmost(true, &floats).is_empty());
    }

    /// Criterion 3 (no busy-loop): re-assertion converges in a single pass.
    /// After the wiring re-applies the drifted floats, re-reading their observed
    /// state yields `true`, so the next decision is empty — re-applying does not
    /// itself keep triggering further work. Modelled as the drift → re-apply →
    /// re-evaluate sequence.
    #[test]
    fn reassert_converges_in_one_pass_no_busy_loop() {
        // Pass 1: one float drifted, the other aligned → exactly the drifted id.
        let before = [
            FloatObserved {
                id: 1,
                observed_topmost: false,
            },
            FloatObserved {
                id: 2,
                observed_topmost: true,
            },
        ];
        assert_eq!(reassert_float_topmost(true, &before), vec![1]);

        // Pass 2: the re-apply from pass 1 set float 1's real flag; re-reading
        // reality now shows every float aligned → no further work. This is the
        // idempotent convergence that prevents a busy-loop.
        let after = [
            FloatObserved {
                id: 1,
                observed_topmost: true,
            },
            FloatObserved {
                id: 2,
                observed_topmost: true,
            },
        ];
        assert!(reassert_float_topmost(true, &after).is_empty());
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

    // ── Foreground location-change trigger (ticket #6) ────────────────
    //
    // The F11 trigger's pure seam: the relevance filter (which location change
    // to re-evaluate) plus the transition sequences it must catch and the
    // no-op it must NOT. All via the existing `classify_foreground` +
    // `decide_float_topmost`, asserted on the DECISION, never any Win32 call.

    /// The trigger re-evaluates only on the foreground window's own location
    /// change; the foreground's own HWND is the relevant one.
    #[test]
    fn foreground_location_change_is_relevant_for_self() {
        assert!(is_foreground_location_change(100, 100));
    }

    /// A location change for any window that is NOT the foreground is
    /// irrelevant — the noisy `EVENT_OBJECT_LOCATIONCHANGE` stream is filtered
    /// down to at most the one foreground HWND.
    #[test]
    fn non_foreground_location_change_is_irrelevant() {
        assert!(!is_foreground_location_change(100, 200));
        assert!(!is_foreground_location_change(200, 100));
    }

    /// Before the first foreground is established (`foreground_hwnd == 0`)
    /// every location change is rejected, so none is misrouted into the toggle
    /// at startup.
    #[test]
    fn location_change_irrelevant_when_no_foreground_known() {
        assert!(!is_foreground_location_change(0, 0));
        assert!(!is_foreground_location_change(100, 0));
    }

    /// Criterion 1: pressing F11 in an already-focused flow-managed app flips
    /// its live fullscreen classification `Flow → Fullscreen`, so the
    /// foreground location change re-evaluates the toggle and DROPS the floats
    /// below it (no focus change needed). The fullscreen fact is the live
    /// geometry+style classification, not the stored registry state.
    #[test]
    fn f11_fullscreen_in_focused_flow_app_drops_floats() {
        // Focused app starts windowed and flow-managed → floats pinned.
        assert_eq!(
            decide_float_topmost(FloatTopmostSnapshot {
                foreground: classify_foreground(true, false), // Flow
                has_floats: true,
                currently_topmost: false,
            }),
            TopmostAction::Pin,
        );
        // Floats are now topmost. F11 makes the focused app fullscreen → the
        // foreground location change re-runs the toggle with the new (live)
        // classification → drop.
        assert_eq!(
            decide_float_topmost(FloatTopmostSnapshot {
                foreground: classify_foreground(true, true), // Fullscreen
                has_floats: true,
                currently_topmost: true,
            }),
            TopmostAction::Drop,
        );
    }

    /// Criterion 2: exiting F11 returns a flow-managed app to windowed, so the
    /// foreground location change re-evaluates and re-PINS the floats.
    #[test]
    fn exiting_f11_in_flow_app_re_pins_floats() {
        // Floats dropped while the focused app is fullscreen.
        assert_eq!(
            decide_float_topmost(FloatTopmostSnapshot {
                foreground: classify_foreground(true, true), // Fullscreen
                has_floats: true,
                currently_topmost: true,
            }),
            TopmostAction::Drop,
        );
        // Floats now dropped. Exit F11 → app windowed again → the foreground
        // location change re-runs the toggle → re-pin.
        assert_eq!(
            decide_float_topmost(FloatTopmostSnapshot {
                foreground: classify_foreground(true, false), // Flow
                has_floats: true,
                currently_topmost: false,
            }),
            TopmostAction::Pin,
        );
    }

    /// Criterion 3: an ordinary move/resize of the foreground that is NOT a
    /// fullscreen transition leaves the classification unchanged, so
    /// re-evaluating the toggle is a `NoOp` — no spurious `SetWindowPos`, and
    /// no busy-loop across the per-pixel location-change stream during a drag.
    #[test]
    fn ordinary_foreground_resize_does_not_toggle_floats() {
        // Flow-managed app, windowed, floats already on top. A location change
        // fires (the user resizes within the screen) but it stays windowed and
        // flow-managed → same classification → no-op.
        assert_eq!(
            decide_float_topmost(FloatTopmostSnapshot {
                foreground: classify_foreground(true, false), // Flow, unchanged
                has_floats: true,
                currently_topmost: true,
            }),
            TopmostAction::NoOp,
        );
        // Same holds while floats are dropped under a non-flow foreground that
        // resizes: stays NonFlow → no-op.
        assert_eq!(
            decide_float_topmost(FloatTopmostSnapshot {
                foreground: classify_foreground(false, false), // NonFlow, unchanged
                has_floats: true,
                currently_topmost: false,
            }),
            TopmostAction::NoOp,
        );
    }
}
