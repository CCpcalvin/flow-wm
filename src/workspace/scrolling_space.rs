//! Scrolling space — the infinite-horizontal-canvas tiling engine.
//!
//! [`ScrollingSpace`] is the main entry point for layout operations. It owns the
//! current [`VirtualLayout`], tracks focus and monocle state, and produces
//! [`AppliedLayout`] results for each mutation.
//!
//! # Where this lives
//!
//! A [`ScrollingSpace`] is the tiling half of a [`Workspace`](super::Workspace)
//! (the other half being a [`FloatingSpace`](super::FloatingSpace)). Many
//! workspaces can share a single monitor, but only the active one is visible;
//! the rest are packed "above" and "below" — the same packing idea the scrolling
//! space uses horizontally between columns, now applied vertically between
//! workspaces. This module is **pure layout math**: it never touches Win32 and
//! knows nothing of HWNDs, only [`WindowId`]s.
//!
//! # The mutation pipeline
//!
//! Every public method runs the same pipeline internally:
//! `mutations::*` (pure) → `apply_mutation` → `projection::project` →
//! [`AppliedLayout`]. The animation layer then diffs that against real on-screen
//! positions to decide what to animate (see [`AppliedLayout`]).
//!
//! # What ScrollingSpace does not own
//!
//! The scrolling space is **pure layout logic**. It does not own window
//! metadata or tile/float/ignore state (that's
//! [`WindowRegistry`](crate::registry::WindowRegistry)), animation timing (that's
//! the [`animation`](crate::animation) layer), or config loading (that's
//! [`config`](crate::config)). It only owns columns, widths, focus, and the
//! viewport offset.
//!
//! See the developer guide's *Mutation Pipeline* chapter
//! (`docs/src/dev-guide/layout/pipeline.md`) for the full flow with a worked example.

use crate::common::{Direction, WindowId};
use crate::layout::mutations::{self, MutationConfig};
use crate::layout::projection;
use crate::layout::types::{ActualLayout, AppliedLayout, MonitorInfo, Padding, VirtualLayout};

/// The scrolling space — owns virtual layout state, tracks focus, and produces applied layouts.
///
/// Single entry point for all tiling operations. Each public method applies a
/// mutation (scroll, focus, swap, resize, toggle monocle, add/remove window),
/// runs the mutation pipeline, and returns an [`AppliedLayout`].
///
/// # Focus tracking
///
/// Focus is stored as `Option<WindowId>` — a stable window identifier, not a
/// slot index — so swapping columns needs no focus fixup and focus follows the
/// window when the layout changes. Removing the focused window falls back to
/// the first available window.
///
/// # Example
///
/// ```rust
/// use scrolling_tiling_manager::workspace::ScrollingSpace;
/// use scrolling_tiling_manager::layout::types::{MonitorInfo, Padding};
/// use scrolling_tiling_manager::common::{Rect, WindowId, Direction};
///
/// let monitor = MonitorInfo {
///     work_area: Rect { x: 0, y: 0, width: 1920, height: 1080 },
/// };
/// let mut engine = ScrollingSpace::new(monitor, 960, 4, Padding { window_gap: 4, up: 0, down: 0 }, 4);
///
/// // Add windows (each becomes a new column, auto-focused)
/// engine.add_window(WindowId(1));
/// engine.add_window(WindowId(2));
/// engine.add_window(WindowId(3));
///
/// // Focus is on WindowId(3) (last added). Focus left to WindowId(2).
/// let (focused, _viewport_diff) = engine.focus(Direction::Left).unwrap();
/// assert_eq!(focused, WindowId(2));
/// ```
pub struct ScrollingSpace {
    /// Current virtual layout (the infinite horizontal canvas).
    virtual_layout: VirtualLayout,
    /// Per-space history cursor: the most recently interacted-with window in this space.
    ///
    /// This is NOT OS-level focus (owned by the registry via `set_focused`).
    /// See (`docs/src/dev-guide/workspace.md`).
    last_focused_window: Option<WindowId>,
    /// Last projected actual layout (cached for the `actual_layout()` accessor).
    actual: ActualLayout,
    /// The monitor being managed.
    monitor: MonitorInfo,
    /// Configuration derived from `StmConfig`.
    config: MutationConfig,
    /// Saved column width before monocle mode (per column index), in pixels.
    monocle_saved_width: Option<(usize, i32)>,
}

