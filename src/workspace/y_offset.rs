//! Vertical parking offset for workspace stacking.
//!
//! Workspaces are stacked **vertically** the same way columns are stacked
//! horizontally inside a [`ScrollingSpace`](super::ScrollingSpace): the active
//! workspace sits at `y = 0`, previous workspaces (id < active) park above at
//! `-y_unit`, and next workspaces (id > active) park below at `+y_unit`, where
//! `y_unit = monitor_height + window_gap`.
//!
//! # The ±1-Unit Model
//!
//! The offset is **always exactly one unit**, regardless of how far apart the
//! workspace ids are. Switching from workspace 2 to 3 moves windows by the
//! same distance as switching from 2 to 8. This is a deliberate design choice:
//!
//! - **Visual consistency** — every workspace switch looks identical, so the
//!   user can anticipate the animation duration and direction without surprises.
//! - **Simplicity** — the offset math is a single sign check, not a scaling
//!   calculation.
//! - **Animation cost** — a fixed distance means a fixed-duration animation,
//!   independent of which workspaces are involved.
//!
//! # The Workspace Stacking Invariant
//!
//! At any moment, for the active workspace `A`:
//!
//! | Workspace id | Parked at | Reason                                     |
//! |--------------|-----------|--------------------------------------------|
//! | `< A`        | `-y_unit` | Above the active workspace (parked).       |
//! | `= A`        | `0`       | The active workspace — on screen.          |
//! | `> A`        | `+y_unit` | Below the active workspace (parked).       |
//!
//! When switching workspaces, only the **source** and **destination**
//! participate in the animation. Non-participating workspaces whose parking
//! *side* changed (e.g. ws 3-7 when switching 2 → 8 — they were below active
//! 2, but become above active 8) are **silently teleported** (direct
//! `SetWindowPos`, bypassing the animator) to restore the invariant without
//! distracting motion.
//!
//! # Where this lives
//!
//! This function is pure math — no Win32, no IPC. It is applied in the daemon
//! animation bridge ([`crate::daemon::animation`]) when building
//! `WindowTarget` batches, **not** in the layout projection layer. The
//! projection layer (`layout::projection`) stays horizontal-only and
//! workspace-local; the daemon decides where each workspace sits vertically.

use crate::workspace::WorkspaceId;

