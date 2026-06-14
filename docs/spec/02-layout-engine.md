# LayoutEngine (`stm-layout`)

## Responsibility

`LayoutEngine` combines what was previously called `LayoutManager` and `AnimationCoordinator` into one crate. It:

- Owns the **Virtual Layout** — the full description of all tiling windows on the infinite canvas
- Computes the **Actual Layout** — the pixel-mapped on-screen slice of the virtual canvas
- Processes layout mutation commands (swap, scroll, resize column, etc.)
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
| `SwapLeft` / `SwapRight` | Swap focused window with its neighbour in the adjacent column. |
| `SwapUp` / `SwapDown` | Swap focused window with the sibling row in the same column. |
| `SwapColumn <direction>` | Swap the focused **column** with its neighbour. The viewport scrolls automatically via `ensure_column_visible` so the focused window stays visible — no separate "offscreen" command is needed. |
| `MoveWindow <direction>` | Semantic "move" — the daemon translates the intent by window state (tiled left/right = column swap; floating = pixel nudge once supported). |
| `ExpandColumn` / `ShrinkColumn` | Increase/decrease focused column's `width_eighths` by 1. Adjacent column adjusts to compensate. |
| `SetColumnWidth <eighths>` | Set focused column width explicitly. |
| `ResizeSnap <new_rect>` | Called after a mouse resize gesture. Snaps to nearest eighths width, adjusts neighbor. |
| `MoveSnap <new_rect>` | Called after a mouse move gesture. Determines target slot (insert into column), updates virtual layout. |
| `Promote <hwnd>` | Move a window from above-layout (overlay) layer back into tiling. |

### Column swap & viewport scrolling

`SwapColumn <direction>` swaps the focused column with its immediate neighbour.
After the swap, `ensure_column_visible` shifts `viewport_offset` if the focused
window's new column would be off-screen, so the swap and the camera scroll
produce a single `LayoutDiff`:

```
Before:                       After swap-column right:
  [viewport]                    [viewport]
  Col 0 | Col 1* | Col 2        Col 0 | Col 2 | Col 1*
         focused                          focused
```

The implementation:
1. Swap the two columns in the `columns` vec
2. Call `ensure_column_visible` to scroll the viewport if needed
3. Re-project → the swapped windows animate to their new positions

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