impl ScrollingSpace {
    /// Create a new scrolling space for the given monitor.
    ///
    /// `column_width` is the base column width in pixels (the `n = 0` rung of
    /// the expand/shrink slot ladder). `min_column_width_px` is the lower
    /// bound for free-form drag-resize. The slot-ladder derived values
    /// (`max_n`, `abs_max_width`) are computed here from `column_width`, the
    /// monitor width, and `window_gap`, then stored in the space's
    /// [`MutationConfig`] — every mutation reuses the same ladder.
    ///
    /// # Ladder derivation
    ///
    /// - `abs_max_width = monitor_width − 2*window_gap` (the monocle width and
    ///   the two-step top of the expand ladder).
    /// - `column_shift = column_width + window_gap` (one slot step).
    /// - `max_n = floor((abs_max_width − column_width) / column_shift)` — how
    ///   many full slot steps fit between the base width and `abs_max_width`.
    ///   Clamped to `≥ 0` for degenerate configs (column wider than monitor).
    #[must_use]
    pub fn new(
        monitor: MonitorInfo,
        column_width: u32,
        min_column_width_px: u32,
        padding: Padding,
        columns_per_screen: u32,
    ) -> Self {
        let gap = padding.window_gap;
        let cw = column_width as i32;
        let monitor_w = monitor.work_area.width;
        // abs_max_width = monitor_width − 2*gap (monocle width / ladder top).
        let abs_max_width = monitor_w - 2 * gap;
        // column_shift = column_width + window_gap — one slot step.
        let shift = cw + gap;
        // max_n: full slot steps between base width and abs_max_width.
        let max_n: u32 = if shift > 0 && abs_max_width > cw {
            ((abs_max_width - cw) / shift) as u32
        } else {
            0
        };

        let virtual_layout = VirtualLayout::new();
        let config = MutationConfig {
            monitor_width: monitor_w,
            column_width,
            min_column_width_px,
            max_n,
            abs_max_width,
            padding,
            columns_per_screen,
        };
        let actual = projection::project(&virtual_layout, &monitor, &config.padding);

        Self {
            virtual_layout,
            last_focused_window: None,
            actual,
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

    /// Get a reference to the current actual layout (projected screen coords).
    ///
    /// This is the last layout produced by [`apply_mutation`](Self::apply_mutation).
    /// It contains the current pixel-accurate rectangles for every window.
    #[must_use]
    pub fn actual_layout(&self) -> &ActualLayout {
        &self.actual
    }

    /// Get the per-space last-focused window (history cursor, NOT OS focus).
    ///
    /// See (`docs/src/dev-guide/workspace.md`).
    #[must_use]
    pub fn last_focused_window(&self) -> Option<WindowId> {
        self.last_focused_window
    }

    /// Get a reference to the monitor info.
    #[must_use]
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// Get the configured [`Padding`].
    ///
    /// The workspace-op dispatch handlers use the cached `window_gap` to
    /// compute the vertical parking unit (`monitor_height + window_gap`) for
    /// [`workspace_y_offset`](crate::workspace::workspace_y_offset). All
    /// workspaces on a given monitor share the same padding — the daemon
    /// derives a single [`Padding`](crate::layout::types::Padding) at
    /// construction and threads it into every workspace's
    /// [`ScrollingSpace`].
    #[must_use]
    pub fn padding(&self) -> Padding {
        self.config.padding
    }

    /// Get the configured `columns_per_screen` threshold.
    ///
    /// Used by dispatch hooks (e.g. auto-center after a workspace move) to
    /// decide whether the destination grid is sparse enough to recenter.
    /// All workspaces on a monitor share this value — see [`Self::padding`]
    /// for the sharing rationale.
    #[must_use]
    pub fn columns_per_screen(&self) -> u32 {
        self.config.columns_per_screen
    }

    /// Access the mutation config for dispatch-level operations.
    ///
    /// Needed by daemon dispatch for `canvas_width` and `ensure_column_visible`
    /// calls that operate on the post-insert layout.
    #[must_use]
    pub(crate) fn config(&self) -> &MutationConfig {
        &self.config
    }

    /// Apply a mutation and compute the resulting [`AppliedLayout`].
    ///
    /// This is the core pipeline that every mutation flows through:
    /// 1. Receive new [`VirtualLayout`] from the mutation function.
    /// 2. [`projection::project`] → camera shift + parking → new [`ActualLayout`].
    /// 3. Update internal state and return the new layout.
    ///
    /// This method returns the full [`ActualLayout`] and lets the animation
    /// layer decide which windows to animate by comparing each target rect
    /// against the window's real on-screen position.
    fn apply_mutation(&mut self, new_layout: VirtualLayout) -> AppliedLayout {
        let new_actual = projection::project(&new_layout, &self.monitor, &self.config.padding);

        let result = AppliedLayout {
            actual_layout: new_actual.clone(),
            virtual_layout: new_layout,
        };

        self.virtual_layout = result.virtual_layout.clone();
        self.actual = new_actual;
        result
    }

    // -----------------------------------------------------------------------
    // Scroll operations
    // -----------------------------------------------------------------------

    /// Scroll the viewport left by one column step.
    pub fn scroll_left(&mut self) -> Option<AppliedLayout> {
        let new_layout = mutations::scroll_left(&self.virtual_layout, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Scroll the viewport right by one column step.
    pub fn scroll_right(&mut self) -> Option<AppliedLayout> {
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
    ///
    /// # Returns
    ///
    /// A tuple of `(focused_window_id, optional_layout_diff)`:
    /// - `focused_window_id` — the newly focused window.
    /// - `Option<AppliedLayout>` — `Some` if the viewport scrolled (animation needed),
    ///   `None` if the target was already visible (no position changes).
    ///
    /// The caller **must** pass the `AppliedLayout` to the animation system when present.
    /// Failure to do so causes window positions to desync from the layout state,
    /// producing incorrect diffs on subsequent mutations.
    pub fn focus(&mut self, direction: Direction) -> Option<(WindowId, Option<AppliedLayout>)> {
        let focused = self.last_focused_window?;
        let result = mutations::focus(&self.virtual_layout, focused, direction, &self.config)?;
        self.last_focused_window = Some(result.focused);

        let diff = if result.new_layout.viewport_offset != self.virtual_layout.viewport_offset {
            Some(self.apply_mutation(result.new_layout))
        } else {
            None
        };

        Some((result.focused, diff))
    }

    /// Set the focused window explicitly by [`WindowId`].
    ///
    /// No validation is performed — the caller should ensure the window exists
    /// in the layout. This does not produce a [`AppliedLayout`]; it only updates
    /// internal focus state.
    pub fn set_focus(&mut self, window: WindowId) {
        self.last_focused_window = Some(window);
    }

    // -----------------------------------------------------------------------
    // Swap operations
    // -----------------------------------------------------------------------

    /// Swap the focused window's **column** with an adjacent column (Left/Right).
    ///
    /// Columns only have horizontal neighbours, so vertical directions
    /// (`Up` / `Down`) always return `None`. Use [`swap_window`](Self::swap_window)
    /// for per-window swaps in all four directions.
    ///
    /// Focus requires no fixup — it follows the window by [`WindowId`].
    pub fn swap_column(&mut self, direction: Direction) -> Option<AppliedLayout> {
        let focused = self.last_focused_window?;
        let new_layout =
            mutations::swap_column(&self.virtual_layout, focused, direction, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Swap the focused **window** with an adjacent individual window.
    ///
    /// Unlike [`swap_column`](Self::swap_column) which moves entire columns,
    /// this swaps two specific window IDs regardless of which columns they
    /// belong to. For Left/Right, the focused window is exchanged with the
    /// nearest window in the adjacent column. For Up/Down, it behaves like
    /// a row swap within the same column.
    ///
    /// Focus requires no fixup — it follows the window by [`WindowId`].
    pub fn swap_window(&mut self, direction: Direction) -> Option<AppliedLayout> {
        let focused = self.last_focused_window?;
        let new_layout =
            mutations::swap_window(&self.virtual_layout, focused, direction, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Resize operations
    // -----------------------------------------------------------------------

    /// Expand the focused column by one `column_shift` (= `column_width + window_gap`).
    ///
    /// No direction is needed — the resize is independent (no neighbor compensation).
    pub fn expand_column(&mut self) -> Option<AppliedLayout> {
        let focused = self.last_focused_window?;
        let new_layout = mutations::expand_column(&self.virtual_layout, focused, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Shrink the focused column by one `column_shift` (= `column_width + window_gap`).
    ///
    /// No direction is needed — the resize is independent (no neighbor compensation).
    pub fn shrink_column(&mut self) -> Option<AppliedLayout> {
        let focused = self.last_focused_window?;
        let new_layout = mutations::shrink_column(&self.virtual_layout, focused, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Set the focused column width to an explicit pixel value.
    pub fn set_column_width(&mut self, target_px: i32) -> Option<AppliedLayout> {
        let focused = self.last_focused_window?;
        let new_layout =
            mutations::set_column_width(&self.virtual_layout, focused, target_px, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Return the inclusive `[min, max]` bounds (in pixels) that an explicit
    /// column width must satisfy.
    ///
    /// This is the free-form range accepted by [`set_column_width`](Self::set_column_width)
    /// and the drag-resize primitive. IPC handlers use it to validate incoming
    /// width values *before* dispatching, so an out-of-range request gets a
    /// precise error message rather than being silently rejected by the space.
    #[must_use]
    pub fn column_width_bounds(&self) -> (i32, i32) {
        (
            self.config.min_column_width_px as i32,
            self.config.abs_max_width,
        )
    }

    // -----------------------------------------------------------------------
    // Monocle toggle
    // -----------------------------------------------------------------------

    /// Toggle monocle mode for the focused window.
    pub fn toggle_monocle(&mut self) -> Option<AppliedLayout> {
        let focused = self.last_focused_window?;
        // Resolve the focused column up front, before any mutation runs. If the
        // focused window isn't in the layout there's nothing to toggle, and
        // returning early here is safe because nothing has been computed yet.
        let focused_col = self.virtual_layout.find_window(focused).map(|(c, _)| c)?;
        let saved = self
            .monocle_saved_width
            .and_then(|(col, w)| if col == focused_col { Some(w) } else { None });
        let (new_layout, new_saved) =
            mutations::toggle_monocle(&self.virtual_layout, focused, saved, &self.config)?;

        // `focused_col` is still valid: `mutations::toggle_monocle` returns a
        // brand-new layout and does not mutate `self.virtual_layout`, and it
        // only succeeded because it located `focused`. Update the cache from
        // the already-resolved `focused_col` instead of re-looking-up with a
        // fragile `?` that could swallow the just-computed mutation.
        self.monocle_saved_width = new_saved.map(|w| (focused_col, w));

        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Window lifecycle
    // -----------------------------------------------------------------------

    /// Add a window as a new column appended to the right.
    pub fn add_window(&mut self, window: WindowId) -> AppliedLayout {
        let new_layout = mutations::add_window(&self.virtual_layout, window, &self.config);
        self.last_focused_window = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Insert a new window as a column immediately after the focused window.
    ///
    /// This is the **window-creation placement strategy**: when a new tiling
    /// window is detected, it is placed right next to the currently focused
    /// window rather than at the far right end of the layout. All columns to
    /// the right of the focused column shift rightward by one `column_shift`
    /// on the canvas. Focus then moves to the new window, and the viewport
    /// adjusts ([`mutations::ensure_column_visible`]) to keep it on screen.
    ///
    /// This runs the standard mutation pipeline (mutate → project),
    /// so the returned [`AppliedLayout`] is ready to pass straight to the
    /// animation system.
    ///
    /// # Edge cases
    ///
    /// - **No focused window** (first window in an empty layout): behaves like
    ///   [`add_window`](Self::add_window) — the new window becomes the sole
    ///   column.
    /// - **Stale focus**: if the focused window ID is no longer in the layout,
    ///   the new column is appended at the end.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use scrolling_tiling_manager::workspace::ScrollingSpace;
    /// # use scrolling_tiling_manager::layout::types::{MonitorInfo, Padding};
    /// # use scrolling_tiling_manager::common::WindowId;
    /// # let monitor = MonitorInfo {
    /// #     work_area: scrolling_tiling_manager::common::Rect { x: 0, y: 0, width: 1920, height: 1080 },
    /// # };
    /// # let mut engine = ScrollingSpace::new(monitor, 960, 4, Padding { window_gap: 4, up: 0, down: 0 }, 4);
    /// engine.add_window(WindowId(1)); // col 0, focused
    /// engine.add_window(WindowId(2)); // col 1, focused
    /// engine.set_focus(WindowId(1));  // focus back to col 0
    ///
    /// engine.insert_window(WindowId(3)); // inserted at col 1, between 1 and 2
    /// assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
    /// ```
    pub fn insert_window(&mut self, window: WindowId) -> AppliedLayout {
        let focused = self.last_focused_window;
        let new_layout = mutations::insert_window_after_focused(
            &self.virtual_layout,
            focused,
            window,
            &self.config,
        );
        self.last_focused_window = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Add a window as a new row in the focused column.
    pub fn add_window_to_focused_column(&mut self, window: WindowId) -> Option<AppliedLayout> {
        let focused = self.last_focused_window?;
        let (col, _) = self.virtual_layout.find_window(focused)?;
        let new_layout = mutations::add_window_to_column(&self.virtual_layout, col, window);
        self.last_focused_window = Some(window);
        Some(self.apply_mutation(new_layout))
    }

    /// Remove a window from the layout and update focus.
    ///
    /// This implements the **window-removal pipeline**:
    ///
    /// 1. **Locate** the window's `(col, row)` in the virtual layout (before
    ///    removal).
    /// 2. **Focus resolution** — if the removed window was the focused window,
    ///    pick a successor via [`mutations::next_available_window`] (left
    ///    column preferred, then right). Otherwise focus is unchanged.
    /// 3. **Remove** the window from the virtual layout. Because projection
    ///    computes canvas positions cumulatively (a running `canvas_x`
    ///    accumulator), deleting a column naturally shifts every window to its
    ///    right leftward by one slot width — no explicit viewport adjustment is
    ///    needed. This is the "natural compression" property.
    /// 4. **Ensure visibility** — if there is a focused window after removal,
    ///    scroll the viewport so its column is on-screen
    ///    ([`mutations::ensure_column_visible`]).
    /// 5. **Apply** — project the new layout to produce the [`AppliedLayout`]
    ///    (canvas positions for every window), which is passed directly to the
    ///    animation system.
    ///
    /// # Focus fallback
    ///
    /// When the focused window is removed, focus moves to the **left** adjacent
    /// column's closest-row window, or — if there is no left column — the
    /// **right** adjacent column's closest-row window. If the removed window
    /// was the only one in the layout, focus becomes `None`.
    ///
    /// # Arguments
    ///
    /// * `window` — the [`WindowId`] to remove.
    ///
    /// # Returns
    ///
    /// An [`AppliedLayout`] describing the new layout and all window positions,
    /// to be passed to the animation system.
    pub fn remove_window(&mut self, window: WindowId) -> AppliedLayout {
        // 1. Find the window's position before removal.
        let pos = self.virtual_layout.find_window(window);
        let was_focused = self.last_focused_window == Some(window);

        // 2. Resolve the new focus if the removed window was focused.
        //    `next_available_window` operates on the pre-removal layout, so it
        //    must run before step 3.
        let new_focused = if was_focused {
            pos.and_then(|(col, row)| {
                mutations::next_available_window(&self.virtual_layout, col, row)
            })
        } else {
            self.last_focused_window
        };

        // 3. Remove the window from the virtual layout.
        //    Natural compression: deleting a column shifts right-side windows
        //    left by their slot width during projection — no explicit shift.
        let mut new_layout = mutations::remove_window(&self.virtual_layout, window, &self.config);

        // Commit the new focus.
        self.last_focused_window = new_focused;

        // 4. Ensure the focused window's column is visible (if any focus remains).
        if let Some(focused) = new_focused
            && let Some((col, _)) = new_layout.find_window(focused)
        {
            new_layout = mutations::ensure_column_visible(&new_layout, col, &self.config);
        }

        // 5. Project + diff → animation instructions.
        self.apply_mutation(new_layout)
    }

    /// Initialize the layout with multiple windows in one operation.
    ///
    /// Creates one column per window. When `widths` is `Some`, each width is
    /// quantized to the nearest slot-ladder rung; when `None`, every column
    /// gets the base [`column_width`](MutationConfig::column_width). Produces a
    /// single [`AppliedLayout`] covering all windows. See
    /// (`docs/src/dev-guide/layout/mutations.md`).
    ///
    /// Focus is set to the window at `focus_col_idx`, falling back to the
    /// last window.
    pub fn initialize_windows(
        &mut self,
        ids: Vec<WindowId>,
        focus_col_idx: Option<usize>,
        widths: Option<&[u32]>,
    ) -> AppliedLayout {
        let new_layout = mutations::initialize_windows(&ids, &self.config, focus_col_idx, widths);
        // Focus the window at the focus column (the user's foreground window),
        // falling back to the last window when no focus column was specified.
        self.last_focused_window = focus_col_idx
            .and_then(|idx| ids.get(idx).copied())
            .or_else(|| ids.last().copied());
        self.apply_mutation(new_layout)
    }

    // -----------------------------------------------------------------------
    // Viewport center operations (prefix-sum, variable-width aware)
    // -----------------------------------------------------------------------

    /// Center the viewport so the focused column lands at the monitor midpoint.
    ///
    /// Uses the actual prefix-sum canvas position (variable-width aware), not
    /// the slot grid. Always computes `canvas_x(focused) − (monitor_width −
    /// focused_width) / 2`, even when all columns fit — this is the explicit
    /// center command behavior. Returns `None` when the layout is empty. See
    /// (`docs/src/dev-guide/layout/mutations.md`).
    pub fn center_focused_column(&mut self) -> Option<AppliedLayout> {
        if self.virtual_layout.columns.is_empty() {
            return None;
        }
        let focus_col = self.focus_col_index();
        debug_assert!(
            focus_col < self.virtual_layout.columns.len(),
            "focus_col {focus_col} out of range"
        );
        let mut new_layout = self.virtual_layout.clone();
        new_layout.viewport_offset =
            mutations::center_viewport_on_focused(&self.virtual_layout, focus_col, &self.config);
        Some(self.apply_mutation(new_layout))
    }

    /// Center the viewport so the entire canvas is centered in the monitor.
    ///
    /// `(canvas_width − monitor_width) / 2` — may be negative when the canvas
    /// is narrower than the monitor. Used by init and MoveWindowToWorkspace
    /// fit cases. Returns `None` when the layout is empty. See
    /// (`docs/src/dev-guide/layout/mutations.md`).
    pub fn center_canvas(&mut self) -> Option<AppliedLayout> {
        if self.virtual_layout.columns.is_empty() {
            return None;
        }
        let mut new_layout = self.virtual_layout.clone();
        new_layout.viewport_offset =
            mutations::center_viewport_canvas(&self.virtual_layout, &self.config);
        Some(self.apply_mutation(new_layout))
    }

    /// Insert a new window as a column after the focused window, preserving
    /// the moved window's width.
    ///
    /// Like [`insert_window`](Self::insert_window) but the new column is
    /// created at `width_px` (quantized to the nearest slot-ladder rung)
    /// instead of the base [`column_width`](MutationConfig::column_width).
    /// This preserves a window's width when moving it between workspaces.
    /// See (`docs/src/dev-guide/layout/mutations.md`).
    pub fn insert_window_with_width(&mut self, window: WindowId, width_px: u32) -> AppliedLayout {
        let focused = self.last_focused_window;
        let width = mutations::quantize_to_ladder(width_px as i32, &self.config);
        let new_layout = mutations::insert_window_after_focused_with_width(
            &self.virtual_layout,
            focused,
            window,
            width,
            &self.config,
        );
        self.last_focused_window = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Shift the viewport so the focused column is visible with a gap margin.
    ///
    /// Delegates to [`mutations::ensure_column_visible`] on the focused
    /// column. Used by the MoveWindowToWorkspace overflow case to bring the
    /// moved window into view. See (`docs/src/dev-guide/layout/mutations.md`).
    pub(crate) fn ensure_focused_visible(&mut self) -> Option<AppliedLayout> {
        if self.virtual_layout.columns.is_empty() {
            return None;
        }
        let focus_col = self.focus_col_index();
        let prev_offset = self.virtual_layout.viewport_offset;
        let new_layout =
            mutations::ensure_column_visible(&self.virtual_layout, focus_col, &self.config);
        // Already visible — no scroll, no diff. This makes the method a natural
        // no-op for self-induced foreground changes re-entering via the hook.
        if new_layout.viewport_offset == prev_offset {
            return None;
        }
        Some(self.apply_mutation(new_layout))
    }

    /// Resolve the focused window's column index.
    ///
    /// Falls back to index 0 when no window is focused or the focused window
    /// is no longer in the layout (stale focus state).
    pub(crate) fn focus_col_index(&self) -> usize {
        self.last_focused_window
            .and_then(|w| self.virtual_layout.find_window(w).map(|(c, _)| c))
            .unwrap_or(0)
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
            window_gap: 4,
            up: 0,
            down: 0,
        }
    }

    fn engine_with_three_columns() -> ScrollingSpace {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));
        engine
    }

    #[test]
    fn engine_add_windows_and_focus() {
        let engine = engine_with_three_columns();
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
        assert_eq!(engine.virtual_layout().columns.len(), 3);
        assert_eq!(engine.virtual_layout().window_count(), 3);
    }

    #[test]
    fn engine_focus_moves() {
        let mut engine = engine_with_three_columns();
        let (new_focus, _diff) = engine.focus(Direction::Right).expect("focus right");
        assert_eq!(new_focus, WindowId(2));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    #[test]
    fn engine_swap_columns() {
        let mut engine = engine_with_three_columns();
        let diff = engine.swap_column(Direction::Right).expect("swap");
        assert_eq!(diff.virtual_layout.columns[0].rows[0], WindowId(2));
        assert_eq!(diff.virtual_layout.columns[1].rows[0], WindowId(1));
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_expand_column() {
        let mut engine = engine_with_three_columns();
        let diff = engine.expand_column().expect("expand");
        // 960px (base) + column_shift(964) = 1924px → clamped to abs_max 1912px
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1912);
        assert_eq!(diff.virtual_layout.columns[1].width_px, 960); // unchanged
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
        // Layout: [W1][W2][W3], focus on W2 (col 1).
        engine.set_focus(WindowId(2));
        let _ = engine.remove_window(WindowId(2));
        // Focus should fall back to the LEFT column's window (W1).
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
    }

    #[test]
    fn engine_monocle_toggle() {
        let mut engine = engine_with_three_columns();
        let diff_on = engine.toggle_monocle().expect("monocle on");
        assert_eq!(diff_on.virtual_layout.columns[0].width_px, 1912);

        let diff_off = engine.toggle_monocle().expect("monocle off");
        assert_eq!(diff_off.virtual_layout.columns[0].width_px, 960);
    }

    #[test]
    fn engine_add_remove_roundtrip() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
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
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);

        // Step 1: Add windows
        let d1 = engine.add_window(WindowId(1));
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
        assert_eq!(d1.actual_layout.entries.len(), 1);

        let d2 = engine.add_window(WindowId(2));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
        assert_eq!(d2.actual_layout.entries.len(), 2);

        let d3 = engine.add_window(WindowId(3));
        assert_eq!(engine.virtual_layout().columns.len(), 3);
        assert_eq!(d3.actual_layout.entries.len(), 3);

        // Step 2: Mutate — swap columns 0 and 1
        engine.set_focus(WindowId(1));
        let d_swap = engine.swap_column(Direction::Right).expect("swap");
        assert_eq!(d_swap.virtual_layout.columns[0].rows[0], WindowId(2));
        assert_eq!(d_swap.virtual_layout.columns[1].rows[0], WindowId(1));
        assert!(!d_swap.actual_layout.entries.is_empty());

        // Step 3: Mutate — expand column (independent, no compensation)
        engine.set_focus(WindowId(1));
        let d_expand = engine.expand_column().expect("expand");
        // 960px + column_shift(964) = 1924 → clamped to abs_max 1912px, other columns unchanged
        assert_eq!(d_expand.virtual_layout.columns[1].width_px, 1912);
        assert_eq!(d_expand.virtual_layout.columns[0].width_px, 960);

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
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4)); // 4 columns × 960px each = 2× viewport width
        engine.set_focus(WindowId(1));

        // Focus right twice: col1→col2 (visible), col2→col3 (triggers scroll)
        let f1 = engine.focus(Direction::Right).expect("focus right 1");
        assert_eq!(f1.0, WindowId(2));
        // After first focus (visible), viewport_offset unchanged
        let offset_after_first = engine.virtual_layout().viewport_offset;

        let f2 = engine.focus(Direction::Right).expect("focus right 2");
        assert_eq!(f2.0, WindowId(3));
        assert!(
            engine.virtual_layout().viewport_offset > offset_after_first,
            "viewport should have scrolled to show col 3 (was {offset_after_first}, now {})",
            engine.virtual_layout().viewport_offset
        );
    }

    #[test]
    fn engine_single_window_all_operations() {
        // Positive: single window — swap, expand/shrink return None appropriately
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));

        // Swap column left → None (no column to left)
        assert!(engine.swap_column(Direction::Left).is_none());
        // Swap column right → None (no column to right)
        assert!(engine.swap_column(Direction::Right).is_none());
        // Swap column up → None (vertical direction invalid for column swap)
        assert!(engine.swap_column(Direction::Up).is_none());
        // Swap column down → None (vertical direction invalid for column swap)
        assert!(engine.swap_column(Direction::Down).is_none());

        // Expand/shrink — single column at base width (960px)
        // Expand: 960 + column_shift(964) = 1924 → clamped to abs_max 1912px
        let diff = engine.expand_column().expect("expand single");
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1912);
        // Shrink: abs_max(1912) → slot_max(960px)

        let diff2 = engine.shrink_column().expect("shrink single");
        assert_eq!(diff2.virtual_layout.columns[0].width_px, 960);

        // Focus vertical — only one row
        assert!(engine.focus(Direction::Up).is_none());
        assert!(engine.focus(Direction::Down).is_none());

        // Monocle still works (single column)
        let diff = engine.toggle_monocle().expect("monocle");
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1912);
    }

    #[test]
    fn engine_empty_operations_return_none() {
        // Negative: all operations on empty engine return None or produce empty diffs
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);

        assert!(engine.scroll_left().is_none());
        assert!(engine.scroll_right().is_none());
        assert!(engine.focus(Direction::Right).is_none());
        assert!(engine.swap_column(Direction::Right).is_none());
        assert!(engine.expand_column().is_none());
        assert!(engine.shrink_column().is_none());
        assert!(engine.toggle_monocle().is_none());
    }

    #[test]
    fn engine_add_window_to_focused_column() {
        // Positive: add window as row to focused column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        let diff = engine
            .add_window_to_focused_column(WindowId(2))
            .expect("add to focused col");
        assert_eq!(engine.virtual_layout().columns.len(), 1); // still one column
        assert_eq!(engine.virtual_layout().columns[0].rows.len(), 2);
        // Both windows' positions changed (existing window shrunk, new window appeared)
        assert_eq!(diff.actual_layout.entries.len(), 2);
    }

    #[test]
    fn engine_add_to_focused_column_no_focus_returns_none() {
        // Negative: no focus → can't add to focused column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        assert!(engine.add_window_to_focused_column(WindowId(1)).is_none());
    }

    // --- Engine insert_window tests ---

    #[test]
    fn engine_insert_window_into_empty_layout() {
        // Positive: first window in empty layout → sole column, focused.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.insert_window(WindowId(1));
        assert_eq!(engine.virtual_layout().columns.len(), 1);
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
    }

    #[test]
    fn engine_insert_window_after_focused_shifts_right() {
        // Positive: insert between focused col 0 and col 1.
        // Layout: [W1][W2][W3], focus W1. insert W4 → [W1][W4][W2][W3].
        let mut engine = engine_with_three_columns();
        // engine_with_three_columns sets focus to W1 (col 0)
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));

        engine.insert_window(WindowId(4));
        assert_eq!(engine.virtual_layout().columns.len(), 4);
        assert_eq!(engine.virtual_layout().columns[0].rows[0], WindowId(1));
        assert_eq!(engine.virtual_layout().columns[1].rows[0], WindowId(4)); // new
        assert_eq!(engine.virtual_layout().columns[2].rows[0], WindowId(2)); // shifted
        assert_eq!(engine.virtual_layout().columns[3].rows[0], WindowId(3)); // shifted
        assert_eq!(engine.last_focused_window(), Some(WindowId(4))); // focus moved to new
    }

    #[test]
    fn engine_insert_window_after_last_column() {
        // Positive: focused on last column → insert at end.
        let mut engine = engine_with_three_columns();
        engine.set_focus(WindowId(3)); // last column

        let _ = engine.insert_window(WindowId(4));
        assert_eq!(engine.virtual_layout().columns.len(), 4);
        assert_eq!(engine.virtual_layout().columns[3].rows[0], WindowId(4));
        assert_eq!(engine.last_focused_window(), Some(WindowId(4)));
    }

    #[test]
    fn engine_insert_window_after_middle_column() {
        // Positive: focused on col 1 (middle) → insert at index 2.
        let mut engine = engine_with_three_columns();
        engine.set_focus(WindowId(2));

        let _ = engine.insert_window(WindowId(4));
        assert_eq!(engine.virtual_layout().columns.len(), 4);
        assert_eq!(engine.virtual_layout().columns[0].rows[0], WindowId(1)); // unchanged
        assert_eq!(engine.virtual_layout().columns[1].rows[0], WindowId(2)); // unchanged
        assert_eq!(engine.virtual_layout().columns[2].rows[0], WindowId(4)); // new
        assert_eq!(engine.virtual_layout().columns[3].rows[0], WindowId(3)); // shifted
    }

    #[test]
    fn engine_insert_window_scrolls_when_new_column_offscreen() {
        // Positive: many columns, insert creates a column off-screen right →
        // viewport scrolls to reveal it.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1)); // focus leftmost (col 0)

        engine.insert_window(WindowId(4));
        // New column at index 1. With 4 columns × 960px > 1920px viewport,
        // ensure_column_visible keeps the new column visible.
        assert_eq!(engine.last_focused_window(), Some(WindowId(4)));
    }

    #[test]
    fn engine_insert_window_after_remove_falls_back() {
        // Negative/edge: after removing the focused window, focus falls back
        // to the first column. Inserting then uses that focus as the anchor.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        // Remove focused (W2) → focus falls back to W1 (first column).
        let _ = engine.remove_window(WindowId(2));
        // Insert W3 after focused (W1) → W3 at col 1.
        engine.insert_window(WindowId(3));
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
    }

    #[test]
    fn engine_monocle_then_add_window() {
        // Positive: monocle on, add window (new column), toggle off on same column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));

        let d_on = engine.toggle_monocle().expect("monocle on");
        assert_eq!(d_on.virtual_layout.columns[0].width_px, 1912);

        // Add window → new column, focus moves to new window
        engine.add_window(WindowId(2));
        assert_eq!(engine.virtual_layout().columns.len(), 2);

        // Focus back to column 0 and toggle monocle off
        engine.set_focus(WindowId(1));
        let d_off = engine.toggle_monocle().expect("monocle off");
        assert_eq!(d_off.virtual_layout.columns[0].width_px, 960);
    }

    #[test]
    fn engine_expand_shrink_produces_pixel_diffs() {
        // Positive: expand → actual layout has different pixel sizes
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));

        // Focus WindowId(1) in column 0, expand (960 + column_shift(964) → abs_max 1912px)
        engine.set_focus(WindowId(1));
        let diff = engine.expand_column().expect("expand");
        // The layout must have entries (windows with new pixel positions)
        assert!(
            !diff.actual_layout.entries.is_empty(),
            "expand should produce layout entries"
        );
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1912);
        assert_eq!(diff.virtual_layout.columns[1].width_px, 960); // unchanged
    }

    #[test]
    fn engine_swap_shifts_camera_when_target_offscreen() {
        // Positive: swap with adjacent column that is off-screen shifts the camera
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.set_focus(WindowId(1));

        // Focus right twice to scroll viewport, then focus back to window 2
        // so column 0 (window 1) is off-screen left
        let _ = engine.focus(Direction::Right).expect("f1");
        let _ = engine.focus(Direction::Right).expect("f2");
        assert!(engine.virtual_layout().viewport_offset > 0);

        // Focus on window at the left edge of viewport, then swap left
        // The column to the left (window 1) is off-screen
        engine.set_focus(WindowId(2));
        let prev_offset = engine.virtual_layout().viewport_offset;
        let diff = engine.swap_column(Direction::Left).expect("swap left");
        assert!(!diff.actual_layout.entries.is_empty());
        // Camera should shift to make the swapped column visible
        assert!(
            diff.virtual_layout.viewport_offset < prev_offset,
            "viewport should have scrolled left to reveal swapped column"
        );
    }

    #[test]
    fn engine_scroll_right_at_boundary() {
        // Negative: can't scroll past rightmost column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1)); // single 960px column

        assert!(engine.scroll_right().is_none());
    }

    #[test]
    fn engine_set_column_width() {
        // Positive: explicit column width setting
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));

        // Focus WindowId(1) in column 0, set width to 1440px (free-form, independent)
        engine.set_focus(WindowId(1));
        let diff = engine.set_column_width(1440).expect("set width");
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1440);
        // Neighbor is unchanged (no compensation)
        assert_eq!(diff.virtual_layout.columns[1].width_px, 960);
    }

