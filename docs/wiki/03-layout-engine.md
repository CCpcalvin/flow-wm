# Layout Engine (`src/layout/`) — Reference

Developer reference for `src/layout/`. The largest implemented module — pure Rust, zero Win32, testable on any platform.

---

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `types.rs` | 224 | Column, VirtualLayout, ActualLayout, WindowMove, AnimationHint, LayoutDiff |
| `projection.rs` | 507 | Virtual → Actual projection with geometric parking |
| `diff.rs` | 549 | Layout diff and AnimationHint classification |
| `mutations.rs` | 1099 | All pure mutation functions |
| `engine.rs` | 622 | LayoutEngine orchestrator |
| `mod.rs` | — | Re-exports |

Total: **3001 lines**

---

## Container Model

```text
HorizontalContainer (VirtualLayout)
├── Column (VerticalContainer) — rows stacked top-to-bottom
├── Column
└── Column
```

- **Horizontal resize** → adjust column `width_eighths` (1–8, proportional to monitor width)
- **Vertical resize** → adjust column `row_ratios` (f32 values summing to 1.0)

---

## Core Types

### Column

```rust
pub type WidthEighths = u8;  // 1–8

pub struct Column {
    pub width_eighths: WidthEighths,
    pub rows: Vec<WindowId>,
    pub row_ratios: Vec<f32>,
}

impl Column {
    pub fn new(width_eighths: WidthEighths, window: WindowId) -> Self;
    pub fn with_equal_rows(width_eighths: WidthEighths, rows: Vec<WindowId>) -> Self;
    pub fn is_valid_width(&self) -> bool;  // 1..=8
}
```

### VirtualLayout

```rust
pub struct VirtualLayout {
    pub columns: Vec<Column>,     // left-to-right on infinite canvas
    pub viewport_offset: i32,     // pixel offset from canvas left to viewport left
}

impl VirtualLayout {
    pub fn new() -> Self;
    pub fn with_columns(columns: Vec<Column>, viewport_offset: i32) -> Self;
    pub fn find_window(&self, id: WindowId) -> Option<(usize, usize)>;  // (col, row)
    pub fn window_count(&self) -> usize;
}
```

### ActualLayout

```rust
pub struct ActualEntry {
    pub window_id: WindowId,
    pub rect: Rect,
}

pub struct ActualLayout {
    pub entries: Vec<ActualEntry>,
}

impl ActualLayout {
    pub fn find(&self, id: WindowId) -> Option<&ActualEntry>;
}
```

### WindowMove & AnimationHint

```rust
pub struct WindowMove {
    pub window_id: WindowId,
    pub from: Rect,
    pub to: Rect,
    pub hint: AnimationHint,
}

pub enum AnimationHint {
    Snap,         // in-viewport move <500px
    Displaced,    // neighbor pushed aside
    ScrollEnter,  // entering viewport (>500px rightward)
    ScrollExit,   // leaving viewport (>500px leftward)
    Restore,      // crash/minimize restore — instant, no animation
}
```

### LayoutDiff

```rust
pub struct LayoutDiff {
    pub virtual_layout: VirtualLayout,
    pub actual_layout: ActualLayout,
    pub moves: Vec<WindowMove>,
}
```

### Supporting Types

```rust
pub struct MonitorInfo {
    pub work_area: Rect,
}

pub struct Gaps {
    pub inner: i32,
    pub outer: i32,
}
```

---

## Projection Algorithm

```rust
pub fn project(
    virtual_layout: &VirtualLayout,
    monitor: &MonitorInfo,
    gaps: &Gaps,
) -> ActualLayout;
```

### Pipeline

1. Compute pixel width per column: `col_px = (width_eighths * monitor_width) / 8`
2. Track cumulative `canvas_x` for each column
3. Determine visibility: `col_right > viewport_left && col_left < viewport_right`
4. **Visible columns**: `screen_x = monitor_left + (canvas_x - viewport_offset)`, then inset by `outer` gap
5. **Off-screen left**: `park_x = monitor_left - col_width - outer_gap`
6. **Off-screen right**: `park_x = monitor_right + outer_gap`

### Geometric Parking Model

```text
[Parked L] [outer] [Col 1] [inner] [Col 2] [outer] | viewport | [outer] [Col n] ... [Parked R]
```

