# Projection

The projection function converts the abstract virtual canvas into concrete pixel rectangles that Windows OS can render. Given a `VirtualLayout`, a `MonitorInfo`, and a `Padding`, the [`project`](../../src/layout/projection.rs) function computes an `ActualLayout` containing one entry per window with an exact screen-coordinate `Rect`. This is the only place in the layout module where pixel coordinates materialize — and the only place where padding is applied.

## The slot model

Columns are laid out in **slots** on the virtual canvas. Each slot is the column's own `width_px` plus a `window_gap` on the right. The canvas starts at `window_gap` (the initial left-edge gap), and the last column's trailing gap serves as the right-edge gap. This structure means the inter-column gap emerges from the layout itself, not from insetting individual windows.

```mermaid
graph LR
    subgraph Canvas["Virtual Canvas"]
        direction LR
        G0["gap"]
        C0["Column 0\n960px"]
        G1["gap"]
        C1["Column 1\n1440px"]
        G2["gap"]
        C2["Column 2\n960px"]
    end

    G0 --- C0 --- G1 --- C1 --- G2 --- C2

    style G0 fill:#fdd,stroke:#999
    style G1 fill:#fdd,stroke:#999
    style G2 fill:#fdd,stroke:#999
    style Canvas fill:#fff,stroke:#333
```

Column x-positions are a **prefix sum**, not a uniform stride. The canvas accumulator starts at `window_gap` and advances by `col.width_px + window_gap` per column. This is important because columns can have different widths (after expand/shrink or drag-resize), so a uniform stride would not work.

## Camera shift: canvas to screen

For each column, projection computes its canvas range `[canvas_left, canvas_right]` and checks whether it overlaps the viewport `[viewport_offset, viewport_offset + monitor_width]`. Visible columns are translated to screen coordinates by subtracting the camera offset:

```
screen_x = monitor_left + (canvas_col_left - viewport_offset)
```

This subtraction is the entire camera mechanism. The `viewport_offset` on the `VirtualLayout` shifts the slice of canvas that maps onto the physical screen. A column whose canvas x-position is less than the offset gets a negative (or off-screen-left) screen x; a column far to the right gets a screen x beyond the monitor right edge.

## Visibility test

A column is visible when its canvas range partially overlaps the viewport:

```
visible = (canvas_col_right > viewport_left) && (canvas_col_left < viewport_right)
```

This is a standard interval overlap test. Columns that overlap even by one pixel are projected at their real screen coordinates; columns with no overlap are parked.

## Row rects: equal-height division

Within a visible column, the monitor's available height (after subtracting `padding.up` and `padding.down`) is divided equally among the column's rows. Each window gets a `row_height = available_height / row_count`. This equal-division model is simple and predictable — all windows in a column are always the same height.

## The container model: from column cell to window rect

The relationship between the column's allocated cell and the actual window rect is where padding meets geometry. The window is inset within its row cell by `window_gap` on top and bottom:

```mermaid
graph TB
    subgraph RowCell["Row Cell (allocated height)"]
        direction TB
        MT["top margin: window_gap"]
        W["Window Rect\nx = col_x\ny = cell_y + gap\nwidth = col_width\nheight = row_height - 2*gap"]
        MB["bottom margin: window_gap"]
    end

    MT --- W --- MB

    style MT fill:#fdd,stroke:#999
    style MB fill:#fdd,stroke:#999
    style W fill:#dfd,stroke:#333,stroke-width:2px
```

Horizontally, the window fills the full `col_width` — there is no horizontal inset within the cell. The gap between columns comes from the slot model (the `window_gap` between slots), not from insetting the window. Vertically, the `window_gap` on top and bottom of each window produces `2 * window_gap` gap between adjacent rows and `window_gap` gap at the top and bottom edges of the tiling area.

## Screen-level margins

The `padding.up` and `padding.down` fields are screen-level margins that reserve space above and below the tiling area. They reduce the available height for row calculation: `available_height = monitor_height - up - down`. Windows never extend into these margin zones. This is distinct from `window_gap` (which applies between windows and between windows and screen edges within the tiling area) — `up`/`down` are for external UI elements like custom title bars or taskbar clearance.

## Parking

Columns that are not visible are **parked** at deterministic off-screen positions rather than left at their unreachable virtual canvas coordinates. Windows OS does not gracefully handle windows placed at extreme off-screen positions — they can cause rendering artifacts, accessibility issues, and broken animation transitions.

```mermaid
graph LR
    subgraph Monitor["Monitor"]
        VP["Viewport"]
    end

    PL["Left Parking\nx = monitor_left - col_width"]
    PR["Right Parking\nx = monitor_right"]

    PL -.->|"parked left"| Monitor
    Monitor -.->|"parked right"| PR

    style PL fill:#f9f,stroke:#333,stroke-dasharray: 5 5
    style PR fill:#f9f,stroke:#333,stroke-dasharray: 5 5
    style Monitor fill:#bbf,stroke:#333,stroke-width:2px
```

There are two parking zones:

- **Left parking**: `monitor_left - col_width`. For columns whose canvas right edge is at or before the viewport left edge.
- **Right parking**: `monitor_right`. For columns whose canvas left edge is at or beyond the viewport right edge.

Parked windows use the same padding and row-division logic as visible windows, so when a column scrolls from parked to visible (or vice versa), its windows animate smoothly with consistent dimensions — there is no sudden resize or padding change at the boundary.

## Why projection is the only place padding lives

Padding is applied exclusively during projection and never stored on `Column`, `VirtualLayout`, or individual windows. This design choice keeps the virtual canvas model clean: columns are pure containers of windows with widths, and the spatial realities of screen edges, gaps, and margins are deferred to the one function that converts canvas geometry to screen coordinates. The `ActualEntry` rects produced by projection are the **final HWND rects** — they can be passed directly to `SetWindowPos` without any further adjustment.

This separation has a practical benefit: the mutation layer operates on a padding-agnostic model. Swap, expand, shrink, and scroll never need to know about gaps or margins. Only the projection function — a single deterministic function with no side effects — handles the translation from the abstract to the physical.

See [Overview](./overview.md) for the VirtualLayout/ActualLayout type relationship and [Mutations](./mutations.md) for the operations that produce the `VirtualLayout` inputs to projection.
