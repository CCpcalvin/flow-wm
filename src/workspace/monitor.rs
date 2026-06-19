//! Physical monitor owning a vertical stack of workspaces.
//!
//! A [`Monitor`] is the top of the workspace hierarchy under
//! [`ScrollTilingManager`](crate::daemon::ScrollTilingManager). It binds a
//! piece of screen geometry (the work-area [`Rect`](crate::common::Rect)) to
//! the [`Workspace`]s available on that display. Only one workspace per
//! monitor is on screen at a time — the `active_workspace` — while the rest
//! sit parked above and below, ready to be scrolled into view.
//!
//! See the [module-level docs](super) for the full hierarchy diagram.

use super::{ScrollingSpace, Workspace};
use crate::common::Rect;

/// A physical monitor and the workspaces it can show.
///
/// The monitor remembers its work-area [`Rect`] so that new workspaces can be
/// sized correctly and so future vertical-packing math (the workspace analogue
/// of horizontal column packing) has the geometry it needs. Each
/// [`Workspace`]'s [`ScrollingSpace`] *also* carries a copy of this rect
/// (inside its `MonitorInfo`) for projection — the two are kept in sync by the
/// daemon at construction time. For this skeleton there is exactly one
/// monitor, so the duplication is benign; multi-monitor support lands later.
pub struct Monitor {
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
    /// Create a new monitor with the given work area and workspace stack.
    ///
    /// `active_workspace` is clamped into range so a stale index can never
    /// panic a later accessor. If `workspaces` is empty the active index is
    /// forced to `0`; callers should push a workspace before relying on
    /// [`active_workspace`](Self::active_workspace).
    #[must_use]
    pub fn new(work_area: Rect, workspaces: Vec<Workspace>, active_workspace: usize) -> Self {
        let active_workspace = if workspaces.is_empty() {
            0
        } else {
            active_workspace.min(workspaces.len() - 1)
        };
        Self {
            work_area,
            workspaces,
            active_workspace,
        }
    }

    /// The work-area [`Rect`] for this monitor.
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
}
