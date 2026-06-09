//! Floating window manager — stub for future implementation.
//!
//! Floating windows are tracked by [`WindowRegistry`](crate::registry::WindowRegistry)
//! but not managed by the layout engine. This module is a placeholder for future
//! floating window management features such as smart placement, stacking order,
//! and gap management.

/// Manager for floating (non-tiled) windows.
///
/// Currently a stub — floating windows are tracked by
/// [`WindowRegistry`](crate::registry::WindowRegistry) but their positioning
/// is left to the OS. Future work may add:
///
/// - Smart placement (cascade, tile remaining, center)
/// - Stacking order management (z-order)
/// - Gap/avoidance management between floating and tiling windows
pub struct FloatingManager;

impl FloatingManager {
    /// Create a new floating manager.
    ///
    /// No initialization is needed — the floating manager acts as a
    /// coordination point between the registry's floating-window state
    /// and any future floating-specific logic.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for FloatingManager {
    fn default() -> Self {
        Self::new()
    }
}
