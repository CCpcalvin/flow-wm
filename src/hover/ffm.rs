//! Pure focus-follows-mouse (FFM) target eligibility predicate.
//!
//! The Win32-coupled resolver in `daemon::hover` walks `WindowFromPoint` to its
//! top-level ancestor, looks the window up in the registry, queries the OS
//! foreground (and resolves the foreground's root owner via
//! `GetAncestor(GA_ROOTOWNER)`), and resolves which workspace the window calls
//! home. Those are the only OS lookups the resolver performs. **Every
//! eligibility rule** — "managed, on the active workspace, not owning the
//! foreground, not already the foreground" — lives here, in a pure, hermetic
//! leaf function the wiring consults. This mirrors the existing
//! pure-fn-called-by-wiring pattern established by
//! [`edge_band_direction`](super::edge_band_direction): the wiring gathers OS
//! truth, the pure leaf owns the rule, and the rule is a unit test with no
//! daemon construction.
//!
//! # The active-workspace clause
//!
//! The load-bearing clause is
//! [`on_active_workspace`](FfmCandidate::on_active_workspace): a window that is
//! not on the active workspace is **never** an eligible FFM target. This is an
//! invariant, not a timing trick — it holds even when animation is disabled in
//! config (instant switch ⇒ old window gone immediately ⇒ no fight). The flag
//! is resolved by the wiring against the **active monitor's** active workspace
//! (matching the existing `on_focus_changed` lookup): a window on a non-active
//! monitor resolves to no home and is ineligible, so FFM never crosses
//! monitors. See `docs/adr/0009-ffm-active-workspace-and-animation-suppression.md`
//! for the full rationale.
//!
//! # The owner-chain clause
//!
//! The owner-chain aware clause [`owns_foreground`](FfmCandidate::owns_foreground)
//! shields an **owned popup** (a top-level window owned by a tracked window but
//! un-tracked itself — e.g. Chrome's download-history panel) from being
//! dismissed by FFM: when one of a managed window's owned transients holds the
//! OS foreground, the managed owner is **not** an eligible FFM target, so the
//! 25 ms dwell never re-focuses it (which would strip activation from the
//! popup and dismiss it). The flag is `true` exactly when
//! `GetAncestor(GA_ROOTOWNER)` of the OS foreground equals the candidate
//! window; it agrees with the literal-foreground clause whenever the
//! foreground is the candidate itself (a window is its own root owner) and
//! additionally covers the un-tracked owned-transient case the literal clause
//! misses. See `docs/adr/0010-ffm-owner-chain-aware-eligibility.md` for the
//! full rationale and the accepted behavioral boundaries.
//!
//! [`WindowState`]: crate::registry::types::WindowState

use crate::registry::types::WindowState;

/// Pure, resolver-supplied snapshot of one window under the cursor.
///
/// Built by the Win32-coupled resolver after all OS lookups are done; the
/// predicate below consumes it with no Win32 access. Each field maps 1:1 to a
/// single eligibility clause so the unit tests can pin each rule independently.
#[derive(Debug, Clone, Copy)]
pub struct FfmCandidate<'a> {
    /// The tracked window's lifecycle state, or `None` when the window under
    /// the cursor is untracked (taskbar, desktop, foreign app, or no window at
    /// all).
    ///
    /// `None` is always ineligible; the predicate never targets an untracked
    /// window.
    pub state: Option<&'a WindowState>,
    /// Whether the window's home workspace is the monitor's active workspace.
    ///
    /// Always `false` when [`state`](Self::state) is `None` (an untracked
    /// window has no home workspace). A tracked window on a non-active
    /// workspace is ineligible: FFM must never focus a foreign-workspace
    /// window, even one briefly visible during a workspace-switch slide.
    pub on_active_workspace: bool,
    /// Whether the window is the current OS foreground.
    ///
    /// Focusing the current foreground is a no-op and would let the cursor's
    /// window re-arm a dwell that fires pointlessly, so the foreground is
    /// always ineligible.
    pub is_foreground: bool,
    /// Whether the candidate is the **root owner** of the current OS
    /// foreground — i.e. `GetAncestor(GA_ROOTOWNER)` of the foreground equals
    /// this candidate.
    ///
    /// When an **owned popup** (a top-level window owned by a tracked window
    /// but un-tracked itself — e.g. Chrome's download-history panel) holds the
    /// foreground, focusing its owner strips activation from the popup and
    /// dismisses it. Marking the owner ineligible while one of its owned
    /// transients is foreground lets the popup survive a pointer sweep. This
    /// clause agrees with [`is_foreground`](Self::is_foreground) when the
    /// foreground is the candidate itself (a window is its own root owner) and
    /// additionally covers the un-tracked owned-transient case the literal
    /// clause cannot see. See
    /// `docs/adr/0010-ffm-owner-chain-aware-eligibility.md`.
    pub owns_foreground: bool,
}