    #[test]
    fn engine_remove_focused_window_focus_falls_back() {
        // Positive: removing a focused middle window falls back to the LEFT column.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(2));

        let _ = engine.remove_window(WindowId(2));
        // Focus falls back to the left column's window (W1).
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
    }

    #[test]
    fn engine_remove_focused_leftmost_falls_to_right() {
        // Removing the focused leftmost window has no left column, so focus
        // falls back to the RIGHT column's window.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));

        let _ = engine.remove_window(WindowId(1));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    #[test]
    fn engine_remove_focused_rightmost_falls_to_left() {
        // Removing the focused rightmost window falls back to the LEFT column.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(3));

        let _ = engine.remove_window(WindowId(3));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    #[test]
    fn engine_remove_only_window_clears_focus() {
        // Removing the only window leaves focus as None.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));

        let _ = engine.remove_window(WindowId(1));
        assert_eq!(engine.last_focused_window(), None);
        assert!(engine.virtual_layout().columns.is_empty());
    }

    #[test]
    fn engine_remove_nonfocused_keeps_focus() {
        // Removing a non-focused window must not change focus.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));

        let _ = engine.remove_window(WindowId(2));
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
    }

    #[test]
    fn engine_remove_nonexistent_window_is_noop() {
        // Negative: removing a window that doesn't exist in the layout should
        // not crash, not change focus, and not alter the layout.
        let mut engine = engine_with_three_columns();
        engine.set_focus(WindowId(1));

        let diff = engine.remove_window(WindowId(99));
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(1)),
            "focus must not change"
        );
        assert_eq!(
            diff.virtual_layout.columns.len(),
            3,
            "column count must not change"
        );
        assert_eq!(
            diff.virtual_layout.window_count(),
            3,
            "window count must not change"
        );
    }