/// Compute the vertical parking offset for a workspace relative to the active one.
///
/// Returns `±(monitor_height + window_gap)` based on whether `target_id` sits
/// below (`+`) or above (`-`) the `active_id`. Returns `0` when the two ids
/// are equal (i.e. the active workspace itself).
///
/// # Arguments
///
/// * `target_id` — The workspace whose parking offset is being computed.
/// * `active_id` — The currently active workspace.
/// * `monitor_height` — The monitor work-area height, in pixels.
/// * `window_gap` — The configured gap, in pixels. The same value is used
///   both intra-workspace (between adjacent windows) and inter-workspace
///   (between stacked workspaces), so the stacking looks visually consistent
///   with the in-workspace tiling.
///
/// # Examples
///
/// ```
/// # use scrolling_tiling_manager::workspace::WorkspaceId;
/// # use scrolling_tiling_manager::workspace::workspace_y_offset;
/// // Workspace 3 sits above active workspace 5.
/// let off = workspace_y_offset(WorkspaceId(3), WorkspaceId(5), 1080, 4);
/// assert_eq!(off, -(1080 + 4));
///
/// // Workspace 7 sits below active workspace 5.
/// let off = workspace_y_offset(WorkspaceId(7), WorkspaceId(5), 1080, 4);
/// assert_eq!(off, 1080 + 4);
///
/// // The active workspace itself sits at y = 0.
/// let off = workspace_y_offset(WorkspaceId(5), WorkspaceId(5), 1080, 4);
/// assert_eq!(off, 0);
/// ```
#[must_use]
pub fn workspace_y_offset(
    target_id: WorkspaceId,
    active_id: WorkspaceId,
    monitor_height: i32,
    window_gap: i32,
) -> i32 {
    // Use i64 for the subtraction so that the common case (target much
    // smaller than active) does not underflow a u32 before signum is taken.
    // The widening is free and self-documents the intent.
    let id_diff = i64::from(target_id.0) - i64::from(active_id.0);
    // signum returns -1, 0, or +1 — exactly the direction scalar we want,
    // without scaling by the (arbitrary) numeric distance between ids.
    let direction = id_diff.signum() as i32;
    direction * (monitor_height + window_gap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typical test monitor: 1920×1080 work area, 4px gap.
    const MONITOR_H: i32 = 1080;
    const GAP: i32 = 4;
    /// The parking unit for a 1080-tall monitor with a 4px gap.
    const Y_UNIT: i32 = MONITOR_H + GAP; // 1084

    // ---------------------------------------------------------------------
    // Sign / direction
    // ---------------------------------------------------------------------

    #[test]
    fn offset_for_previous_workspace_is_negative() {
        // Workspace 3 relative to active 5 → above (negative offset).
        let off = workspace_y_offset(WorkspaceId(3), WorkspaceId(5), MONITOR_H, GAP);
        assert_eq!(off, -Y_UNIT);
    }

    #[test]
    fn offset_for_next_workspace_is_positive() {
        // Workspace 7 relative to active 5 → below (positive offset).
        let off = workspace_y_offset(WorkspaceId(7), WorkspaceId(5), MONITOR_H, GAP);
        assert_eq!(off, Y_UNIT);
    }

    #[test]
    fn offset_for_active_workspace_is_zero() {
        // The active workspace itself sits at y = 0 by definition.
        let off = workspace_y_offset(WorkspaceId(5), WorkspaceId(5), MONITOR_H, GAP);
        assert_eq!(off, 0);
    }

    // ---------------------------------------------------------------------
    // ±1-unit invariant (the core design contract)
    // ---------------------------------------------------------------------

    #[test]
    fn offset_does_not_scale_with_id_distance() {
        // The ±1 unit model: the offset for workspace 2 is the SAME regardless
        // of whether the new active is 3, 4, or 8. Switching 2→3, 2→4, and 2→8
        // must all leave ws 2 at -Y_UNIT (it is previous to all of them).
        let off_to_3 = workspace_y_offset(WorkspaceId(2), WorkspaceId(3), MONITOR_H, GAP);
        let off_to_4 = workspace_y_offset(WorkspaceId(2), WorkspaceId(4), MONITOR_H, GAP);
        let off_to_8 = workspace_y_offset(WorkspaceId(2), WorkspaceId(8), MONITOR_H, GAP);
        assert_eq!(off_to_3, -Y_UNIT);
        assert_eq!(off_to_4, -Y_UNIT);
        assert_eq!(off_to_8, -Y_UNIT);
    }

    #[test]
    fn offset_independent_of_direction_of_switch() {
        // Switching 5→6 vs 6→5 must produce symmetric offsets for the same
        // workspace. Ws 5 relative to active 6 → -Y_UNIT. Ws 6 relative to
        // active 5 → +Y_UNIT. Equal magnitude, opposite sign.
        let ws5_below_6 = workspace_y_offset(WorkspaceId(5), WorkspaceId(6), MONITOR_H, GAP);
        let ws6_above_5 = workspace_y_offset(WorkspaceId(6), WorkspaceId(5), MONITOR_H, GAP);
        assert_eq!(ws5_below_6, -Y_UNIT);
        assert_eq!(ws6_above_5, Y_UNIT);
        assert_eq!(ws5_below_6.abs(), ws6_above_5.abs());
    }

    // ---------------------------------------------------------------------
    // Edge cases
    // ---------------------------------------------------------------------

    #[test]
    fn offset_smallest_id_above_active_two() {
        // Workspace 1 (the smallest id in the typical 1..=10 range) is just
        // above active workspace 2.
        let off = workspace_y_offset(WorkspaceId(1), WorkspaceId(2), MONITOR_H, GAP);
        assert_eq!(off, -Y_UNIT);
    }

    #[test]
    fn offset_handles_zero_gap() {
        // A zero gap degenerates y_unit to exactly monitor_height.
        let off = workspace_y_offset(WorkspaceId(2), WorkspaceId(1), 1080, 0);
        assert_eq!(off, 1080);
    }

    #[test]
    fn offset_same_id_returns_zero_regardless_of_inputs() {
        // The active-workspace offset is always zero, independent of monitor
        // size or gap. This guards against future refactors accidentally
        // returning a non-zero "self offset".
        assert_eq!(workspace_y_offset(WorkspaceId(1), WorkspaceId(1), 0, 0), 0);
        assert_eq!(
            workspace_y_offset(WorkspaceId(1), WorkspaceId(1), 9999, 999),
            0
        );
    }

    #[test]
    fn offset_does_not_underflow_when_target_much_smaller() {
        // Regression: the subtraction `target.0 - active.0` must not underflow
        // when target is much smaller than active. Use the extreme pair (1, 10).
        let off = workspace_y_offset(WorkspaceId(1), WorkspaceId(10), MONITOR_H, GAP);
        assert_eq!(off, -Y_UNIT);
    }

    #[test]
    fn offset_does_not_overflow_when_target_much_larger() {
        // Symmetric to the underflow test: target much larger than active.
        let off = workspace_y_offset(WorkspaceId(10), WorkspaceId(1), MONITOR_H, GAP);
        assert_eq!(off, Y_UNIT);
    }

    #[test]
    fn offset_at_u32_extremes_uses_i64_widening_correctly() {
        // Edge: the function body widens both ids to `i64` before subtracting
        // (see the `i64::from(...)` casts) precisely so that a large `target`
        // minus a small `active` (or vice versa) cannot underflow a `u32` and
        // flip the sign of `signum`. The under/overflow tests above only use
        // the small pair (1, 10); this test exercises the same protection at
        // the actual `u32` type boundary, where a naive `u32` subtraction
        // would wrap to a huge positive and produce the wrong direction.
        //
        // Arrange: the extreme id pairs. Act/Assert: sign is still correct.
        // target >> active → parks below → +Y_UNIT.
        assert_eq!(
            workspace_y_offset(WorkspaceId(u32::MAX), WorkspaceId(1), MONITOR_H, GAP),
            Y_UNIT,
        );
        // target << active → parks above → -Y_UNIT.
        assert_eq!(
            workspace_y_offset(WorkspaceId(1), WorkspaceId(u32::MAX), MONITOR_H, GAP),
            -Y_UNIT,
        );
        // Equal extremes → active workspace → 0, independent of magnitude.
        assert_eq!(
            workspace_y_offset(WorkspaceId(u32::MAX), WorkspaceId(u32::MAX), MONITOR_H, GAP),
            0,
        );
    }
}