/// Pure FFM target eligibility predicate.
///
/// Returns `true` only when the candidate is a **managed** window — tracked
/// and in the [`Tiling`](WindowState::Tiling) or
/// [`Floating`](WindowState::Floating) state (ignored / maximized / fullscreen
/// windows are tracked but excluded) — that lives on the **active workspace**,
/// does **not own the current foreground** (no owned popup is up), and is
/// **not already the foreground**. Returns `false` otherwise.
///
/// This owns every eligibility rule; the resolver delegates to it. The
/// predicate touches no Win32, no daemon, no layout — it is a pure function
/// over a small data snapshot, so every clause below is a hermetic unit test.
///
/// # Examples
///
/// ```
/// # use flow_wm::hover::{ffm_target_eligible, FfmCandidate};
/// # use flow_wm::registry::types::{WindowState, TilingState};
/// let managed = WindowState::Tiling(TilingState::Active { col: 0, row: 0 });
/// // Managed, on the active workspace, not owning the foreground, not
/// // foreground → eligible.
/// assert!(ffm_target_eligible(FfmCandidate {
///     state: Some(&managed),
///     on_active_workspace: true,
///     is_foreground: false,
///     owns_foreground: false,
/// }));
/// // Same window on a non-active workspace → not eligible.
/// assert!(!ffm_target_eligible(FfmCandidate {
///     state: Some(&managed),
///     on_active_workspace: false,
///     is_foreground: false,
///     owns_foreground: false,
/// }));
/// ```
#[must_use]
pub fn ffm_target_eligible(candidate: FfmCandidate<'_>) -> bool {
    // Clause 1 — tracked: an untracked window (taskbar, desktop, foreign app,
    // or no window at all) is never an FFM target.
    let Some(state) = candidate.state else {
        return false;
    };
    // Clause 2 — managed: ignored windows (maximized / fullscreen / explicit
    // rule) are tracked but excluded from FFM. Minimized/hidden tiled or
    // floating windows still match here; they cannot be under the cursor, so
    // the broad state match is safe (mirrors the pre-extraction behaviour).
    if !matches!(state, WindowState::Tiling(_) | WindowState::Floating(_)) {
        return false;
    }
    // Clause 3 — active workspace: a window that is not on the active
    // workspace is never an FFM target. This is the load-bearing clause: it
    // defeats the workspace-switch flicker at the eligibility layer, holding
    // even with animation disabled. See
    // `docs/adr/0009-ffm-active-workspace-and-animation-suppression.md`.
    if !candidate.on_active_workspace {
        return false;
    }
    // Clause 4 — owner-chain aware: a candidate that owns the current
    // foreground (i.e. one of its owned popups is up) is not an FFM target.
    // Re-focusing the owner would strip activation from the popup and dismiss
    // it. This clause extends the literal-foreground clause below — the two
    // compose without a gap: when the foreground is the candidate itself the
    // two agree (a window is its own root owner); this clause additionally
    // covers the un-tracked owned-transient case the literal clause cannot
    // see. The literal-foreground clause is retained as load-bearing for the
    // fail-open / OS-quirk path. See
    // `docs/adr/0010-ffm-owner-chain-aware-eligibility.md`.
    if candidate.owns_foreground {
        return false;
    }
    // Clause 5 — not foreground: focusing the current foreground is a no-op
    // and would re-arm a dwell that fires pointlessly.
    if candidate.is_foreground {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::types::{FloatingState, IgnoredReason, TilingState};

    /// A managed tiling window — the canonical eligible candidate.
    fn tiling_active() -> WindowState {
        WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
    }

    /// A managed floating window — also eligible (FFM covers floats).
    fn floating_active() -> WindowState {
        WindowState::Floating(FloatingState::Active {
            rect: crate::common::Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        })
    }

    /// An ignored (maximized) window — tracked but excluded from FFM.
    fn ignored_maximized() -> WindowState {
        WindowState::Ignored(IgnoredReason::Maximized)
    }

    // =====================================================================
    // Happy path: managed, on active workspace, not owning foreground, not
    // foreground → eligible
    // =====================================================================

    #[test]
    fn managed_tiling_on_active_workspace_not_foreground_is_eligible() {
        let state = tiling_active();
        assert!(ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn managed_floating_on_active_workspace_not_foreground_is_eligible() {
        let state = floating_active();
        assert!(ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    // =====================================================================
    // Workspace clause: a window off the active workspace is never eligible
    // (the load-bearing fix for the workspace-switch flicker)
    // =====================================================================

    #[test]
    fn managed_tiling_off_active_workspace_is_not_eligible() {
        let state = tiling_active();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: false,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn managed_floating_off_active_workspace_is_not_eligible() {
        let state = floating_active();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: false,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    // =====================================================================
    // Existing exclusions preserved
    // =====================================================================

    #[test]
    fn untracked_window_is_not_eligible() {
        // state = None covers taskbar, desktop, foreign apps, and "no window".
        assert!(!ffm_target_eligible(FfmCandidate {
            state: None,
            on_active_workspace: false,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn untracked_is_not_eligible_even_if_workspace_flags_look_eligible() {
        // Defense in depth: an untracked window has no home workspace, so
        // on_active_workspace is meaningless. The state=None check must win
        // regardless of the other fields (the resolver always passes false
        // here, but the predicate must not rely on the caller for safety).
        assert!(!ffm_target_eligible(FfmCandidate {
            state: None,
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn ignored_maximized_window_is_not_eligible() {
        let state = ignored_maximized();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn ignored_fullscreen_window_is_not_eligible() {
        let state = WindowState::Ignored(IgnoredReason::Fullscreen);
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn ignored_explicit_rule_window_is_not_eligible() {
        let state = WindowState::Ignored(IgnoredReason::ExplicitRule);
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn ignored_window_off_active_workspace_is_not_eligible() {
        // Excluded by both the managed clause and the workspace clause —
        // either alone suffices; the predicate short-circuits on the first.
        let state = ignored_maximized();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: false,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn foreground_window_is_not_eligible() {
        let state = tiling_active();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: true,
            owns_foreground: false,
        }));
    }

    #[test]
    fn foreground_off_active_workspace_is_not_eligible() {
        // The workspace clause rejects first; the foreground clause is moot.
        let state = tiling_active();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: false,
            is_foreground: true,
            owns_foreground: false,
        }));
    }

    // =====================================================================
    // Owner-chain clause: a managed candidate that owns the current foreground
    // (one of its owned popups is up) is not eligible — the load-bearing fix
    // for the popup-dismissal bug. See ADR-0010.
    // =====================================================================

    #[test]
    fn managed_tiling_owning_foreground_is_not_eligible() {
        // A managed tiling window whose owned popup (e.g. Chrome
        // download-history) holds the foreground must not be re-focused by
        // FFM — re-focus strips activation and dismisses the popup.
        let state = tiling_active();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: true,
        }));
    }

    #[test]
    fn managed_floating_owning_foreground_is_not_eligible() {
        // The shield applies uniformly across tile and float layers (a
        // tracked floating window's owned dialogs are shielded the same way
        // tiled windows' popups are).
        let state = floating_active();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: true,
        }));
    }

    #[test]
    fn managed_not_owning_foreground_remains_eligible() {
        // Happy path preserved: with no owned popup up, the candidate is a
        // normal FFM target — the new clause does not over-suppress.
        let state = tiling_active();
        assert!(ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn untracked_owning_foreground_is_not_eligible() {
        // Defense in depth: an untracked candidate (state = None) is
        // ineligible regardless of the ownership flag — the state=None check
        // must win. (The resolver never passes owns_foreground=true with
        // state=None, but the predicate must not rely on the caller.)
        assert!(!ffm_target_eligible(FfmCandidate {
            state: None,
            on_active_workspace: false,
            is_foreground: false,
            owns_foreground: true,
        }));
    }

    #[test]
    fn owner_off_active_workspace_is_not_eligible() {
        // Composition with the workspace clause: an off-active-workspace
        // owner is ineligible by either clause; the predicate short-circuits
        // on the workspace check first.
        let state = tiling_active();
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: false,
            is_foreground: false,
            owns_foreground: true,
        }));
    }

    #[test]
    fn ownership_clause_agrees_with_literal_foreground_when_candidate_is_foreground() {
        // When the foreground is the candidate itself, GA_ROOTOWNER of the
        // foreground is the candidate, so owns_foreground must be true as
        // well. Both clauses reject; flipping only is_foreground (or only
        // owns_foreground) must still reject, so the two clauses compose
        // without a gap when the wiring happens to compute one but not the
        // other.
        let state = tiling_active();
        // Both clauses reject:
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: true,
            owns_foreground: true,
        }));
        // Literal foreground alone rejects:
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: true,
            owns_foreground: false,
        }));
        // Ownership alone rejects (the popup case the literal clause misses):
        assert!(!ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: true,
        }));
    }

    // =====================================================================
    // Minimized / hidden managed windows: still "managed" by the broad match
    // (they cannot be under the cursor, so this is safe and unchanged).
    // =====================================================================

    #[test]
    fn minimized_tiling_passes_managed_clause() {
        // The state match is deliberately broad; minimized windows cannot be
        // under the cursor, so this never matters in practice — but pin the
        // behaviour so a future narrowing of the match is a conscious change.
        let state = WindowState::Tiling(TilingState::Minimized);
        assert!(ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }

    #[test]
    fn hidden_floating_passes_managed_clause() {
        let state = WindowState::Floating(FloatingState::Hidden);
        assert!(ffm_target_eligible(FfmCandidate {
            state: Some(&state),
            on_active_workspace: true,
            is_foreground: false,
            owns_foreground: false,
        }));
    }
}
