//! Layout engine orchestrator.
//!
//! [`LayoutEngine`] is the main entry point for layout operations. It owns
//! the current [`VirtualLayout`], tracks focus and monocle state, and
//! produces [`LayoutDiff`] results for each mutation.
//!
//! # The Mutation Pipeline
//!
//! Every public method follows the same 3-step pipeline internally:
//!
//! ```text
//! User command (e.g., swap Right)
//!     │
//!     ▼
//! mutations::swap() → new VirtualLayout
//!     │
//!     ▼  (apply_mutation)
//! projection::project() → new ActualLayout
//!     │
//!     ▼
//! diff::diff(prev_actual, new_actual) → Vec<WindowMove>
//!     │
//!     ▼
//! LayoutDiff { virtual_layout, actual_layout, moves }
//! ```
//!
//! # What LayoutEngine Does NOT Own
//!
//! The engine is **pure layout logic**. It does not own:
//! - Window metadata (titles, classes, HWNDs) — that's `WindowRegistry`
//! - Tile/float/ignore state — that's `WindowRegistry`
//! - Animation timing — that's the compositor
//! - Config loading — that's [`config`](crate::config)
//!
//! It only owns the layout math: columns, widths, focus, viewport offset.

use crate::common::{Direction, WindowId};
use crate::layout::diff;
use crate::layout::mutations::{self, MutationConfig};
use crate::layout::projection;
use crate::layout::types::{ActualLayout, LayoutDiff, MonitorInfo, Padding, VirtualLayout};

/// The layout engine — owns virtual layout state, tracks focus, and produces diffs.
///
/// This is the single entry point for all layout operations. Each public method
/// applies a mutation (scroll, focus, swap, resize, etc.), runs the full
/// mutation pipeline, and returns a [`LayoutDiff`] describing what changed.
///
/// # Focus tracking
///
/// Focus is stored as `Option<WindowId>` — a stable window identifier, not a
/// position index. This means:
/// - Swapping columns does not require focus fixup.
/// - Removing a window falls back to the first available window.
/// - Focus moves with the window, not with the layout slot.
///
/// This is the single entry point for all layout mutations. Call its public
/// methods to scroll, focus, swap, resize, merge, toggle monocle, or
/// add/remove windows. Each call returns a [`LayoutDiff`] containing the
/// new layouts and animation instructions.
///
/// # Example
///
/// ```rust
/// use scrolling_tiling_manager::layout::{
///     LayoutEngine,
///     types::{MonitorInfo, Padding},
/// };
/// use scrolling_tiling_manager::common::{Rect, WindowId, Direction};
///
/// let monitor = MonitorInfo {
///     work_area: Rect { x: 0, y: 0, width: 1920, height: 1080 },
/// };
/// let mut engine = LayoutEngine::new(monitor, 960, 4, Padding { window: 4, up: 0, down: 0 });
///
/// // Add windows (each becomes a new column, auto-focused)
/// engine.add_window(WindowId(1));
/// engine.add_window(WindowId(2));
/// engine.add_window(WindowId(3));
///
/// // Focus is on WindowId(3) (last added). Focus left to WindowId(2).
/// let focused = engine.focus(Direction::Left);
/// assert_eq!(focused, Some(WindowId(2)));
/// ```
pub struct LayoutEngine {
    /// Current virtual layout (the infinite horizontal canvas).
    virtual_layout: VirtualLayout,
    /// Currently focused window.
    focused: Option<WindowId>,
    /// Previous actual layout (used for diff computation).
    prev_actual: ActualLayout,
    /// The monitor being managed.
    monitor: MonitorInfo,
    /// Configuration derived from `StmConfig`.
    config: MutationConfig,
    /// Saved column width before monocle mode (per column index).
    monocle_saved_width: Option<(usize, u8)>,
}