- Left-parked: `monitor_left - col_width - outer_gap`
- Right-parked: `monitor_right + outer_gap`

No magic offsets — deterministic and proportional.

### Row Projection

Within a visible column, rows are stacked top-to-bottom:
- Available height = `monitor_height - 2 * outer_gap`
- Usable height = `available - inner_gap * (row_count - 1)`
- Each row height = `usable * row_ratio[i]` (or equal division if no ratios)

### Helper Functions

```rust
pub(crate) fn column_eighths_to_pixels(eighths: u8, monitor_width: i32) -> i32;
pub fn canvas_width(layout: &VirtualLayout, monitor_width: i32, gaps: &Gaps) -> i32;
pub fn column_step_width(column: &Column, monitor_width: i32, inner_gap: i32) -> i32;
```

---

## Diff Computation

```rust
pub fn diff(prev: &ActualLayout, next: &ActualLayout) -> Vec<WindowMove>;
pub fn removed_windows(prev: &ActualLayout, next: &ActualLayout) -> Vec<WindowId>;
```

### Classification Rules

Uses `SCROLL_THRESHOLD = 500` pixels:

| Condition | Hint |
|-----------|------|
| `|dx| ≤ 500` | `Snap` |
| `dx > 500` (moving right) | `ScrollEnter` |
| `dx > 500` (moving left) | `ScrollExit` |
| New window (not in prev) | `ScrollEnter` (from parked at x=-10000) |
| No position change | Not included in diff |

Windows present in `prev` but not `next` are NOT in the diff — call `removed_windows()` separately.

---

## Mutation Operations

All mutations are **pure functions** — they take `&VirtualLayout` and return a new one. The layout is never mutated in place.

### MutationConfig

```rust
pub struct MutationConfig {
    pub monitor_width: i32,
    pub default_column_width_eighths: u8,
    pub gaps: Gaps,
}
```

Derived from `StmConfig` — the layout engine never hardcodes size values.

### Scroll

```rust
pub fn scroll_left(layout: &VirtualLayout, config: &MutationConfig) -> Option<VirtualLayout>;
pub fn scroll_right(layout: &VirtualLayout, config: &MutationConfig) -> Option<VirtualLayout>;
```

Scrolls by the step width of the first visible column (`col_width + inner_gap`). Returns `None` at boundaries.

### Focus

```rust
pub struct FocusResult {
    pub focused: WindowId,
    pub new_layout: VirtualLayout,
}

pub fn focus(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<FocusResult>;
```

- `Left`/`Right`: moves to adjacent column, auto-scrolls if target is off-screen
- `Up`/`Down`: moves within column rows, no scroll
- Returns `None` if no window in that direction

Auto-scroll uses `ensure_column_visible()` which adjusts `viewport_offset` minimally to bring the target column into the viewport.

### Swap

```rust
pub fn swap(layout: &VirtualLayout, focused: WindowId, direction: Direction) -> Option<VirtualLayout>;
pub fn swap_with_offscreen(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<VirtualLayout>;
```

- `swap(Left/Right)`: swaps columns in the columns vec (also swaps `row_ratios`)
- `swap(Up/Down)`: swaps rows within a column (also swaps `row_ratios`)
- `swap_with_offscreen`: finds first off-screen column in direction, swaps columns, then scrolls to make focused column visible

### Resize

```rust
pub fn expand_column(layout: &VirtualLayout, focused: WindowId, direction: Direction) -> Option<VirtualLayout>;
pub fn shrink_column(layout: &VirtualLayout, focused: WindowId, direction: Direction) -> Option<VirtualLayout>;
pub fn set_column_width(layout: &VirtualLayout, focused: WindowId, eighths: u8, config: &MutationConfig) -> Option<VirtualLayout>;
```

- `expand_column`: focused column +1 eighth, neighbor in direction -1
- `shrink_column`: focused column -1 eighth, neighbor in direction +1
- `set_column_width`: sets explicit width, applies delta iteratively
- Returns `None` if either column would go outside 1–8 range
- Only `Left`/`Right` direction — `Up`/`Down` returns `None`

### Merge

```rust
pub fn merge_column_left(layout: &VirtualLayout, focused: WindowId) -> Option<VirtualLayout>;
pub fn merge_column_right(layout: &VirtualLayout, focused: WindowId) -> Option<VirtualLayout>;
```

