//! Physical monitor owning a vertical stack of workspaces.
//!
//! A [`Monitor`] is the top of the workspace hierarchy under
//! [`ScrollTilingManager`](crate::daemon::ScrollTilingManager). It binds two
//! pieces of screen geometry to the [`Workspace`]s available on that display:
//! the full physical [`Rect`](crate::common::Rect) (for parking workspaces
//! off-screen) and the taskbar-excluded work area (for in-workspace tiling).
//! Only one workspace per monitor is on screen at a time — the
//! `active_workspace` — while the rest sit parked above and below, ready to
//! be scrolled into view.
//!
//! See the [module-level docs](super) for the full hierarchy diagram.

use super::{FloatingSpace, ScrollingSpace, Workspace, WorkspaceId};
use crate::common::Rect;

/// A physical monitor and the workspaces it can show.
///
/// The monitor remembers **two** screen rectangles for this display:
///
/// - **`screen_rect`** — the full physical bounds (taskbar included). Used
///   for *inter-workspace* geometry: parking a non-active workspace far
///   enough off-screen that none of it leaks past the taskbar strip (see
///   `workspace::workspace_y_offset`).
/// - **`work_area`** — the taskbar-excluded bounds. Used for *intra-workspace*
///   geometry: sizing each workspace so tiled windows never underlap the
///   taskbar or any shell appbar (e.g. `yasb`).
///
/// Each [`Workspace`]'s [`ScrollingSpace`] *also* carries a copy of the work
/// area (inside its `MonitorInfo`) for projection — the two are kept in sync
/// by the daemon at construction time. For this skeleton there is exactly
/// one monitor, so the duplication is benign; multi-monitor support lands
/// later.
pub struct Monitor {
    /// Full physical screen geometry for this display, in screen
    /// coordinates. Taskbar and appbars are NOT excluded. Source of truth
    /// for parking workspaces off-screen.
    screen_rect: Rect,
    /// Work-area geometry (taskbar excluded) for this display, in screen
    /// coordinates. Source of truth for sizing workspaces.
    work_area: Rect,
    /// The vertical stack of workspaces on this monitor. Index `0` is the
    /// topmost (first created).
    workspaces: Vec<Workspace>,
    /// Index into [`workspaces`](Self::workspaces) of the currently visible
    /// workspace. Always a valid index while `workspaces` is non-empty.
    active_workspace: usize,
}

impl Monitor {
    /// Create a new monitor with the given geometry and workspace stack.
    ///
    /// `screen_rect` is the full physical monitor rect (taskbar included);
    /// `work_area` is the taskbar-excluded rect used for in-workspace
    /// tiling. The daemon populates both from a single
    /// `GetMonitorInfoW` query at construction time.
    ///
    /// `active_workspace` is clamped into range so a stale index can never
    /// panic a later accessor. If `workspaces` is empty the active index is
    /// forced to `0`; callers should push a workspace before relying on
    /// [`active_workspace`](Self::active_workspace).
    #[must_use]
    pub fn new(
        screen_rect: Rect,
        work_area: Rect,
        workspaces: Vec<Workspace>,
        active_workspace: usize,
    ) -> Self {
        let active_workspace = if workspaces.is_empty() {
            0
        } else {
            active_workspace.min(workspaces.len() - 1)
        };
        Self {
            screen_rect,
            work_area,
            workspaces,
            active_workspace,
        }
    }

    /// The full physical [`Rect`] for this monitor (taskbar included).
    ///
    /// Use this for inter-workspace math (parking workspaces off-screen).
    /// For in-workspace window placement use [`work_area`](Self::work_area).
    #[must_use]
    pub fn screen_rect(&self) -> Rect {
        self.screen_rect
    }

    /// The work-area [`Rect`] for this monitor (taskbar excluded).
    #[must_use]
    pub fn work_area(&self) -> Rect {
        self.work_area
    }

    /// Read access to every workspace on this monitor.
    #[must_use]
    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// The index of the currently visible workspace.
    #[must_use]
    pub fn active_workspace_index(&self) -> usize {
        self.active_workspace
    }

