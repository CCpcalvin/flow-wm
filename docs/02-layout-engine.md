# LayoutEngine (`stm-layout`)

## Responsibility

`LayoutEngine` combines what was previously called `LayoutManager` and `AnimationCoordinator` into one crate. It:

- Owns the **Virtual Layout** — the full description of all tiling windows on the infinite canvas
- Computes the **Actual Layout** — the pixel-mapped on-screen slice of the virtual canvas
- Processes layout mutation commands (swap, scroll, resize column, merge, etc.)
- Diffs old vs new Actual Layout to produce a `Vec<WindowMove>` batch
- Passes the batch to `window-animation`

The separation between Virtual and Actual is the central design invariant. Nothing outside this crate ever sets window positions directly — all `SetWindowPos` calls are mediated through `window-animation` using the diff output.

---

## Data Structures

```rust
/// The complete virtual canvas
pub struct VirtualLayout {
    pub columns: Vec<Column>,
    pub viewport_offset: i32,   // pixel offset from left edge of canvas to left edge of monitor
}

pub struct Column {
    pub width_eighths: u8,      // 1–8, proportional to monitor width
    pub rows: Vec<HWND>,        // top-to-bottom order within this column
    pub row_ratios: Vec<f32>,   // height ratios summing to 1.0 (default: equal)
}

/// The on-screen projection of the virtual layout
pub struct ActualLayout {
    pub entries: Vec<ActualEntry>,
}

pub struct ActualEntry {
    pub hwnd: HWND,
    pub rect: Rect,             // absolute pixel coordinates on the monitor
}
```

The `width_eighths` field encodes the snap grid: 1 = 1/8 monitor width, 4 = half, 8 = full. Columns are stored in virtual left-to-right order. The viewport clips this sequence by `viewport_offset`.

---

## Virtual → Actual Projection

Given `VirtualLayout`, projection works as follows:

1. Compute the pixel width of each column: `col_px = (width_eighths / 8.0) * monitor_width`
2. Compute cumulative left edges for each column on the infinite canvas
3. Find all columns whose pixel range overlaps `[viewport_offset, viewport_offset + monitor_width]`
4. For each overlapping column, compute its rows' `Rect`s by dividing monitor height using `row_ratios`
5. Translate canvas-relative X coordinates to screen coordinates: `screen_x = canvas_x - viewport_offset + monitor_left`

Off-screen columns (no overlap with viewport) produce parked positions: `screen_x = monitor_left - 10000`.

---

## Layout Mutations

All mutations take the current `VirtualLayout`, compute a new one, re-project to `ActualLayout`, diff against the previous `ActualLayout`, and emit a `LayoutDiff`.

### Supported mutations

| Command | Description |
|---|---|
| `ScrollLeft` / `ScrollRight` | Shift `viewport_offset` by one column width. Windows animate on/off screen. |
| `FocusLeft` / `FocusRight` / `FocusUp` / `FocusDown` | Move focus. If focus would leave viewport, implicitly scroll. |
| `SwapLeft` / `SwapRight` | Swap focused window's column with adjacent column. Both columns animate. |
| `SwapUp` / `SwapDown` | Swap focused window with sibling row in same column. |
| `SwapWithOffscreen <direction>` | Focused window swaps with first window in the next off-screen column. Viewport shifts so that column comes into view; the displaced window moves to the vacated off-screen slot. |
| `ExpandColumn` / `ShrinkColumn` | Increase/decrease focused column's `width_eighths` by 1. Adjacent column adjusts to compensate. |
| `MergeColumnLeft` / `MergeColumnRight` | Absorb adjacent column's windows into focused column as new rows. Adjacent column disappears; remaining columns shift. |
| `SetColumnWidth <eighths>` | Set focused column width explicitly. |
| `ResizeSnap <new_rect>` | Called after a mouse resize gesture. Snaps to nearest eighths width, adjusts neighbor. |
| `MoveSnap <new_rect>` | Called after a mouse move gesture. Determines target slot (insert or merge), updates virtual layout. |
| `Promote <hwnd>` | Move a window from above-layout (overlay) layer back into tiling. |

### Cross-viewport swap detail

`SwapWithOffscreen` is the most complex mutation:

```
Before:
  [viewport]
  Col 0 | Col 1* | Col 2          Col 3 (off-screen right)
         focused

After ScrollRight one step + swap:
  [viewport]
  Col 1* | Col 2  | Col 3         Col 0 (off-screen left)
```

The actual implementation:
1. Determine the target off-screen column (first column outside viewport in the given direction)
2. Shift `viewport_offset` so target column enters the viewport
3. Swap the two columns in `columns` vec
4. Re-project → the swapped windows animate in from off-screen, the displaced windows animate out

---

## Overlay Layer

The overlay layer is a separate `Vec<OverlayWindow>` that lives above the tiling canvas, similar to virtual desktops or Niri's layer surfaces. Overlay windows float at a fixed position regardless of viewport offset. They are not part of `VirtualLayout`.

```rust
pub struct OverlayWindow {
    pub hwnd: HWND,
    pub rect: Rect,
    pub pinned: bool,  // true = stays visible regardless of workspace
}
```

`PlaceAbove` moves a tiling window into the overlay layer. `Promote` moves it back into tiling.

---

## Animation Diff

After every mutation, `LayoutEngine` calls:

```rust
fn diff(prev: &ActualLayout, next: &ActualLayout) -> Vec<WindowMove>
```

This produces the minimal set of window moves needed. Each `WindowMove` carries the HWND, target `Rect`, and an `AnimationHint` that tells `window-animation` which easing to use:

```rust
pub enum AnimationHint {
    Snap,           // snapped window itself — fast, springy
    Displaced,      // neighbor pushed out of the way — smooth, slightly slower
    ScrollEnter,    // window entering viewport from off-screen
    ScrollExit,     // window leaving viewport
    Restore,        // crash/minimize restore — no animation, instant
}
```

The entire `Vec<WindowMove>` is passed to `window-animation` in a single call, never piecemeal.

