//! Workspace hierarchy — monitors, workspaces, and the scrolling/floating split.
//!
//! This module groups everything that sits *above* the pure layout math in the
//! [`layout`](crate::layout) module. It models a niri-style virtual desktop
//! stack: a [`Monitor`] owns one or more [`Workspace`]s, and a workspace owns
//! exactly one [`ScrollingSpace`] (the tiled windows on an infinite horizontal
//! canvas) plus one [`FloatingSpace`] (the non-tiled windows — currently a
//! stub).
//!
//! # The Hierarchy
//!
//! ```text
//! Vec<Monitor>           (ScrollTilingManager tracks the active monitor)
//!   └─ Monitor           (one physical display; owns its work area + workspaces)
//!       └─ Vec<Workspace>  (only the active_workspace is visible)
//!           └─ Workspace
//!               ├─ ScrollingSpace   ← reused from the old LayoutEngine
//!               └─ FloatingSpace    ← stub, different coordinate space
//! ```
//!
//! # Vertical Scrolling Between Workspaces
//!
//! The horizontal scrolling inside a [`ScrollingSpace`] (left/right across
//! columns) now has a vertical analogue: workspaces are stacked "above" and
//! "below" the active one. Switching workspaces will eventually animate the
//! whole stack vertically, the same way scrolling a column animates windows
//! horizontally. **The animation design is not yet finalised**, so the
//! workspace-switch IPC commands (`switchworkspace`, `swapworkspace`,
//! `movetoworkspace`) are wired up as stubs in this skeleton — see the daemon
//! dispatch module.
//!
//! # What Lives Here vs. What Doesn't
//!
//! - **Here**: the monitor/workspace container types, the `WorkspaceId`
//!   identifier, and the two space kinds a workspace holds.
//! - **Not here**: the [`WindowRegistry`](crate::registry::WindowRegistry),
//!   IPC plumbing, and window-event hooks. Those remain direct fields of
//!   [`ScrollTilingManager`](crate::daemon::ScrollTilingManager). A workspace
//!   never touches Win32 or the registry directly; the daemon is the only
//!   thing that shuttles windows between the registry and the active
//!   workspace's [`ScrollingSpace`].

use serde::{Deserialize, Serialize};

pub mod floating_space;
pub mod monitor;
pub mod scrolling_space;

pub use floating_space::FloatingSpace;
pub use monitor::Monitor;
pub use scrolling_space::ScrollingSpace;

/// Stable, IPC-friendly identifier for a workspace.
///
/// Workspaces are numbered with a plain `u32`, mirroring how niri and most
/// Wayland compositors expose workspace ids over IPC. The id is **stable**:
/// it does not change when workspaces are reordered or swapped, so clients
/// can key on it safely. The [`ScrollTilingManager`](crate::daemon::ScrollTilingManager)
/// assigns ids at creation time and never reuses them within a session.
///
/// # Serialisation
///
/// `WorkspaceId` is `#[serde(transparent)]`, so it serialises as a bare
/// integer — e.g. `3` rather than `{"workspace_id": 3}`. This keeps the IPC
/// message shape small and matches the `u32` payloads of the
/// `switchworkspace` / `swapworkspace` / `movetoworkspace` commands.
///
/// ```
/// # use scrolling_tiling_manager::workspace::WorkspaceId;
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
///   the user dragged it to. It is currently a stub.
///
/// Because the two spaces never interact at the layout level, splitting them
/// keeps the tiling math pure and leaves room for floating-window logic to
/// grow independently. The daemon always consults the **active** workspace of
/// the **active** monitor — see
/// [`ScrollTilingManager::active_workspace`](crate::daemon::ScrollTilingManager).
pub struct Workspace {
    /// Stable identifier for this workspace. Never reused within a session.
    pub id: WorkspaceId,
    /// The tiled windows on this workspace, laid out on the scrolling canvas.
    pub scrolling: ScrollingSpace,
    /// The floating (non-tiled) windows on this workspace. Currently a stub.
    pub floating: FloatingSpace,
}

impl Workspace {
    /// Create a new workspace with the given id and scrolling space.
    ///
    /// The floating space starts empty (a fresh [`FloatingSpace`] stub). Use
    /// this at startup to wrap the [`ScrollingSpace`] built for the initial
    /// monitor.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use scrolling_tiling_manager::workspace::{Workspace, WorkspaceId, ScrollingSpace};
    /// # use scrolling_tiling_manager::layout::types::MonitorInfo;
    /// # use scrolling_tiling_manager::common::Rect;
    /// # let monitor = MonitorInfo { work_area: Rect { x: 0, y: 0, width: 1920, height: 1080 } };
    /// # let scrolling = ScrollingSpace::new(monitor, 960, 4,
    /// #     scrolling_tiling_manager::layout::types::Padding { window_gap: 4, up: 0, down: 0 }, 4);
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
