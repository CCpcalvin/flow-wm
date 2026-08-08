//! Workspace hierarchy — monitors, workspaces, and the scrolling/floating split.
//!
//! Everything that sits *above* the pure layout math in the [`layout`](crate::layout)
//! module. Models a niri-style virtual desktop stack: a [`Monitor`] owns one or
//! more [`Workspace`]s, and a workspace owns exactly one [`ScrollingSpace`] (tiled
//! windows on an infinite horizontal canvas) plus one [`FloatingSpace`] (non-tiled
//! windows in literal pixel coordinates).
//!
//! # Vertical scrolling between workspaces
//!
//! The horizontal scrolling inside a [`ScrollingSpace`] (left/right across columns)
//! has a vertical analogue: workspaces stacked "above" and "below" the active one,
//! switched the same way columns scroll. `switch-workspace` and `move-to-workspace`
//! animate a vertical-packing switch (see the daemon dispatch module); only
//! `swap-workspace` remains a stub pending its own animation model.
//!
//! # What lives here vs. what doesn't
//!
//! A workspace never touches Win32 or the registry directly. The daemon
//! ([`FlowWM`](crate::daemon::FlowWM)) is the only thing
//! that shuttles windows between the
//! [`WindowRegistry`](crate::registry::WindowRegistry) and the active workspace's
//! [`ScrollingSpace`]. IPC plumbing and window-event hooks remain direct fields of
//! [`FlowWM`](crate::daemon::FlowWM), not of the workspace.
//!
//! See the developer guide's *Workspace Hierarchy* chapter
//! (`docs/src/dev-guide/workspace.md`) for the hierarchy diagram and the roadmap
//! for multi-monitor support.

use serde::{Deserialize, Serialize};

pub mod floating_space;
pub mod monitor;
pub mod scrolling_space;
pub mod y_offset;

pub use floating_space::FloatingSpace;
pub use monitor::Monitor;
pub use scrolling_space::ScrollingSpace;
pub use y_offset::workspace_y_offset;

/// Stable, IPC-friendly identifier for a workspace.
///
/// Workspaces are numbered with a plain `u32`, mirroring how niri and most
/// Wayland compositors expose workspace ids over IPC. The id is **stable**:
/// it does not change when workspaces are reordered or swapped, so clients
/// can key on it safely. The [`FlowWM`](crate::daemon::FlowWM)
/// assigns ids at creation time and never reuses them within a session.
///
/// # Serialisation
///
/// `WorkspaceId` is `#[serde(transparent)]`, so it serialises as a bare
/// integer — e.g. `3` rather than `{"workspace_id": 3}`. This keeps the IPC
/// message shape small and matches the `u32` payloads of the
/// `switch-workspace` / `swap-workspace` / `move-to-workspace` commands.
///
/// ```
/// # use flow_wm::workspace::WorkspaceId;
/// let id = WorkspaceId(7);
/// assert_eq!(id.0, 7);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub u32);

/// One virtual desktop: a tiling half and a floating half.
///
/// A [`Workspace`] is the niri-style "virtual desktop" unit: the thing the
/// user switches between by scrolling vertically. Each workspace is fully
/// independent — it owns its own tiled windows (in the [`ScrollingSpace`])
/// and its own floating windows (in the [`FloatingSpace`]). Only one
/// workspace per monitor is visible at a time (the monitor's active
/// workspace); the rest are parked "above" or "below", ready to scroll into
/// view.
///
/// # Two Coordinate Spaces
///
/// The two halves share a monitor but use **different coordinate spaces**:
///
/// - [`ScrollingSpace`] runs windows through the virtual → actual projection
///   pipeline (an infinite horizontal canvas clipped to the work area).
/// - [`FloatingSpace`] keeps each window at the literal on-screen rectangle
///   the user dragged it to (or a centered default when first floated).
///
/// Because the two spaces never interact at the layout level, splitting them
/// keeps the tiling math pure and leaves room for floating-window logic to
/// grow independently. The daemon always consults the **active** workspace of
/// the **active** monitor — see
/// [`FlowWM::active_workspace`](crate::daemon::FlowWM).
pub struct Workspace {
    /// Stable identifier for this workspace. Never reused within a session.
    pub id: WorkspaceId,
    /// The tiled windows on this workspace, laid out on the scrolling canvas.
    pub scrolling: ScrollingSpace,
    /// The floating (non-tiled) windows on this workspace (literal pixel rects).
    pub floating: FloatingSpace,
}

impl Workspace {
    /// Create a new workspace with the given id and scrolling space.
    ///
    /// The floating space starts empty. Use
    /// this at startup to wrap the [`ScrollingSpace`] built for the initial
    /// monitor.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use flow_wm::workspace::{Workspace, WorkspaceId, ScrollingSpace};
    /// # use flow_wm::layout::types::MonitorInfo;
    /// # use flow_wm::common::Rect;
    /// # let monitor = MonitorInfo { work_area: Rect { x: 0, y: 0, width: 1920, height: 1080 } };
    /// # let scrolling = ScrollingSpace::new(monitor, 960, 320, 100, 100,
    /// #     flow_wm::layout::types::Padding { window_gap: 4, up: 0, down: 0 }, 4);
    /// let ws = Workspace::new(WorkspaceId(1), scrolling);
    /// assert_eq!(ws.id, WorkspaceId(1));
    /// ```
    #[must_use]
    pub fn new(id: WorkspaceId, scrolling: ScrollingSpace) -> Self {
        Self {
            id,
            scrolling,
            floating: FloatingSpace::new(),
        }
    }
}
