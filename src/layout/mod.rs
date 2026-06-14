//! Layout engine — infinite horizontal canvas with camera-based viewport.
//!
//! This is the largest module in stm. All layout computation is **pure Rust**
//! with **zero Win32 dependencies** — testable on any platform.
//!
//! # Architecture: VirtualLayout vs ActualLayout
//!
//! The layout system is built on a clear separation between the **infinite virtual
//! canvas** and the **actual on-screen windows** that Windows OS manages:
//!
//! - **[`VirtualLayout`]** — The complete description of all columns and windows on
//!   an infinite horizontal canvas. Columns live at logical positions starting from
//!   x=0, packed left-to-right with no gaps. A `viewport_offset` field acts as a
//!   **camera position** — it determines which slice of the canvas is currently visible.
//!   No pixel coordinates exist at this layer.
//!
//! - **[`ActualLayout`]** — The real pixel rectangles that Windows OS must render.
//!   Only windows visible in the viewport receive on-screen coordinates. Windows
//!   outside the viewport are **parked** at deterministic off-screen positions
//!   (one column-width beyond the nearest viewport edge), rather than being left at
//!   their unreachable virtual positions. This is critical because Windows OS does
//!   not gracefully ignore windows placed far off-screen — they must be moved to a
//!   known, nearby parking spot so animations and transitions work correctly.
//!
//! # The Camera Model
//!
//! The `viewport_offset` on [`VirtualLayout`] acts as a camera that slides along the
//! infinite canvas. Many operations — scrolling, focus-to-offscreen, swap —
//! are implemented simply by adjusting this offset rather than moving individual windows.
//!
//! ```text
//!  Camera →  ┃ viewport ┃
//!            ┃ visible  ┃
//!  ┌───┬───┬─╋───┬───┬─╋───┬───┐
//!  │ C1│ C2│ C3│ C4│ C5│ C6│ C7│   ← infinite canvas
//!  └───┴───┴─╋───┴───┴─╋───┴───┘
//!     parked    on-screen   parked
//!      left                  right
//! ```
//!
//! # The 2-Layer Pipeline
//!
//! Layout computation follows a functional, declarative approach — there is no
//! mutable "update Column position → propagate to Windows" loop. Instead:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Layer 1: VirtualLayout (infinite canvas, no pixels)        │
//! │  ┌───────────────────────────────────────────────────────┐  │
//! │  │ Column { width_eighths: 4, rows: [WinId(1), WinId(2)]}│  │
//! │  │ Column { width_eighths: 6, rows: [WinId(3)]          }│  │
//! │  │ viewport_offset: 0  ← camera position                 │  │
//! │  └───────────────────────────────────────────────────────┘  │
//! │         │                                                   │
//! │         │  projection::project()  (camera shift + park)     │
//! │         ▼                                                   │
//! │  Layer 2: ActualLayout (pixel rects for Windows OS)         │
//! │  ┌───────────────────────────────────────────────────────┐  │
//! │  │ Visible:  WinId(1) @ Rect { x:4, y:4, w:952, h:532 }  │  │
//! │  │ Visible:  WinId(2) @ Rect { x:4, y:540, w:952, h:532} │  │
//! │  │ Parked-L: WinId(3) @ Rect { x:-964, y:4, ... }        │  │
//! │  └───────────────────────────────────────────────────────┘  │
//! │         │                                                   │
//! │         │  AppliedLayout { virtual_layout, actual_layout }  │
//! │         ▼                                                   │
//! │  Animation layer: compare target rects vs real positions   │
//! │  (build_tweens filters no-ops; retargets mid-flight wins)  │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! See [`engine::LayoutEngine`] for the orchestrator that wires it all together.
//!
//! # Submodules
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`types`] | Core data types — [`Column`], [`VirtualLayout`], [`ActualLayout`] |
//! | [`projection`] | Virtual → actual projection with camera shift and parking |
//! | [`diff`] | Layout diff and [`AnimationHint`] classification |
//! | [`mutations`] | All pure mutation functions (scroll, focus, swap, resize, etc.) |
//! | [`engine`] | [`LayoutEngine`] orchestrator that wires mutations → projection → [`AppliedLayout`] |

pub mod diff;
pub mod engine;
pub mod mutations;
pub mod projection;
pub mod types;

pub use engine::LayoutEngine;
pub use mutations::NeighborLocation;
pub use types::{
    ActualEntry, ActualLayout, AnimationHint, AppliedLayout, Column, VirtualLayout, WindowMove,
};
