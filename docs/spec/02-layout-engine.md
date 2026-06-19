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
    pub width_px: i32,          // pixel width on the virtual canvas (> 0)
    pub rows: Vec<WindowId>,    // top-to-bottom order within this column
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

Columns store their width directly in pixels (`width_px`). Position is **not stored** — it is derived via prefix-sum: column `i`'s left edge is the sum of all preceding columns' `width_px + window_gap`. This keeps the model lightweight (only the delta that matters) and makes reordering (swap) a simple `Vec::swap` with no coordinate arithmetic.

The `width_px` field is bounded by mutation-layer config (`[min_column_width_px, abs_max_width]`), not enforced by the `Column` type itself — the layout types are config-agnostic.

### Why pixel widths (not proportional units)

Earlier revisions stored widths as eighths of the base `column_width` to stay resolution-independent. This quantization caused a subtle bug: `expand_column`/`shrink_column` computed the correct pixel target (`column_width + window_gap`), then `pixels_to_eighths` re-quantized onto the `column_width` grid, **discarding the gap** (it was smaller than one eighth and rounded away). So each expand step grew by exactly `column_width` instead of `column_width + window_gap`. Pixel widths make the gap observable at every step and fix the bug. The cost — dependence on `column_width` and monitor resolution — is accepted; the `window_gap` is already pixel-based everywhere else.

---

## Virtual → Actual Projection

Given `VirtualLayout`, projection works as follows:

1. Each column already carries its pixel width (`width_px`). Compute cumulative left edges via prefix-sum: `col_left(0) = window_gap; col_left(i) = col_right(i−1) + window_gap`.
2. Find all columns whose pixel range overlaps `[viewport_offset, viewport_offset + monitor_width]`
3. For each overlapping column, compute its rows' `Rect`s by dividing monitor height equally among rows.
4. Translate canvas-relative X coordinates to screen coordinates: `screen_x = canvas_x - viewport_offset + monitor_left`

Off-screen columns (no overlap with viewport) are **parked** just beyond the nearest viewport edge (one column-width beyond), rather than at far-off coordinates — this keeps OS window management well-behaved and produces smooth scroll-in animations.

---

## Layout Mutations

All mutations take the current `VirtualLayout`, compute a new one, re-project to `ActualLayout`, diff against the previous `ActualLayout`, and emit a `LayoutDiff`.

### Supported mutations

| Command | Description |
|---|---|
| `ScrollLeft` / `ScrollRight` | Shift `viewport_offset` by one column step (free-form pixel offset, not snapped to a grid). Windows animate on/off screen. |
| `FocusLeft` / `FocusRight` / `FocusUp` / `FocusDown` | Move focus. If focus would leave viewport, `ensure_column_visible` shifts the camera with a free-form offset to reveal the target column. |
| `SwapLeft` / `SwapRight` | Swap focused window with its neighbour in the adjacent column. |
| `SwapUp` / `SwapDown` | Swap focused window with the sibling row in the same column. |
| `SwapColumn <direction>` | Swap the focused **column** with its neighbour. The viewport scrolls automatically via `ensure_column_visible` so the focused window stays visible — no separate "offscreen" command is needed. |
| `MoveWindow <direction>` | Semantic "move" — the daemon translates the intent by window state (tiled left/right = column swap; floating = pixel nudge once supported). |
| `ExpandColumn` / `ShrinkColumn` | Expand/shrink the focused column by one slot on the **F4 ladder**: `column_shift = column_width + window_gap`. At `slot_max` the next expand jumps to `abs_max_width` (`monitor_width − 2*gap`); at `abs_max_width` it is a no-op. Shrink reverses. Monocle = `abs_max_width`. Independent — no neighbor compensation. |
| `SetColumnWidth <px>` | Set focused column width to an explicit pixel value (free-form). Validated against `[min_column_width_px, abs_max_width]`. No quantization to any grid. |
| `ResizeSnap <new_rect>` | Called after a mouse resize gesture. Computes pixel delta and delegates to `set_column_width` (free-form px, bounded by min/max). |
| `MoveSnap <new_rect>` | Called after a mouse move gesture. Determines target slot (insert into column), updates virtual layout. |

| `Promote <hwnd>` | Move a window from above-layout (overlay) layer back into tiling. |

### Column swap & viewport scrolling

`SwapColumn <direction>` swaps the focused column with its immediate neighbour.
After the swap, `ensure_column_visible` shifts `viewport_offset` if the focused
window's new column would be off-screen. The offset is **free-form** — it is
computed as the minimum scroll that reveals the target column with a
`window_gap` margin, clamped to ≥ 0. It is not snapped to any column-width
grid. This means the swap and camera scroll produce a single `LayoutDiff`:

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

