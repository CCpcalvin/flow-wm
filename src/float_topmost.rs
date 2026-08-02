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

// ── Re-assertion against drift (ticket #7) ─────────────────────────
//
// `WS_EX_TOPMOST` is not set-and-forget: other apps can grab topmost and
// various events can clear the flag while the wiring's cached `floats_topmost`
// still believes it is set. The toggle above only compares the target against
// that cache, so drift on the TOPMOST-target path slips through silently. The
// functions below re-assert against OBSERVED reality instead: read each float's
// live flag and re-apply only what was actually lost. Pure over its inputs; the
// Win32 read + conditional re-apply live in the thin `daemon::float_topmost`
// shell. (`docs/src/dev-guide/floating-space.md`)

/// One float's observed reality for a re-assertion pass.
///
/// Pairs the float's opaque identity (its HWND value) with the **live**
/// `WS_EX_TOPMOST` reading taken from Win32 — never the cache. The layer-level
/// [`FloatTopmostSnapshot`] is not enough here, because drift can clear one
/// float's real flag while leaving a sibling's set, so the re-assertion decision
/// is per-float.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatObserved {
    /// Opaque float identity — the HWND value, carried through unchanged so the
    /// wiring can act on exactly the floats the decision flags with no re-lookup.
    pub id: isize,
    /// The live `GetWindowLongW(GWL_EXSTYLE) & WS_EX_TOPMOST` reading — reality,
    /// not the cached last-applied state.
    pub observed_topmost: bool,
}

/// Per-float re-assertion verdict.
///
/// Output of [`decide_float_reapply`] for a single float under a TOPMOST target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopmostReapply {
    /// The float's real flag already matches the target — no `SetWindowPos`.
    Aligned,
    /// The float lost its real TOPMOST flag while its target is topmost —
    /// re-apply `HWND_TOPMOST`.
    Repin,
}

/// Decide whether a single float needs its real `WS_EX_TOPMOST` flag
/// re-applied.
///
/// The re-assertion core rule: verify reality, never the cache. A float is
/// re-pinned **iff** its target is TOPMOST and its **observed** real flag reads
/// non-topmost (drift). A float whose target is non-topmost is **never**
/// re-pinned here, regardless of its observed state — dropping is the job of the
/// [`TopmostAction::Drop`] path, and re-assertion must never force a
/// non-topmost-target float back up.
///
/// Pure and side-effect-free. Idempotent: after the wiring re-applies the flag
/// the float's observed state reads `true`, so re-running this returns
/// [`TopmostReapply::Aligned`] — no further work. That is the single-pass
/// convergence the no-busy-loop guarantee rests on (see
/// [`reassert_float_topmost`]).
///
/// (`docs/src/dev-guide/floating-space.md`)
#[must_use]
pub fn decide_float_reapply(target_topmost: bool, observed_topmost: bool) -> TopmostReapply {
    if target_topmost && !observed_topmost {
        TopmostReapply::Repin
    } else {
        TopmostReapply::Aligned
    }
}

/// Run the per-float re-assertion decision across the whole float layer.
///
/// Returns the identities of the floats whose real `WS_EX_TOPMOST` flag must be
/// re-applied. Pure over its inputs; the wiring translates each returned id into
/// one `SetWindowPos(HWND_TOPMOST, … | NOACTIVATE)`.
///
/// Only floats whose **target is TOPMOST** are ever returned: when
/// `target_topmost` is `false` the result is always empty — dropping is owned by
/// the [`TopmostAction::Drop`] path, and re-assertion never forces a
/// non-topmost-target float back up.
///
/// Converges in one pass: once the wiring has re-applied the returned flags,
/// every float's observed state equals the target, so the next call returns an
/// empty list — no repeated toggling, no busy-loop.
///
/// (`docs/src/dev-guide/floating-space.md`)
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
/// Pure: no Win32, no side effects. Returns [`TopmostAction::NoOp`] when there
/// are no floats or the desired state already holds, so the wiring performs no
/// `SetWindowPos` churn in those cases.
///
/// Note: the `NoOp` for the TOPMOST-target case compares only against the cached
/// state and so cannot see drift. The wiring therefore re-asserts against
/// observed reality on that path (see [`reassert_float_topmost`]).
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
}