    #[test]
    fn engine_remove_from_multirow_column_preserves_column() {
        // Positive: removing a window from a multi-row column removes only the
        // row — the column persists with the remaining window.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        // Layout: [W1, W2] in one column. Focus on W2.

        let _ = engine.remove_window(WindowId(2));
        // Column should still exist with W1.
        assert_eq!(engine.virtual_layout().columns.len(), 1);
        assert_eq!(engine.virtual_layout().columns[0].rows.len(), 1);
        assert_eq!(engine.virtual_layout().columns[0].rows[0], WindowId(1));
        // Focus was on W2 (removed). next_available_window at (col 0, row 1):
        // no left, no right → None. Focus becomes None.
        assert_eq!(
            engine.last_focused_window(),
            None,
            "focus should be None after removing the focused window from a single-column multi-row layout"
        );
    }

    #[test]
    fn engine_remove_nonfocused_left_of_focus_shifts_column_index() {
        // Positive: removing a non-focused window from a column to the LEFT of
        // the focused window shifts the focused window's column index left by 1.
        // The viewport should still show the focused window correctly.
        // Layout: [W1][W2][W3], focus W3 (col 2). Remove W1 (col 0).
        // After removal: [W2][W3], W3 is now at col 1.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(3));

        let diff = engine.remove_window(WindowId(1));
        // Focus should be unchanged (W3 was not removed).
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
        // Layout should have 2 columns.
        assert_eq!(diff.virtual_layout.columns.len(), 2);
        // W3 should be in column index 1 (shifted from 2).
        assert_eq!(diff.virtual_layout.columns[1].rows[0], WindowId(3));
    }