Absorbs neighbor's rows into focused column. Row ratios are rebalanced to equal. The absorbed column is removed.

### Monocle

```rust
pub fn toggle_monocle(
    layout: &VirtualLayout,
    focused: WindowId,
    saved_width: Option<u8>,
) -> Option<(VirtualLayout, Option<u8>)>;
```

- Enter monocle: set column to 8/8, return saved original width
- Exit monocle: restore saved width (or default 4)
- Returns `(new_layout, saved_width)` tuple

### Window Lifecycle

```rust
pub fn add_window(layout: &VirtualLayout, window: WindowId, config: &MutationConfig) -> VirtualLayout;
pub fn add_window_to_column(layout: &VirtualLayout, col_idx: usize, window: WindowId) -> VirtualLayout;
pub fn remove_window(layout: &VirtualLayout, window: WindowId, config: &MutationConfig) -> VirtualLayout;
```

- `add_window`: appends new column to the right with `default_column_width_eighths`
- `add_window_to_column`: appends row, rebalances ratios to equal
- `remove_window`: removes from column. If column empties, removes column. Clamps viewport offset. Non-existent window = no-op.

---

## LayoutEngine Orchestrator

```rust
pub struct LayoutEngine {
    virtual_layout: VirtualLayout,
    focused: Option<WindowId>,
    prev_actual: ActualLayout,
    monitor: MonitorInfo,
    config: MutationConfig,
    monocle_saved_width: Option<(usize, u8)>,
}
```

### Constructor

```rust
pub fn new(monitor: MonitorInfo, default_column_width_eighths: u8, gaps: Gaps) -> Self;
```

Creates empty layout, projects initial (empty) actual layout for diff baseline.

### Mutation Pipeline

```text
User command
    │
    ▼
LayoutEngine.scroll_right() / .swap() / .add_window() / etc.
    │
    ▼
mutations::scroll_right() / .swap() / .add_window() / etc.
    │  (returns new VirtualLayout)
    ▼
projection::project()
    │  (returns new ActualLayout)
    ▼
diff::diff()
    │  (compares prev_actual vs new_actual)
    ▼
LayoutDiff { virtual_layout, actual_layout, moves }
```

Every mutation follows: **apply → project → diff → return**.

### Public Methods

```rust
// Accessors
pub fn virtual_layout(&self) -> &VirtualLayout;
pub fn focused(&self) -> Option<WindowId>;
pub fn monitor(&self) -> &MonitorInfo;

// Scroll
pub fn scroll_left(&mut self) -> Option<LayoutDiff>;
pub fn scroll_right(&mut self) -> Option<LayoutDiff>;

// Focus
pub fn focus(&mut self, direction: Direction) -> Option<WindowId>;
pub fn set_focus(&mut self, window: WindowId);

// Swap
pub fn swap(&mut self, direction: Direction) -> Option<LayoutDiff>;
pub fn swap_with_offscreen(&mut self, direction: Direction) -> Option<LayoutDiff>;

// Resize
pub fn expand_column(&mut self, direction: Direction) -> Option<LayoutDiff>;
pub fn shrink_column(&mut self, direction: Direction) -> Option<LayoutDiff>;
pub fn set_column_width(&mut self, eighths: u8) -> Option<LayoutDiff>;

// Merge
pub fn merge_column_left(&mut self) -> Option<LayoutDiff>;
pub fn merge_column_right(&mut self) -> Option<LayoutDiff>;

// Monocle
pub fn toggle_monocle(&mut self) -> Option<LayoutDiff>;

// Window lifecycle
pub fn add_window(&mut self, window: WindowId) -> LayoutDiff;
pub fn add_window_to_focused_column(&mut self, window: WindowId) -> Option<LayoutDiff>;
pub fn remove_window(&mut self, window: WindowId) -> LayoutDiff;
```

### Focus Management

- `add_window` auto-sets focus to new window
- `remove_window` falls focus back to first window of first column if the removed window was focused
- `focus()` returns the newly focused `WindowId`, auto-scrolls viewport if needed

### Monocle State

The engine tracks `(column_index, saved_width)` so monocle toggle correctly restores the original width even after focus changes.