impl LayoutEngine {
    /// Create a new layout engine for the given monitor.
    #[must_use]
    pub fn new(
        monitor: MonitorInfo,
        column_width: u32,
        default_column_width_eighths: u8,
        padding: Padding,
    ) -> Self {
        let virtual_layout = VirtualLayout::new();
        let config = MutationConfig {
            monitor_width: monitor.work_area.width,
            column_width,
            default_column_width_eighths,
            padding,
        };
        let prev_actual = projection::project(
            &virtual_layout,
            &monitor,
            config.column_width,
            &config.padding,
        );

        Self {
            virtual_layout,
            focused: None,
            prev_actual,
            monitor,
            config,
            monocle_saved_width: None,
        }
    }

    /// Get a reference to the current virtual layout.
    #[must_use]
    pub fn virtual_layout(&self) -> &VirtualLayout {
        &self.virtual_layout
    }

    /// Get the currently focused window.
    #[must_use]
    pub fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    /// Get a reference to the monitor info.
    #[must_use]
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// Apply a mutation and compute the resulting [`LayoutDiff`].
    ///
    /// This is the core pipeline that every mutation flows through:
    /// 1. Receive new [`VirtualLayout`] from the mutation function.
    /// 2. [`projection::project`] → camera shift + parking → new [`ActualLayout`].
    /// 3. [`diff::diff`] old vs new actual → `Vec<WindowMove>` (animation instructions).
    /// 4. Update internal state and return the diff.
    fn apply_mutation(&mut self, new_layout: VirtualLayout) -> LayoutDiff {
        let new_actual = projection::project(
            &new_layout,
            &self.monitor,
            self.config.column_width,
            &self.config.padding,
        );
        let moves = diff::diff(&self.prev_actual, &new_actual);

        let result = LayoutDiff {
            virtual_layout: new_layout,
            actual_layout: new_actual.clone(),
            moves,
        };

        self.virtual_layout = result.virtual_layout.clone();
        self.prev_actual = new_actual;
        result
    }

    // -----------------------------------------------------------------------
    // Scroll operations
    // -----------------------------------------------------------------------