    #[test]
    fn engine_remove_focused_scrolls_to_successor() {
        // Positive: removing the focused window when the successor's column is
        // off-screen should scroll the viewport to reveal the successor.
        // Setup: 4 columns, viewport scrolled to show columns 2–3, focus on col 3.
        // Remove col 3's window → successor is col 2 (left) which is already
        // visible, so viewport doesn't scroll. Then test a case where successor
        // IS off-screen: focus col 3, scroll viewport so col 0 is visible only,
        // then remove col 3 → successor col 2 is off-screen → viewport adjusts.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.set_focus(WindowId(4));

        // Scroll viewport so only the first column is visible (viewport_offset = 0).
        // All 4 columns span ~3860px, viewport is 1920px. Col 0 is at [4, 964],
        // col 1 at [968, 1928], col 2 at [1932, 2892], col 3 at [2896, 3860].
        // Only col 0 is fully visible from offset 0.

        let prev_offset = engine.virtual_layout().viewport_offset;
        let diff = engine.remove_window(WindowId(4));
        // Focus should fall to W3 (left neighbour).
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
        // W3 is now in column index 2 (columns shifted after removal).
        // W3's column is at canvas position ~1932, which is off-screen from
        // offset 0 (viewport right = 1920). ensure_column_visible should scroll.
        assert_eq!(
            diff.virtual_layout.columns.len(),
            3,
            "should have 3 columns after removing W4"
        );
        // The viewport_offset should have changed (or at minimum, the diff
        // should contain moves if any viewport shift happened).
        // W3 at col 2, canvas ~1932. ensure_column_visible should shift viewport.
        // The exact offset depends on the quantization math, but it should be
        // non-zero because col 2 is off-screen at offset 0.
        assert_ne!(
            diff.virtual_layout.viewport_offset, prev_offset,
            "viewport should scroll to reveal successor column that is off-screen"
        );
    }

