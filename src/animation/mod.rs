//! Embedded animation engine — rect-based HWND animation for Windows tiling
//! window managers.
//!
//! This module is an in-tree copy of the `window-animation` crate, providing
//! smooth, frame-paced window movement driven by a background worker thread.
//!
//! # Quick Start
//!
//! 1. Create a [`backend::win32::Win32Backend`] (or [`backend::mock::MockBackend`] for tests).
//! 2. Build an [`AnimatorConfig`] (or use [`AnimatorConfig::default`]).
//! 3. Create a [`WindowAnimator`] and call [`WindowAnimator::animate`] with a
//!    list of [`WindowTarget`] values describing the desired final geometry for
//!    each window.
//!
//! ```rust,ignore
//! use flow_wm::animation::{AnimatorConfig, WindowAnimator, WindowRef, WindowTarget, IVec2};
//! use flow_wm::animation::backend::win32::Win32Backend;
//!
//! let backend = Win32Backend::new();
//! let config  = AnimatorConfig::default();
//! let mut animator = WindowAnimator::new(backend, config);
//!
//! let targets = vec![
//!     WindowTarget::new(WindowRef(hwnd as isize), IVec2::new(0, 0), IVec2::new(800, 600)),
//! ];
//! animator.animate(targets).unwrap();
//! ```

pub(crate) mod animator;
pub(crate) mod backend;
pub(crate) mod batch;
pub(crate) mod config;
pub(crate) mod easing;
pub(crate) mod interpolation;
pub(crate) mod metrics;
pub(crate) mod types;

/// Re-export of the background-threaded animator that drives all window moves.
pub use animator::WindowAnimator;
/// Re-export of the full animator configuration struct.
pub use config::AnimatorConfig;
/// Re-export of the frame-timing metrics summary produced after an animation run.
pub use metrics::FrameMetrics;
/// Re-export of the metrics recorder used to collect per-frame timestamps.
pub use metrics::MetricsRecorder;
/// Re-export of the error enum produced by animation operations.
pub use types::AnimationError;
/// Re-export of the 2D integer vector type used for positions and sizes.
pub use types::IVec2;
/// Re-export of the animation engine's rectangle type.
///
/// This type uses `{x, y, w, h}` fields, distinct from flow's `common::Rect`
/// which uses `{x, y, width, height}`. The two types coexist in different
/// modules and are converted at integration boundaries.
pub use types::Rect as AnimRect;
/// Re-export of the opaque window reference type wrapping HWND as `isize`.
pub use types::WindowRef;
/// Re-export of the target geometry description for one animated window.
pub use types::WindowTarget;