    /// Borrow the active workspace (the one currently on screen).
    ///
    /// # Panics
    ///
    /// Panics if the monitor has no workspaces. The daemon always keeps at
    /// least one workspace per monitor, so this never fires in practice.
    #[must_use]
    pub fn active_workspace(&self) -> &Workspace {
        &self.workspaces[self.active_workspace]
    }

    /// Mutably borrow the active workspace.
    ///
    /// # Panics
    ///
    /// Panics if the monitor has no workspaces — see
    /// [`active_workspace`](Self::active_workspace).
    #[must_use]
    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_workspace]
    }

    /// Borrow the active workspace's scrolling space.
    ///
    /// Convenience for `self.active_workspace().scrolling` — the daemon routes
    /// every tiling mutation through this accessor.
    ///
    /// # Panics
    ///
    /// Panics if the monitor has no workspaces.
    #[must_use]
    pub fn active_scrolling(&self) -> &ScrollingSpace {
        &self.active_workspace().scrolling
    }

    /// Mutably borrow the active workspace's scrolling space.
    ///
    /// # Panics
    ///
    /// Panics if the monitor has no workspaces.
    #[must_use]
    pub fn active_scrolling_mut(&mut self) -> &mut ScrollingSpace {
        &mut self.active_workspace_mut().scrolling
    }

    /// Borrow the active workspace's floating space.
    ///
    /// Convenience for `self.active_workspace().floating` — the daemon routes
    /// float/tile transitions through this accessor.
    ///
    /// # Panics
    ///
    /// Panics if the monitor has no workspaces.
    #[must_use]
    pub fn active_floating(&self) -> &FloatingSpace {
        &self.active_workspace().floating
    }

    /// Mutably borrow the active workspace's floating space.
    ///
    /// # Panics
    ///
    /// Panics if the monitor has no workspaces.
    #[must_use]
    pub fn active_floating_mut(&mut self) -> &mut FloatingSpace {
        &mut self.active_workspace_mut().floating
    }

    // ---- Workspace lookup-by-id ------------------------------------------
    //
    // The accessors above all operate on the *active* workspace. The
    // workspace-op commands (SwitchWorkspace, MoveWindowToWorkspace) need to
    // reach *other* workspaces on the same monitor — to read their layouts for
    // animation, mutate their scrolling spaces on window-transport, or change
    // which workspace is active. The methods below give the daemon stable-id
    // access without exposing the underlying `Vec<Workspace>` indices, so the
    // storage representation can change (e.g. a future swap operation that
    // reorders the vec) without breaking callers.

    /// Find the storage index of a workspace by its stable [`WorkspaceId`].
    ///
    /// Linear scan over the workspace stack. Workspaces are not guaranteed to
    /// be sorted by id (a future `SwapWorkspace` operation may reorder them),
    /// so a `position` search is the only correct general lookup.
    ///
    /// Returns `None` if no workspace on this monitor has the given id. Use
    /// [`active_workspace_index`](Self::active_workspace_index) if you need
    /// the active workspace's position rather than its id.
    #[must_use]
    pub fn find_workspace_index(&self, id: WorkspaceId) -> Option<usize> {
        self.workspaces.iter().position(|ws| ws.id == id)
    }

    /// Borrow a workspace by stable [`WorkspaceId`].
    ///
    /// Returns `None` if no workspace on this monitor has the given id. For
    /// the on-screen workspace prefer the cheaper
    /// [`active_workspace`](Self::active_workspace) accessor.
    #[must_use]
    pub fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        // `find_workspace_index` returns an owned `Option<usize>` (no borrow
        // held), so NLL permits the indexing lookup immediately afterwards.
        self.find_workspace_index(id).map(|i| &self.workspaces[i])
    }

    /// Mutably borrow a workspace by stable [`WorkspaceId`].
    ///
    /// Returns `None` if no workspace on this monitor has the given id. For
    /// the on-screen workspace prefer
    /// [`active_workspace_mut`](Self::active_workspace_mut).
    ///
    /// Note: this is intentionally a single-target lookup. If a caller needs
    /// to mutate two workspaces at once (e.g. `MoveWindowToWorkspace`
    /// removes a window from the source and inserts into the dest), it must
    /// decompose the operation into single-workspace steps — Rust's borrow
    /// checker forbids two simultaneous `&mut` borrows of the same `Vec`.
    /// The daemon-side workspace-op handlers do exactly this.
    #[must_use]
    pub fn workspace_mut(&mut self, id: WorkspaceId) -> Option<&mut Workspace> {
        match self.find_workspace_index(id) {
            Some(i) => Some(&mut self.workspaces[i]),
            None => None,
        }
    }

    /// The stable [`WorkspaceId`] of the currently on-screen workspace.
    ///
    /// Convenience for `self.active_workspace().id`. The workspace-op dispatch
    /// handlers use this to capture the source workspace before calling
    /// [`set_active_workspace`](Self::set_active_workspace).
    #[must_use]
    pub fn active_workspace_id(&self) -> WorkspaceId {
        self.active_workspace().id
    }

    /// Change which workspace is on screen, addressed by stable [`WorkspaceId`].
    ///
    /// This is the dispatch-time mechanism for `SwitchWorkspace`: the daemon
    /// captures the current active id via
    /// [`active_workspace_id`](Self::active_workspace_id), calls this method
    /// to update the active index, then submits the animation batch (see the
    /// daemon dispatch module).
    ///
    /// Returns the **previous** active *index* (not id) on success so callers
    /// can report or restore it; returns `None` if no workspace on this
    /// monitor has the given id, in which case the active index is left
    /// untouched. Calling with the already-active id is a no-op but still
    /// returns `Some(previous_index)`.
    ///
    /// This method is deliberately pure bookkeeping — it does **not** animate
    /// or touch any window positions. The daemon is responsible for
    /// submitting animation targets for both the source and destination
    /// workspaces after this returns.
    #[must_use]
    pub fn set_active_workspace(&mut self, id: WorkspaceId) -> Option<usize> {
        match self.find_workspace_index(id) {
            Some(new_idx) => {
                let prev = self.active_workspace;
                self.active_workspace = new_idx;
                Some(prev)
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Rect;
    use crate::layout::types::{MonitorInfo, Padding};

    /// Build a minimal [`ScrollingSpace`] for tests — parameters mirror the
    /// test setup in `scrolling_space.rs::tests` so behaviour matches the
    /// production engine. We never drive the scrolling space in these tests;
    /// it exists only to satisfy [`Workspace::new`]'s signature.
    fn make_scrolling() -> ScrollingSpace {
        ScrollingSpace::new(
            MonitorInfo {
                work_area: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            960,
            320,
            Padding {
                window_gap: 4,
                up: 0,
                down: 0,
            },
            4,
        )
    }

    /// Build a [`Monitor`] whose workspaces have the given ids (in order),
    /// with the given index active. Helper keeps the lookup-by-id tests
    /// self-contained.
    fn make_monitor_with_ids(ids: &[u32], active_idx: usize) -> Monitor {
        let workspaces: Vec<Workspace> = ids
            .iter()
            .map(|&id| Workspace::new(WorkspaceId(id), make_scrolling()))
            .collect();
        // Tests don't model a taskbar, so screen_rect and work_area are
        // identical — the two-rect distinction only matters for the parking
        // math exercised in y_offset.rs.
        let rect = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        Monitor::new(rect, rect, workspaces, active_idx)
    }

    // ---- find_workspace_index --------------------------------------------

    // Positive: existing ids resolve to their storage position.
    #[test]
    fn find_workspace_index_returns_position_for_existing_id() {
        let monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        assert_eq!(monitor.find_workspace_index(WorkspaceId(1)), Some(0));
        assert_eq!(monitor.find_workspace_index(WorkspaceId(2)), Some(1));
        assert_eq!(monitor.find_workspace_index(WorkspaceId(3)), Some(2));
    }

    // Negative: missing id returns None rather than panicking.
    #[test]
    fn find_workspace_index_returns_none_for_missing_id() {
        let monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        assert_eq!(monitor.find_workspace_index(WorkspaceId(99)), None);
    }

    // Positive: lookup works even if workspaces are stored out of numeric
    // order — important for a future SwapWorkspace that reorders the vec.
    #[test]
    fn find_workspace_index_works_for_unordered_ids() {
        let monitor = make_monitor_with_ids(&[5, 1, 9], 0);
        assert_eq!(monitor.find_workspace_index(WorkspaceId(9)), Some(2));
        assert_eq!(monitor.find_workspace_index(WorkspaceId(5)), Some(0));
    }

    // ---- workspace / workspace_mut ---------------------------------------

    #[test]
    fn workspace_borrows_by_id() {
        let monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        assert_eq!(
            monitor.workspace(WorkspaceId(2)).map(|ws| ws.id),
            Some(WorkspaceId(2)),
        );
    }

    #[test]
    fn workspace_returns_none_for_missing_id() {
        let monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        assert!(monitor.workspace(WorkspaceId(99)).is_none());
    }

    #[test]
    fn workspace_mut_borrows_by_id() {
        let mut monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        let ws = monitor
            .workspace_mut(WorkspaceId(3))
            .expect("workspace 3 should exist");
        assert_eq!(ws.id, WorkspaceId(3));
    }

    #[test]
    fn workspace_mut_returns_none_for_missing_id() {
        let mut monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        assert!(monitor.workspace_mut(WorkspaceId(99)).is_none());
    }

    // ---- active_workspace_id ---------------------------------------------

    #[test]
    fn active_workspace_id_reflects_current_active() {
        // Active index 1 -> workspace with id 10.
        let monitor = make_monitor_with_ids(&[5, 10, 15], 1);
        assert_eq!(monitor.active_workspace_id(), WorkspaceId(10));
    }

    // ---- set_active_workspace --------------------------------------------

    #[test]
    fn set_active_workspace_updates_index_and_returns_previous() {
        let mut monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        let prev = monitor.set_active_workspace(WorkspaceId(3));
        assert_eq!(prev, Some(0));
        assert_eq!(monitor.active_workspace_index(), 2);
        assert_eq!(monitor.active_workspace_id(), WorkspaceId(3));
    }

    #[test]
    fn set_active_workspace_returns_none_for_missing_id_leaving_state_unchanged() {
        let mut monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        let prev = monitor.set_active_workspace(WorkspaceId(99));
        assert_eq!(prev, None);
        // Active index must be untouched.
        assert_eq!(monitor.active_workspace_index(), 0);
    }

    #[test]
    fn set_active_workspace_to_current_returns_previous_without_changing_state() {
        let mut monitor = make_monitor_with_ids(&[1, 2, 3], 1);
        let prev = monitor.set_active_workspace(WorkspaceId(2));
        assert_eq!(prev, Some(1));
        assert_eq!(monitor.active_workspace_index(), 1);
    }

    #[test]
    fn set_active_workspace_round_trip_restores_original_active() {
        let mut monitor = make_monitor_with_ids(&[1, 2, 3], 0);
        let _ = monitor.set_active_workspace(WorkspaceId(3));
        let prev = monitor.set_active_workspace(WorkspaceId(1));
        assert_eq!(prev, Some(2));
        assert_eq!(monitor.active_workspace_index(), 0);
    }

    // ---- screen_rect / work_area storage --------------------------------
    //
    // Regression guard for the y_offset bug: a Monitor must carry the full
    // physical rect SEPARATELY from the taskbar-excluded work area, so the
    // parking math can use the (larger) physical height.

    #[test]
    fn screen_rect_returns_stored_value() {
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1200,
        };
        let work = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1160,
        };
        let monitor = Monitor::new(screen, work, Vec::new(), 0);
        assert_eq!(monitor.screen_rect(), screen);
    }

    #[test]
    fn screen_rect_and_work_area_are_stored_independently() {
        // Physical 1200-tall screen with a 40px bottom taskbar → work area
        // is 1160 tall. The two must NOT collapse to the same value, or the
        // parking offset would under-travel by exactly the taskbar height.
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1200,
        };
        let work = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1160,
        };
        let monitor = Monitor::new(screen, work, Vec::new(), 0);
        assert_ne!(monitor.screen_rect(), monitor.work_area());
        assert_eq!(
            monitor.screen_rect().height - monitor.work_area().height,
            40
        );
    }
}