    #[test]
    fn engine_focus_returns_none_diff_when_no_scroll() {
        // Positive: focus within fully visible columns → diff is None (no viewport change).
        // The (WindowId, Option<AppliedLayout>) tuple must have None in the diff slot.
        // Use a wider monitor (3840px) so all 3 columns fit in the viewport without scroll.
        let monitor = MonitorInfo {
            work_area: Rect {
                x: 0,
                y: 0,
                width: 3840,
                height: 1080,
            },
        };
        let mut engine = ScrollingSpace::new(monitor, 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));

        let result = engine.focus(Direction::Right).expect("focus right");
        assert_eq!(result.0, WindowId(2));
        // Column 2 is visible within the 3840px viewport → no scroll needed → diff is None
        assert!(
            result.1.is_none(),
            "focus within visible area should not produce a AppliedLayout, got Some"
        );
    }

    #[test]
    fn engine_focus_returns_some_diff_when_viewport_scrolls() {
        // Positive: focus into off-screen column → diff is Some with non-empty moves.
        // This is the core Fix 1 behavior: the caller receives the AppliedLayout to animate.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4)); // 4 columns × 960px each = 2× viewport width
        engine.set_focus(WindowId(1));

        // Focus right twice: first stays visible, second triggers scroll
        let _ = engine.focus(Direction::Right).expect("focus right 1");
        let result = engine.focus(Direction::Right).expect("focus right 2");

        assert_eq!(result.0, WindowId(3));
        // Viewport scrolled → diff must be Some
        assert!(
            result.1.is_some(),
            "focus into off-screen column must produce Some(AppliedLayout)"
        );
        let diff = result.1.unwrap();
        // The diff must contain moves because windows shifted on screen
        assert!(
            !diff.actual_layout.entries.is_empty(),
            "AppliedLayout from viewport scroll must contain animation moves"
        );
    }

    #[test]
    fn engine_focus_vertical_never_produces_diff() {
        // Positive: vertical focus (Up/Down) never scrolls viewport, so diff is always None.
        // Even when the window has rows, vertical focus just changes the focused WindowId.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        engine.set_focus(WindowId(1));

        let result = engine.focus(Direction::Down).expect("focus down");
        assert_eq!(result.0, WindowId(2));
        assert!(
            result.1.is_none(),
            "vertical focus should never produce a AppliedLayout"
        );
    }

    #[test]
    fn engine_focus_no_change_in_layout_when_visible() {
        // Positive: focus within fully visible area → no viewport scroll needed.
        // With a single multi-row column, focus has nowhere to go horizontally,
        // but vertical focus doesn't trigger scroll. This test verifies vertical focus
        // doesn't change the viewport offset.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        engine.set_focus(WindowId(1));

        let prev_offset = engine.virtual_layout().viewport_offset;
        let _ = engine.focus(Direction::Down).expect("focus down");
        // Vertical focus within a fully visible column → no scroll
        assert_eq!(engine.virtual_layout().viewport_offset, prev_offset);
    }

    // --- Engine swap_window tests ---

    #[test]
    fn engine_swap_window_cross_column() {
        // Positive: engine swap_window moves individual windows between columns
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(5));
        engine.add_window(WindowId(2));
        engine.set_focus(WindowId(1));

        // Layout: [W1, W5] [W2]. swap_window Right on W1 → [W2, W5] [W1]
        let diff = engine.swap_window(Direction::Right).expect("swap window");
        assert_eq!(
            diff.virtual_layout.columns[0].rows,
            vec![WindowId(2), WindowId(5)]
        );
        assert_eq!(diff.virtual_layout.columns[1].rows, vec![WindowId(1)]);
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_swap_window_same_column_vertical() {
        // Positive: engine swap_window Up/Down within column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        engine.set_focus(WindowId(2));

        let diff = engine.swap_window(Direction::Up).expect("swap window up");
        assert_eq!(
            diff.virtual_layout.columns[0].rows,
            vec![WindowId(2), WindowId(1)]
        );
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_swap_window_none_when_no_focus() {
        // Negative: engine swap_window without focus → None
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        // Focus is already on WindowId(1) from add_window. Clear it.
        // We can't clear focus directly, so test with empty engine instead.
        let mut empty_engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        assert!(empty_engine.swap_window(Direction::Right).is_none());
        assert!(empty_engine.swap_window(Direction::Up).is_none());
        assert!(empty_engine.swap_window(Direction::Down).is_none());
    }

    #[test]
    fn engine_swap_window_none_at_boundary() {
        // Negative: engine swap_window at layout boundary → None
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.set_focus(WindowId(1));

        assert!(engine.swap_window(Direction::Left).is_none());
        assert!(engine.swap_window(Direction::Right).is_none());
        assert!(engine.swap_window(Direction::Up).is_none());
        assert!(engine.swap_window(Direction::Down).is_none());
    }

    // --- Engine initialize_windows tests ---

    #[test]
    fn engine_initialize_windows_empty() {
        // Positive: empty vec → empty layout, no focus
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        let diff = engine.initialize_windows(vec![], None, None);
        assert!(diff.virtual_layout.columns.is_empty());
        assert!(engine.last_focused_window().is_none());
    }

    #[test]
    fn engine_initialize_windows_single() {
        // Positive: single window → single column, focused
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        let diff = engine.initialize_windows(vec![WindowId(1)], None, None);
        assert_eq!(diff.virtual_layout.columns.len(), 1);
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
        // Should produce moves (new window appeared)
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_initialize_windows_multiple() {
        // Positive: multiple windows → multiple columns, focus on last
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        let diff =
            engine.initialize_windows(vec![WindowId(10), WindowId(20), WindowId(30)], None, None);
        assert_eq!(diff.virtual_layout.columns.len(), 3);
        assert_eq!(diff.virtual_layout.columns[0].rows[0], WindowId(10));
        assert_eq!(diff.virtual_layout.columns[1].rows[0], WindowId(20));
        assert_eq!(diff.virtual_layout.columns[2].rows[0], WindowId(30));
        assert_eq!(engine.last_focused_window(), Some(WindowId(30))); // last window
    }

    #[test]
    fn engine_initialize_windows_focus_first_column() {
        // Positive: focus_col_idx=Some(0) → focused should be the first window,
        // NOT the last window. This tests the viewport-init fix: the engine
        // now respects the focus column from the foreground window lookup
        // instead of blindly picking the last column.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        let diff = engine.initialize_windows(
            vec![WindowId(10), WindowId(20), WindowId(30)],
            Some(0),
            None,
        );
        assert_eq!(diff.virtual_layout.columns.len(), 3);
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(10)),
            "focus_col_idx=0 should focus the first window, not the last"
        );
    }

    #[test]
    fn engine_initialize_windows_focus_middle_column() {
        // Positive: focus_col_idx=Some(1) → focused should be the second window.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.initialize_windows(
            vec![WindowId(10), WindowId(20), WindowId(30)],
            Some(1),
            None,
        );
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(20)),
            "focus_col_idx=1 should focus the second window"
        );
    }

    #[test]
    fn engine_initialize_windows_none_focus_falls_back_to_last() {
        // Positive: focus_col_idx=None → fallback to last window (existing behavior).
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.initialize_windows(vec![WindowId(10), WindowId(20), WindowId(30)], None, None);
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(30)),
            "focus_col_idx=None should fall back to last window"
        );
    }

    #[test]
    fn engine_initialize_windows_scroll_mode_focus_col() {
        // Positive: 4 cols, columns_per_screen=2 → scroll mode.
        // Focus on column 2 → viewport_offset should be non-zero,
        // and focused should be the window at column 2.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 2);
        let diff = engine.initialize_windows(
            vec![WindowId(1), WindowId(2), WindowId(3), WindowId(4)],
            Some(2),
            None,
        );
        assert_eq!(diff.virtual_layout.columns.len(), 4);
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(3)),
            "focus_col_idx=2 should focus WindowId(3)"
        );
        assert_ne!(
            diff.virtual_layout.viewport_offset, 0,
            "4 cols > columns_per_screen=2 → viewport should scroll to focus col"
        );
    }

    // -------------------------------------------------------------------
    // Bug 1 regression: AppliedLayout.actual_layout.entries must contain
    // ALL windows (not just "moved" ones). This is the core invariant
    // that prevents the swapcolumn animation stranding bug.
    // -------------------------------------------------------------------

    #[test]
    fn applied_layout_entries_match_window_count_after_swap() {
        // Positive: after a swap_column mutation, actual_layout.entries must
        // contain every window — not just the two columns that swapped.
        // Regression test for: rapid swapcolumn during in-flight animation
        // stranded mid-flight windows because the old diff only included
        // windows whose target changed.
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count(); // 3

        let result = engine.swap_column(Direction::Right).expect("swap");
        assert_eq!(
            result.actual_layout.entries.len(),
            total_windows,
            "AppliedLayout must contain ALL windows after swap_column, not just the swapped ones"
        );

        // Verify each specific window is present
        for wid in [WindowId(1), WindowId(2), WindowId(3)] {
            assert!(
                result
                    .actual_layout
                    .entries
                    .iter()
                    .any(|e| e.window_id == wid),
                "AppliedLayout must contain window after swap_column (id={:?})",
                wid
            );
        }
    }

    #[test]
    fn applied_layout_entries_match_window_count_after_expand() {
        // Positive: after expand_column, entries must still contain ALL windows
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count();

        let result = engine.expand_column().expect("expand");
        assert_eq!(
            result.actual_layout.entries.len(),
            total_windows,
            "AppliedLayout must contain ALL windows after expand_column"
        );
    }

    #[test]
    fn rapid_consecutive_mutations_both_return_full_layouts() {
        // Positive: simulate rapid mutations (A then B) without animation settling.
        // Both AppliedLayouts must contain all windows. This is the exact scenario
        // that caused Bug 1: a second swap while the first swap's animation was
        // still in-flight dropped mid-flight windows from the diff.
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count(); // 3

        // Mutation A: swap columns 0 ↔ 1
        let result_a = engine.swap_column(Direction::Right).expect("swap A");
        assert_eq!(
            result_a.actual_layout.entries.len(),
            total_windows,
            "First mutation (swap A) must contain ALL windows"
        );

        // Mutation B: immediately swap again (same direction, different result
        // because focus is still on WindowId(1) which is now in column 1).
        // [W2, W1, W3] → swap W1 right → [W2, W3, W1]
        let result_b = engine.swap_column(Direction::Right).expect("swap B");
        assert_eq!(
            result_b.actual_layout.entries.len(),
            total_windows,
            "Second mutation (swap B) must contain ALL windows — \
             this was Bug 1: mid-flight windows were stranded because the old \
             diff only included windows whose target changed"
        );

        // Verify the second swap actually moved W1 further right
        assert_eq!(result_b.virtual_layout.columns[0].rows[0], WindowId(2));
        assert_eq!(result_b.virtual_layout.columns[1].rows[0], WindowId(3));
        assert_eq!(result_b.virtual_layout.columns[2].rows[0], WindowId(1));
    }

    #[test]
    fn rapid_swap_then_expand_both_return_full_layouts() {
        // Positive: rapid heterogeneous mutations (swap then expand) must both
        // return full layouts. Tests that the invariant holds across different
        // mutation types, not just consecutive same-type mutations.
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count();

        // Mutation A: swap column
        let result_a = engine.swap_column(Direction::Right).expect("swap");
        assert_eq!(result_a.actual_layout.entries.len(), total_windows);

        // Mutation B: expand column (different mutation type, still rapid)
        let result_b = engine.expand_column().expect("expand");
        assert_eq!(
            result_b.actual_layout.entries.len(),
            total_windows,
            "Expand after swap must still return ALL windows in actual_layout.entries"
        );
    }

    #[test]
    fn applied_layout_includes_parked_windows() {
        // Positive: windows that are off-screen (parked) must still appear in
        // actual_layout.entries. The projection parks them off-screen with
        // deterministic rects — they must not be omitted.
        // Use 5 columns: only ~2 visible at a time on a 1920px monitor.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.add_window(WindowId(5));
        let total_windows = engine.virtual_layout().window_count(); // 5

        // Viewport at offset 0: cols 0-1 visible, cols 2-4 parked right.
        let layout = engine.actual_layout();
        assert_eq!(
            layout.entries.len(),
            total_windows,
            "actual_layout must include parked (off-screen) windows, not just visible ones"
        );

        // Verify ALL window IDs are present, including parked ones
        for wid in [
            WindowId(1),
            WindowId(2),
            WindowId(3),
            WindowId(4),
            WindowId(5),
        ] {
            assert!(
                layout.entries.iter().any(|e| e.window_id == wid),
                "actual_layout must contain parked window (id={:?})",
                wid
            );
        }
    }

    #[test]
    fn applied_layout_after_scroll_contains_all_windows() {
        // Positive: scrolling changes viewport but all windows must still be present
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count();

        let result = engine.scroll_right().expect("scroll");
        assert_eq!(
            result.actual_layout.entries.len(),
            total_windows,
            "Scroll must return ALL windows in actual_layout.entries (visible + parked)"
        );
    }

    // --- column_width_bounds (Area 10) ---

    #[test]
    fn engine_column_width_bounds_returns_config_range() {
        // Area 10: column_width_bounds() returns (min_column_width_px, abs_max_width).
        // test_monitor: 1920×1080, test_padding: gap=4.
        // Engine created with min_column_width_px=320.
        // abs_max = 1920 − 2×4 = 1912.
        let engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        let (min, max) = engine.column_width_bounds();
        assert_eq!(min, 320, "min should equal min_column_width_px");
        assert_eq!(max, 1912, "max should equal monitor_width − 2×gap");
    }

    #[test]
    fn engine_column_width_bounds_with_different_config() {
        // Area 10: verify bounds change with different min_column_width_px and gap.
        // gap=8 → abs_max = 1920 − 2×8 = 1904.
        let engine = ScrollingSpace::new(
            test_monitor(),
            960,
            500, // min_column_width_px
            Padding {
                window_gap: 8,
                up: 0,
                down: 0,
            },
            4,
        );
        let (min, max) = engine.column_width_bounds();
        assert_eq!(min, 500);
        assert_eq!(max, 1904);
    }

    // --- padding accessor (Phase 4 workspace-op support) ---
    //
    // The `padding()` accessor is how the workspace-op dispatch handlers read
    // the cached `window_gap` to compute the vertical parking unit
    // (`monitor_height + window_gap`) fed to `workspace_y_offset`. It is a
    // trivial getter, but it had zero direct coverage — every `test_padding()`
    // reference elsewhere in this module is the *helper*, not the accessor.

    #[test]
    fn engine_padding_returns_configured_value() {
        // Arrange: construct an engine with the canonical test padding.
        // Act/Assert: the accessor returns exactly the Padding handed to `new`,
        // unchanged — the dispatch path depends on this being the stored value.
        let engine = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        assert_eq!(engine.padding(), test_padding());
    }

    #[test]
    fn engine_padding_preserves_non_default_margins() {
        // Arrange: a distinct padding with non-zero up/down margins and a
        // different gap, so a future refactor that only stores `window_gap`
        // (or hardcodes the default) would be caught.
        // Act/Assert: the full Padding (gap + up + down) round-trips verbatim.
        let custom = Padding {
            window_gap: 12,
            up: 8,
            down: 40,
        };
        let engine = ScrollingSpace::new(test_monitor(), 960, 320, custom, 4);
        assert_eq!(engine.padding(), custom);
    }

    // Shared helper for the prefix-sum center tests below (col_width=400 so
    // single-column offsets are non-degenerate).

    /// Build an engine with `col_width=400` for non-degenerate center tests.
    fn center_test_engine() -> ScrollingSpace {
        ScrollingSpace::new(test_monitor(), 400, 100, test_padding(), 4)
    }

    // --- Prefix-sum center operations (center_focused_column / center_canvas) ---

    #[test]
    fn engine_center_focused_column_empty_returns_none() {
        // Negative: empty layout → None.
        let mut engine = center_test_engine();
        assert!(engine.center_focused_column().is_none());
    }

    #[test]
    fn engine_center_focused_column_centers_at_midpoint() {
        // Positive: single column (400px), gap=4, monitor=1920.
        // canvas_x(0) = 0*(anything) + (0+1)*4 = 4.
        // viewport_offset = 4 - (1920 - 400) / 2 = 4 - 760 = -756.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.set_focus(WindowId(1));

        let diff = engine
            .center_focused_column()
            .expect("center focused on 1 col");
        assert_eq!(
            diff.virtual_layout.viewport_offset, -756,
            "focused column should land at monitor midpoint"
        );
        assert_eq!(engine.virtual_layout().viewport_offset, -756);
    }

    #[test]
    fn engine_center_focused_column_overflow_always_centers() {
        // Positive: 4 cols × 400px (overflow case), focus on col 2.
        // canvas_x(2) = (400 + 400)*1 + 3*4 = 812.
        // focused_width = 400.
        // offset = 812 - (1920 - 400) / 2 = 812 - 760 = 52.
        // Always centers even in overflow — this is the explicit center behavior.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.set_focus(WindowId(3)); // col 2

        let diff = engine
            .center_focused_column()
            .expect("center focused overflow");
        assert_eq!(diff.virtual_layout.viewport_offset, 52);
    }

    #[test]
    fn engine_center_focused_column_preserves_all_windows() {
        // Invariant: centering only changes viewport_offset, not focus or columns.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(2));

        let total = engine.virtual_layout().window_count();
        let diff = engine.center_focused_column().expect("center focused");
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
        assert_eq!(diff.virtual_layout.window_count(), total);
        assert_eq!(diff.actual_layout.entries.len(), total);
    }

    #[test]
    fn engine_center_canvas_empty_returns_none() {
        // Negative: empty layout → None.
        let mut engine = center_test_engine();
        assert!(engine.center_canvas().is_none());
    }

    #[test]
    fn engine_center_canvas_negative_when_canvas_smaller_than_monitor() {
        // Positive: single col (400px), gap=4 → canvas_width = 4 + 404 = 408.
        // offset = (408 - 1920) / 2 = -756. Negative — projection handles it.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));

        let diff = engine.center_canvas().expect("center canvas on 1 col");
        assert_eq!(
            diff.virtual_layout.viewport_offset, -756,
            "canvas < monitor → negative offset"
        );
    }

    #[test]
    fn engine_center_canvas_overflow_positive() {
        // Positive: 5 cols × 400px, gap=4.
        // canvas_width = 4 + 5*(400+4) = 4 + 2020 = 2024.
        // offset = (2024 - 1920) / 2 = 52.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.add_window(WindowId(5));

        let diff = engine.center_canvas().expect("center canvas overflow");
        assert_eq!(diff.virtual_layout.viewport_offset, 52);
    }

    // --- insert_window_with_width tests ---

    #[test]
    fn engine_insert_window_with_width_preserves_custom_width() {
        // Positive: insert with width_px=1440 → column gets quantized width.
        // col_width=400, column_shift=404. Nearest rung to 1440:
        // n = round((1440-400)/404) = round(2.574) = 3. target = 400 + 3*404 = 1612.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.set_focus(WindowId(1));

        engine.insert_window_with_width(WindowId(2), 1440);
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(
            engine.virtual_layout().columns[1].width_px,
            1612,
            "width 1440 should quantize to nearest ladder rung (1612)"
        );
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    #[test]
    fn engine_insert_window_with_width_no_focus_appends() {
        // Positive: no focus → appends at end with custom width.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        // Clear focus to test the no-focus fallback.
        engine.last_focused_window = None;

        engine.insert_window_with_width(WindowId(2), 400);
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(engine.virtual_layout().columns[1].rows[0], WindowId(2));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    // --- initialize_windows with widths tests ---

    #[test]
    fn engine_initialize_windows_with_widths_quantizes() {
        // Positive: widths are quantized to the nearest ladder rung.
        // col_width=400, shift=404. 1440 → nearest rung: n=round((1440-400)/404)=3, 400+3*404=1612.
        let mut engine = center_test_engine();
        engine.initialize_windows(vec![WindowId(1), WindowId(2)], None, Some(&[400, 1440]));
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(engine.virtual_layout().columns[0].width_px, 400);
        assert_eq!(
            engine.virtual_layout().columns[1].width_px,
            1612,
            "1440 should quantize to 1612"
        );
    }

    #[test]
    fn engine_initialize_windows_with_widths_fit_centers_canvas() {
        // Positive: 2 cols with small widths → fit case → canvas centered.
        // col_width=400. Both widths=400. canvas = 4 + 2*404 = 812.
        // offset = (812 - 1920) / 2 = -554.
        let mut engine = center_test_engine();
        let diff =
            engine.initialize_windows(vec![WindowId(1), WindowId(2)], None, Some(&[400, 400]));
        assert_eq!(
            diff.virtual_layout.viewport_offset, -554,
            "fit case should center entire canvas"
        );
    }

    // --- ensure_focused_visible tests ---
    //
    // `ensure_focused_visible` is the pub(crate) wrapper that dispatch's
    // MoveWindowToWorkspace overflow branch relies on. It resolves the
    // focused column (via `focus_col_index`, with col-0 fallback) and
    // delegates to `mutations::ensure_column_visible`. The underlying
    // mutation is heavily tested; these tests pin the wrapper's contract:
    // empty → None, no-op when visible, shifts to reveal when off-screen,
    // and the no-focus → col-0 fallback.

    #[test]
    fn engine_ensure_focused_visible_empty_returns_none() {
        // Negative: empty layout → None (mirrors center_focused_column /
        // center_canvas on empty).
        let mut engine = center_test_engine();
        assert!(engine.ensure_focused_visible().is_none());
    }

    #[test]
    fn engine_ensure_focused_visible_no_change_when_already_visible() {
        // Positive: 2 cols × 400px fit easily in 1920px viewport at offset 0.
        // Focused col already visible → no viewport shift, no diff (idempotency).
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.set_focus(WindowId(1));
        let before = engine.virtual_layout().viewport_offset;

        let diff = engine.ensure_focused_visible();

        assert!(
            diff.is_none(),
            "no diff when focused col is already visible (idempotency)"
        );
        assert_eq!(
            engine.virtual_layout().viewport_offset, before,
            "no shift when focused col is already visible"
        );
    }

    #[test]
    fn engine_ensure_focused_visible_shifts_viewport_to_reveal_focused_col() {
        // Positive: 5 cols × 400px (canvas=2024 > 1920). After 5 add_window
        // calls the viewport is still at offset 0 (add_window doesn't shift).
        // Focus col 4 (WindowId(5)) is off-screen right: canvas_x(4) = 4 +
        // 4*404 = 1620, right = 2020 > vp_right (1920). After
        // ensure_focused_visible, the focused col's right edge must be within
        // the viewport.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.add_window(WindowId(5));
        engine.set_focus(WindowId(5)); // rightmost column

        let offset_before = engine.virtual_layout().viewport_offset;
        let _ = engine.ensure_focused_visible().expect("non-empty layout");

        let offset_after = engine.virtual_layout().viewport_offset;
        let vp_right = offset_after + 1920;
        let col4_right = 1620 + 400; // canvas_x(4) + col_width
        assert!(
            offset_after > offset_before,
            "viewport must shift right when focused col is off-screen"
        );
        assert!(
            col4_right <= vp_right,
            "col 4 right edge ({col4_right}) must be within viewport right ({vp_right}) \
             after ensure_focused_visible"
        );
    }

    #[test]
    fn engine_ensure_focused_visible_uses_col0_fallback_when_focus_is_none() {
        // Contract: when self.last_focused_window is None, focus_col_index returns 0 and
        // ensure_focused_visible targets col 0. With col 0 already visible
        // from offset 0, this is a no-op (returns None — idempotency).
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.last_focused_window = None; // bypass set_focus (which writes Some)
        let before = engine.virtual_layout().viewport_offset;

        let diff = engine.ensure_focused_visible();

        assert!(
            diff.is_none(),
            "no-focus path falls back to col 0; col 0 visible → no diff"
        );
        assert_eq!(
            engine.virtual_layout().viewport_offset, before,
            "no shift when col 0 is already visible"
        );
    }

    #[test]
    fn engine_ensure_focused_visible_uses_col0_fallback_when_focus_is_stale() {
        // Contract: when self.last_focused_window points at a window no longer in the
        // layout, focus_col_index falls back to 0. With col 0 visible the
        // fallback is a no-op (returns None — idempotency).
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.set_focus(WindowId(99)); // not in layout
        let before = engine.virtual_layout().viewport_offset;

        let diff = engine.ensure_focused_visible();

        assert!(
            diff.is_none(),
            "stale focus falls back to col 0; col 0 visible → no diff"
        );
        assert_eq!(
            engine.virtual_layout().viewport_offset, before,
            "no shift when col 0 is already visible"
        );
    }
}
