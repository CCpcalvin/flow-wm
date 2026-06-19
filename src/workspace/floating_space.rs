//! Floating window space — stub for future implementation.
//!
//! Floating windows are tracked by [`WindowRegistry`](crate::registry::WindowRegistry)
//! but not managed by the scrolling space. This module is a placeholder for
//! future floating window management features such as smart placement,
//! stacking order, and gap management.
//!
//! # Where this lives
//!
//! A [`FloatingSpace`] is owned by a [`Workspace`](super::Workspace) alongside
//! its [`ScrollingSpace`](super::ScrollingSpace). The two share a monitor but
//! use **different coordinate spaces**: the scrolling space owns the tiling
//! math (an infinite horizontal canvas projected onto the work area), while
//! the floating space will eventually track per-window pixel rectangles
//! directly. This stub exists so the workspace hierarchy has the right shape
//! from day one; the real floating logic lands in a later session once the
//! workspace animation design settles.

/// Space for floating (non-tiled) windows within a [`Workspace`](super::Workspace).
///
/// Currently a stub — floating windows are tracked by
/// [`WindowRegistry`](crate::registry::WindowRegistry) but their positioning
/// is left to the OS. Future work may add:
///
/// - Smart placement (cascade, tile remaining, center)
/// - Stacking order management (z-order)
/// - Gap/avoidance management between floating and tiling windows
///
/// Unlike the scrolling space, the floating space does **not** run windows
/// through the virtual/actual projection pipeline. Each floating window keeps
/// the on-screen rectangle the user dragged it to; the space just remembers
/// those rectangles so it can hide/show them as the workspace switches.
pub struct FloatingSpace;

impl FloatingSpace {
    /// Create a new floating space.
    ///
    /// No initialization is needed — the floating space acts as a
    /// coordination point between the registry's floating-window state
    /// and any future floating-specific logic.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for FloatingSpace {
    fn default() -> Self {
        Self::new()
    }
}
