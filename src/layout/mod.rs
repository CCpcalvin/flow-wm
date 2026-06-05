//! Layout engine — virtual canvas, projection, and mutation logic.
//!
//! This is the largest module in stm. All layout computation is **pure Rust**
//! with **zero Win32 dependencies** — testable on any platform.
//!
//! # The 3-Layer Pipeline
//!
//! Layout computation follows a functional, declarative approach — there is no
//! mutable "update Column position → propagate to Windows" loop. Instead:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Layer 1: VirtualLayout (logical, no pixels)                │
//! │  ┌───────────────────────────────────────────────────────┐  │
//! │  │ Column { width_eighths: 4, rows: [WinId(1), WinId(2)]}│  │
//! │  │ Column { width_eighths: 6, rows: [WinId(3)]          }│  │
//! │  │ viewport_offset: 0                                     │  │
//! │  └───────────────────────────────────────────────────────┘  │
//! │         │                                                   │
//! │         │  projection::project()  (pure function)            │
//! │         ▼                                                   │
//! │  Layer 2: ActualLayout (pixel rects, padding baked in)      │
//! │  ┌───────────────────────────────────────────────────────┐  │
//! │  │ ActualEntry { WinId(1), Rect { x:4, y:4, w:952, h:532}}│  │
//! │  │ ActualEntry { WinId(2), Rect { x:4, y:540, w:952, h:532}}│ │
//! │  │ ActualEntry { WinId(3), Rect { x:964, y:4, w:1428, h:1072}}│ │
//! │  └───────────────────────────────────────────────────────┘  │
//! │         │                                                   │
//! │         │  diff::diff()  (pure function)                     │
//! │         ▼                                                   │
//! │  Layer 3: LayoutDiff (animation instructions)               │
//! │  ┌───────────────────────────────────────────────────────┐  │
//! │  │ WindowMove { WinId(1), from: old_rect, to: new_rect } │  │
//! │  └───────────────────────────────────────────────────────┘  │
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
//! | [`projection`] | Virtual → actual projection (pure function) |
//! | [`diff`] | Layout diff and [`AnimationHint`] classification |
//! | [`mutations`] | All pure mutation functions (scroll, focus, swap, resize, etc.) |
//! | [`engine`] | [`LayoutEngine`] orchestrator that wires mutations → projection → diff |

pub mod diff;
pub mod engine;
pub mod mutations;
pub mod projection;
pub mod types;

pub use engine::LayoutEngine;
pub use types::{ActualEntry, ActualLayout, AnimationHint, Column, VirtualLayout, WindowMove};
