//! Core types shared across all modules of the animation engine.

/// A 2D integer vector, used for screen positions and sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct IVec2 {
    /// Horizontal component (x-axis or width).
    pub x: i32,
    /// Vertical component (y-axis or height).
    pub y: i32,
}

impl IVec2 {
    /// The zero vector.
    pub const ZERO: Self = Self { x: 0, y: 0 };

    /// Construct a new [`IVec2`].
    #[inline]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Screen-space rectangle expressed as origin + dimensions.
///
/// Uses `(x, y, w, h)` rather than Win32's `(left, top, right, bottom)`.
/// The Win32 backend converts between the two representations internally.
///
/// **Note**: This type differs from flow's `common::Rect` which uses
/// `{x, y, width, height}`. The two types coexist in different modules and
/// are converted at integration boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Rect {
    /// Left edge in screen coordinates.
    pub x: i32,
    /// Top edge in screen coordinates.
    pub y: i32,
    /// Width in pixels.
    pub w: i32,
    /// Height in pixels.
    pub h: i32,
}

impl Rect {
    /// Construct a new [`Rect`].
    #[inline]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// Extract the position as an [`IVec2`].
    #[inline]
    pub fn position(self) -> IVec2 {
        IVec2::new(self.x, self.y)
    }

    /// Extract the size as an [`IVec2`].
    #[inline]
    pub fn size(self) -> IVec2 {
        IVec2::new(self.w, self.h)
    }

    /// Returns `true` when width and height are both non-negative.
    #[inline]
    pub fn is_valid(self) -> bool {
        self.w >= 0 && self.h >= 0
    }
}

/// Opaque reference to a top-level window, wrapping the raw `HWND` as `isize`.
///
/// Using `isize` rather than a Windows-specific type keeps the public API
/// compilable on all platforms. The Win32 backend converts to `HWND` internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowRef(pub isize);

/// Desired final geometry for one window in an animation batch.
///
/// The animator reads the window's current geometry from the backend and
/// interpolates from that to the target.
#[derive(Debug, Clone)]
pub struct WindowTarget {
    /// The window to animate.
    pub window_ref: WindowRef,
    /// Desired top-left corner in screen coordinates after animation.
    pub final_position: IVec2,
    /// Desired size (width × height) after animation.
    pub final_size: IVec2,
}

impl WindowTarget {
    /// Construct a new [`WindowTarget`].
    pub fn new(window_ref: WindowRef, final_position: IVec2, final_size: IVec2) -> Self {
        Self {
            window_ref,
            final_position,
            final_size,
        }
    }

    /// Convert the target fields into a [`Rect`].
    pub fn as_rect(&self) -> Rect {
        Rect::new(
            self.final_position.x,
            self.final_position.y,
            self.final_size.x,
            self.final_size.y,
        )
    }
}

/// Opaque handle returned by [`crate::animation::WindowAnimator::animate`].
///
/// Currently used to identify an animation request. Future versions may
/// expose progress queries or per-handle cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnimationHandle(pub(crate) u64);

/// Errors produced by the animation engine.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AnimationError {
    /// A Win32 (or other backend) call returned an error.
    #[error("backend error: {0}")]
    Backend(String),

    /// The submitted batch contained no windows.
    #[error("animation batch must contain at least one window")]
    EmptyBatch,

    /// The background worker thread has exited.
    #[error("animator worker thread is not running")]
    WorkerDead,
}

/// Convenience [`Result`] alias for this crate.
pub type Result<T> = std::result::Result<T, AnimationError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ivec2_new_stores_components() {
        let v = IVec2::new(3, -7);
        assert_eq!(v.x, 3);
        assert_eq!(v.y, -7);
    }

    #[test]
    fn rect_position_and_size() {
        let r = Rect::new(10, 20, 800, 600);
        assert_eq!(r.position(), IVec2::new(10, 20));
        assert_eq!(r.size(), IVec2::new(800, 600));
    }

    #[test]
    fn rect_validity() {
        assert!(Rect::new(0, 0, 100, 100).is_valid());
        assert!(!Rect::new(0, 0, -1, 100).is_valid());
    }

    #[test]
    fn window_target_as_rect() {
        let t = WindowTarget::new(WindowRef(42), IVec2::new(50, 60), IVec2::new(400, 300));
        assert_eq!(t.as_rect(), Rect::new(50, 60, 400, 300));
    }
}