    /// Scroll the viewport left by one column step.
    pub fn scroll_left(&mut self) -> Option<LayoutDiff> {
        let new_layout = mutations::scroll_left(&self.virtual_layout, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Scroll the viewport right by one column step.
    pub fn scroll_right(&mut self) -> Option<LayoutDiff> {
        let new_layout = mutations::scroll_right(&self.virtual_layout, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Focus operations
    // -----------------------------------------------------------------------

    /// Move focus in the given direction, shifting the camera if the target
    /// is off-screen.
    ///
    /// Focus is tracked by [`WindowId`] — if the target column requires a
    /// camera shift, the viewport scrolls automatically. No focus fixup is
    /// needed because the window ID is stable regardless of layout changes.
    pub fn focus(&mut self, direction: Direction) -> Option<WindowId> {
        let focused = self.focused?;
        let result = mutations::focus(&self.virtual_layout, focused, direction, &self.config)?;
        self.focused = Some(result.focused);
        if result.new_layout.viewport_offset != self.virtual_layout.viewport_offset {
            self.apply_mutation(result.new_layout);
        }
        Some(result.focused)
    }

    /// Set the focused window explicitly by [`WindowId`].
    ///
    /// No validation is performed — the caller should ensure the window exists
    /// in the layout. This does not produce a [`LayoutDiff`]; it only updates
    /// internal focus state.
    pub fn set_focus(&mut self, window: WindowId) {
        self.focused = Some(window);
    }

    // -----------------------------------------------------------------------
    // Swap operations
    // -----------------------------------------------------------------------

    /// Swap the focused window in the given direction.
    ///
    /// For Left/Right, swaps the focused column with its neighbor.
    /// For Up/Down, swaps the focused row within its column.
    /// Focus requires no fixup — it follows the window by [`WindowId`].
    pub fn swap(&mut self, direction: Direction) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let new_layout = mutations::swap(&self.virtual_layout, focused, direction)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Swap the focused column with the first off-screen column, then
    /// shift the camera to reveal it.
    ///
    /// Full flow: swap columns in [`VirtualLayout`] → call
    /// [`mutations::swap_with_offscreen`] to shift the camera → project
    /// and diff. Focus stays on the same [`WindowId`] throughout.
    pub fn swap_with_offscreen(&mut self, direction: Direction) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let new_layout =
            mutations::swap_with_offscreen(&self.virtual_layout, focused, direction, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Resize operations
    // -----------------------------------------------------------------------

    /// Expand the focused column by 1 eighth.
    pub fn expand_column(&mut self, direction: Direction) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let new_layout = mutations::expand_column(&self.virtual_layout, focused, direction)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Shrink the focused column by 1 eighth.
    pub fn shrink_column(&mut self, direction: Direction) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let new_layout = mutations::shrink_column(&self.virtual_layout, focused, direction)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Set the focused column width explicitly.
    pub fn set_column_width(&mut self, eighths: u8) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let new_layout =
            mutations::set_column_width(&self.virtual_layout, focused, eighths, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Merge operations
    // -----------------------------------------------------------------------

    /// Merge the focused column with its left neighbor.
    pub fn merge_column_left(&mut self) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let new_layout = mutations::merge_column_left(&self.virtual_layout, focused)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Merge the focused column with its right neighbor.
    pub fn merge_column_right(&mut self) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let new_layout = mutations::merge_column_right(&self.virtual_layout, focused)?;
        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Monocle toggle
    // -----------------------------------------------------------------------

    /// Toggle monocle mode for the focused window.
    pub fn toggle_monocle(&mut self) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let saved = self.monocle_saved_width.and_then(|(col, w)| {
            // Only use saved width if it's for the same column
            if self.virtual_layout.find_window(focused).map(|(c, _)| c) == Some(col) {
                Some(w)
            } else {
                None
            }
        });
        let (new_layout, new_saved) =
            mutations::toggle_monocle(&self.virtual_layout, focused, saved)?;

        if let Some(w) = new_saved {
            let col = self.virtual_layout.find_window(focused).map(|(c, _)| c)?;
            self.monocle_saved_width = Some((col, w));
        } else {
            self.monocle_saved_width = None;
        }

        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Window lifecycle
    // -----------------------------------------------------------------------

    /// Add a window as a new column appended to the right.
    pub fn add_window(&mut self, window: WindowId) -> LayoutDiff {
        let new_layout = mutations::add_window(&self.virtual_layout, window, &self.config);
        self.focused = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Add a window as a new row in the focused column.
    pub fn add_window_to_focused_column(&mut self, window: WindowId) -> Option<LayoutDiff> {
        let focused = self.focused?;
        let (col, _) = self.virtual_layout.find_window(focused)?;
        let new_layout = mutations::add_window_to_column(&self.virtual_layout, col, window);
        self.focused = Some(window);
        Some(self.apply_mutation(new_layout))
    }

    /// Remove a window from the layout.
    pub fn remove_window(&mut self, window: WindowId) -> LayoutDiff {
        let new_layout = mutations::remove_window(&self.virtual_layout, window, &self.config);

        // Update focus
        if self.focused == Some(window) {
            self.focused = self
                .virtual_layout
                .columns
                .first()
                .and_then(|c| c.rows.first().copied());
        }

        self.apply_mutation(new_layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Rect;

    fn test_monitor() -> MonitorInfo {
        MonitorInfo {
            work_area: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        }
    }

    fn test_padding() -> Padding {
        Padding {
            window: 4,
            up: 0,
            down: 0,
        }
    }

    fn engine_with_three_columns() -> LayoutEngine {
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));
        engine
    }

    #[test]
    fn engine_add_windows_and_focus() {
        let engine = engine_with_three_columns();
        assert_eq!(engine.focused(), Some(WindowId(1)));
        assert_eq!(engine.virtual_layout().columns.len(), 3);
        assert_eq!(engine.virtual_layout().window_count(), 3);
    }

    #[test]
    fn engine_focus_moves() {
        let mut engine = engine_with_three_columns();
        let new_focus = engine.focus(Direction::Right).expect("focus right");
        assert_eq!(new_focus, WindowId(2));
        assert_eq!(engine.focused(), Some(WindowId(2)));
    }

    #[test]
    fn engine_swap_columns() {
        let mut engine = engine_with_three_columns();
        let diff = engine.swap(Direction::Right).expect("swap");
        assert_eq!(diff.virtual_layout.columns[0].rows[0], WindowId(2));
        assert_eq!(diff.virtual_layout.columns[1].rows[0], WindowId(1));
        assert!(!diff.moves.is_empty());
    }

    #[test]
    fn engine_expand_column() {
        let mut engine = engine_with_three_columns();
        let diff = engine.expand_column(Direction::Right).expect("expand");
        assert_eq!(diff.virtual_layout.columns[0].width_eighths, 5);
        assert_eq!(diff.virtual_layout.columns[1].width_eighths, 3);
    }

    #[test]
    fn engine_scroll_right_then_left() {
        let mut engine = engine_with_three_columns();
        let diff_r = engine.scroll_right().expect("scroll right");
        assert!(diff_r.virtual_layout.viewport_offset > 0);

        let diff_l = engine.scroll_left().expect("scroll left");
        assert_eq!(diff_l.virtual_layout.viewport_offset, 0);
    }

    #[test]
    fn engine_remove_window_updates_focus() {
        let mut engine = engine_with_three_columns();
        engine.set_focus(WindowId(2));
        let _ = engine.remove_window(WindowId(2));
        // Focus should fall back to first window of first column
        assert!(engine.focused().is_some());
    }

    #[test]
    fn engine_merge_columns() {
        let mut engine = engine_with_three_columns();
        let diff = engine.merge_column_right().expect("merge");
        assert_eq!(diff.virtual_layout.columns.len(), 2);
        assert_eq!(diff.virtual_layout.columns[0].rows.len(), 2);
    }

    #[test]
    fn engine_monocle_toggle() {
        let mut engine = engine_with_three_columns();
        let diff_on = engine.toggle_monocle().expect("monocle on");
        assert_eq!(diff_on.virtual_layout.columns[0].width_eighths, 8);

        let diff_off = engine.toggle_monocle().expect("monocle off");
        assert_eq!(diff_off.virtual_layout.columns[0].width_eighths, 4);
    }

    #[test]
    fn engine_add_remove_roundtrip() {
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        assert_eq!(engine.virtual_layout().window_count(), 2);

        let _ = engine.remove_window(WindowId(2));
        assert_eq!(engine.virtual_layout().window_count(), 1);
        assert_eq!(engine.virtual_layout().columns.len(), 1);

        let _ = engine.remove_window(WindowId(1));
        assert_eq!(engine.virtual_layout().window_count(), 0);
        assert!(engine.virtual_layout().columns.is_empty());
    }

    // --- Integration: Engine lifecycle tests ---

    #[test]
    fn engine_full_lifecycle() {
        // Positive: empty → add 3 → mutate → remove all → verify state at each step
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());

        // Step 1: Add windows
        let d1 = engine.add_window(WindowId(1));
        assert_eq!(engine.focused(), Some(WindowId(1)));
        assert_eq!(d1.actual_layout.entries.len(), 1);
        assert_eq!(d1.moves.len(), 1); // 1 new window from parked

        let d2 = engine.add_window(WindowId(2));
        assert_eq!(engine.focused(), Some(WindowId(2)));
        assert_eq!(d2.actual_layout.entries.len(), 2);

        let d3 = engine.add_window(WindowId(3));
        assert_eq!(engine.virtual_layout().columns.len(), 3);
        assert_eq!(d3.moves.len(), 1); // only the new window moved

        // Step 2: Mutate — swap columns 0 and 1
        engine.set_focus(WindowId(1));
        let d_swap = engine.swap(Direction::Right).expect("swap");
        assert_eq!(d_swap.virtual_layout.columns[0].rows[0], WindowId(2));
        assert_eq!(d_swap.virtual_layout.columns[1].rows[0], WindowId(1));
        assert!(!d_swap.moves.is_empty());

        // Step 3: Mutate — expand column
        engine.set_focus(WindowId(1));
        let d_expand = engine.expand_column(Direction::Left).expect("expand");
        assert_eq!(d_expand.virtual_layout.columns[1].width_eighths, 5);
        assert_eq!(d_expand.virtual_layout.columns[0].width_eighths, 3);

        // Step 4: Remove windows one by one
        let _ = engine.remove_window(WindowId(1));
        assert_eq!(engine.virtual_layout().columns.len(), 2);

        let _ = engine.remove_window(WindowId(2));
        assert_eq!(engine.virtual_layout().columns.len(), 1);

        let _ = engine.remove_window(WindowId(3));
        assert_eq!(engine.virtual_layout().columns.len(), 0);
        assert_eq!(engine.virtual_layout().window_count(), 0);
    }

    #[test]
    fn engine_focus_triggers_viewport_scroll() {
        // Positive: focus into off-screen column triggers viewport scroll
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4)); // 4 columns × 4/8 each = 2× viewport width
        engine.set_focus(WindowId(1));

        // Focus right twice: col1→col2 (visible), col2→col3 (triggers scroll)
        let f1 = engine.focus(Direction::Right).expect("focus right 1");
        assert_eq!(f1, WindowId(2));
        // After first focus (visible), viewport_offset unchanged
        let offset_after_first = engine.virtual_layout().viewport_offset;

        let f2 = engine.focus(Direction::Right).expect("focus right 2");
        assert_eq!(f2, WindowId(3));
        assert!(
            engine.virtual_layout().viewport_offset > offset_after_first,
            "viewport should have scrolled to show col 3 (was {offset_after_first}, now {})",
            engine.virtual_layout().viewport_offset
        );
    }

    #[test]
    fn engine_single_window_all_operations() {
        // Positive: single window — swap, expand/shrink return None appropriately
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));

        // Swap left → None (no column to left)
        assert!(engine.swap(Direction::Left).is_none());
        // Swap right → None (no column to right)
        assert!(engine.swap(Direction::Right).is_none());
        // Swap up → None (only row)
        assert!(engine.swap(Direction::Up).is_none());
        // Swap down → None (only row)
        assert!(engine.swap(Direction::Down).is_none());

        // Expand/shrink — needs neighbor
        assert!(engine.expand_column(Direction::Right).is_none());
        assert!(engine.shrink_column(Direction::Right).is_none());

        // Merge — needs neighbor
        assert!(engine.merge_column_left().is_none());
        assert!(engine.merge_column_right().is_none());

        // Focus vertical — only one row
        assert!(engine.focus(Direction::Up).is_none());
        assert!(engine.focus(Direction::Down).is_none());

        // Monocle still works (single column)
        let diff = engine.toggle_monocle().expect("monocle");
        assert_eq!(diff.virtual_layout.columns[0].width_eighths, 8);
    }

    #[test]
    fn engine_empty_operations_return_none() {
        // Negative: all operations on empty engine return None or produce empty diffs
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());

        assert!(engine.scroll_left().is_none());
        assert!(engine.scroll_right().is_none());
        assert!(engine.focus(Direction::Right).is_none());
        assert!(engine.swap(Direction::Right).is_none());
        assert!(engine.expand_column(Direction::Right).is_none());
        assert!(engine.shrink_column(Direction::Right).is_none());
        assert!(engine.merge_column_left().is_none());
        assert!(engine.merge_column_right().is_none());
        assert!(engine.toggle_monocle().is_none());
        assert!(engine.swap_with_offscreen(Direction::Right).is_none());
    }

    #[test]
    fn engine_add_window_to_focused_column() {
        // Positive: add window as row to focused column
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));
        let diff = engine
            .add_window_to_focused_column(WindowId(2))
            .expect("add to focused col");
        assert_eq!(engine.virtual_layout().columns.len(), 1); // still one column
        assert_eq!(engine.virtual_layout().columns[0].rows.len(), 2);
        // Both windows' positions changed (existing window shrunk, new window appeared)
        assert_eq!(diff.moves.len(), 2);
    }

    #[test]
    fn engine_add_to_focused_column_no_focus_returns_none() {
        // Negative: no focus → can't add to focused column
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        assert!(engine.add_window_to_focused_column(WindowId(1)).is_none());
    }

    #[test]
    fn engine_monocle_then_add_window() {
        // Positive: monocle on, add window (new column), toggle off on same column
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));

        let d_on = engine.toggle_monocle().expect("monocle on");
        assert_eq!(d_on.virtual_layout.columns[0].width_eighths, 8);

        // Add window → new column, focus moves to new window
        engine.add_window(WindowId(2));
        assert_eq!(engine.virtual_layout().columns.len(), 2);

        // Focus back to column 0 and toggle monocle off
        engine.set_focus(WindowId(1));
        let d_off = engine.toggle_monocle().expect("monocle off");
        assert_eq!(d_off.virtual_layout.columns[0].width_eighths, 4);
    }

    #[test]
    fn engine_expand_shrink_produces_pixel_diffs() {
        // Positive: expand → actual layout has different pixel sizes
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));

        // Focus WindowId(1) in column 0, expand right (shrinks column 1)
        engine.set_focus(WindowId(1));
        let diff = engine.expand_column(Direction::Right).expect("expand");
        // The diff must contain moves (pixel positions changed)
        assert!(!diff.moves.is_empty(), "expand should produce window moves");
        assert_eq!(diff.virtual_layout.columns[0].width_eighths, 5);
        assert_eq!(diff.virtual_layout.columns[1].width_eighths, 3);
    }

    #[test]
    fn engine_swap_with_offscreen() {
        // Positive: swap focused column with off-screen column
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.set_focus(WindowId(1));

        // Focus right twice to scroll, then swap with offscreen left
        let _ = engine.focus(Direction::Right).expect("f1");
        let _ = engine.focus(Direction::Right).expect("f2");
        assert!(engine.virtual_layout().viewport_offset > 0);

        let diff = engine.swap_with_offscreen(Direction::Left).expect("swap");
        assert!(!diff.moves.is_empty());
    }

    #[test]
    fn engine_scroll_right_at_boundary() {
        // Negative: can't scroll past rightmost column
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1)); // single 4/8 column

        assert!(engine.scroll_right().is_none());
    }

    #[test]
    fn engine_set_column_width() {
        // Positive: explicit column width setting
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));

        // Focus WindowId(1) in column 0, set width to 6 (neighbor col 1 shrinks to 2)
        engine.set_focus(WindowId(1));
        let diff = engine.set_column_width(6).expect("set width");
        assert_eq!(diff.virtual_layout.columns[0].width_eighths, 6);
        // Neighbor should be compensated
        assert_eq!(diff.virtual_layout.columns[1].width_eighths, 2);
    }

    #[test]
    fn engine_remove_focused_window_focus_falls_back() {
        // Positive: removing focused window falls back to first available
        let mut engine = LayoutEngine::new(test_monitor(), 960, 4, test_padding());
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(2));

        let _ = engine.remove_window(WindowId(2));
        // Focus should be some remaining window (first of first column)
        assert!(engine.focused().is_some());
        assert_ne!(engine.focused(), Some(WindowId(2)));
    }

    #[test]
    fn engine_focus_no_change_in_layout_when_visible() {
        // Positive: focus within fully visible area → no viewport scroll needed.
        // With a single full-width column (8/8), focus has nowhere to go horizontally,
        // but vertical focus doesn't trigger scroll. This test verifies vertical focus
        // doesn't change the viewport offset.
        let mut engine = LayoutEngine::new(test_monitor(), 1920, 8, test_padding());
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        engine.set_focus(WindowId(1));

        let prev_offset = engine.virtual_layout().viewport_offset;
        let _ = engine.focus(Direction::Down).expect("focus down");
        // Vertical focus within a fully visible column → no scroll
        assert_eq!(engine.virtual_layout().viewport_offset, prev_offset);
    }
}
