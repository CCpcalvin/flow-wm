# Config Reference

FlowWM splits configuration across two TOML files in your config directory
(`%USERPROFILE%\.config\flow\` by default):

| File | Controls |
|------|----------|
| `flow.toml` | Geometry, padding, animation, borders, floating defaults, focus reconciliation. |
| `flow-rules.toml` | Window classification — which apps tile, which float, which are ignored entirely. |

Both files are optional — `flow config init` writes commented defaults, but you
can delete either file (or run with an empty one) and the daemon falls back to
compiled-in defaults. Your `flow.toml` may be partial: missing fields are
filled in from `FlowConfig::default()`.

> This page is the **user-facing** reference: every field, its default, what it
> does. For the internals — config directory resolution, the 4-phase load
> lifecycle, hot-reload semantics, validate-before-apply — see
> [Config & Persistence](../dev-guide/config-and-persistence.md) in the
> Developer Guide.

## Editing and reloading

Edit either file in any text editor. To pick up changes without restarting the
daemon:

```powershell
flow config reload
```

The daemon reloads `flow.toml` and `flow-rules.toml` from disk, validates them
**before** touching any state, and applies the live-reloadable fields. A
broken file produces an error on your terminal and leaves the running daemon
untouched — there is nothing to roll back.

To validate without loading:

```powershell
flow config check
```

### Live-reloadable vs. structural fields

Not every field can change at runtime. Reload applies **live** fields in place;
**structural** fields require a daemon restart.

| Field | Reload effect |
|-------|---------------|
| `borders.*` (color / thickness / overlap) | Immediate recolor/resize of all border overlays. |
| `padding.window_gap` | Re-projects every workspace's pixel layout — windows animate to the new gaps. |
| `columns_per_screen` | Updates the cached threshold (no window movement; affects future auto-center). |
| `column_width`, `min_column_width_px`, `min_window_height_px` | Update cached bounds; existing per-column widths preserved. |
| `animation.*` | New duration / easing / enabled flag applies to the next animation. |
| **Workspace count (10), monitor geometry** | **Structural** — not reloadable. Restart the daemon. |

What is preserved across reload: the user's arranged layout (columns, rows,
window order), focus, scroll viewport, monocle state, the daemon's window
registry, learned classification rules, and float positions.

### Rules reload is non-fatal

If `flow-rules.toml` fails to load, the daemon **keeps the current rules** and
warns on stderr; `flow.toml` (if valid) still reloads. This per-file-independent
policy is intentional. Reloaded rules affect only **newly opened** windows —
existing layout is never disrupted.

---

## `flow.toml`

The application config. Schema:
[`schemas/flow-config.schema.json`](https://github.com/CCpcalvin/flow-wm/blob/master/schemas/flow-config.schema.json).
A hand-written, fully-commented example lives at
[`default-config.toml`](https://github.com/CCpcalvin/flow-wm/blob/master/default-config.toml)
in the repo root — `flow config init` copies it verbatim.

> **Single source of truth.** Default values live in code, in the `Default`
> impls in `src/config/types.rs`. The `default-config.toml` example is enforced
> by a test (`default_config_toml_matches_compiled_defaults`) to deserialise to
> exactly `FlowConfig::default()`. If you ever wonder "what's the real
> default?", trust the table below over an outdated copy you find in the wild.

### Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `columns_per_screen` | `u32` | `4` | How many columns fit side-by-side on one monitor. The pixel width of each column is derived at runtime from this, the monitor resolution, and `padding.window_gap`. |
| `column_width` | `Option<u32>` | `None` | Optional fixed pixel width per column. When set, ignores `columns_per_screen`. Useful on ultrawide monitors where you want consistent column sizes regardless of monitor count. |
| `min_column_width_px` | `u32` | `640` | Floor for column width in pixels. Columns cannot be resized below this. |
| `min_window_height_px` | `u32` | `100` | Minimum row height in pixels — the floor for any single window's allocated height inside a column. Bounds the maximum number of rows that can stack inside one column. |
| `check_for_updates` | `bool` | `true` | Whether `flow start` checks GitHub for a newer release and prints a one-line notification prompting `flow update` when one exists. Runs after the daemon is ready, is bounded by a short timeout, and never blocks startup. The explicit `flow update --check` command is unaffected by this flag. |

Example:

```toml
columns_per_screen = 4
# column_width = 1280          # optional fixed width; ignores columns_per_screen
min_column_width_px = 640
min_window_height_px = 100
check_for_updates = true       # set false to silence the start-time update notification
```

### `[padding]` — spacing

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `window_gap` | `u32` | `16` | Uniform gap (pixels) between all elements: window-to-window, window-to-screen-edge. |
| `up` | `u32` | `0` | Reserved space (pixels) above the tiling area. |
| `down` | `u32` | `0` | Reserved space (pixels) below the tiling area — e.g. for taskbar clearance. |

Example:

```toml
[padding]
window_gap = 16
up = 0
down = 0
```

### `[animation]` — layout transitions

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `true` | Master switch. `false` = layout changes snap instantly. |
| `duration_ms` | `u32` | `240` | Transition duration in milliseconds. |
| `easing` | `ConfigEasing` | `"ease-out-expo"` | Easing function name. See below for accepted values. |

Accepted `easing` values (one of the `ConfigEasing` enum variants):

- `"linear"`
- `"ease-in"`, `"ease-out"`, `"ease-in-out"`
- `"ease-in-quad"`, `"ease-out-quad"`, `"ease-in-out-quad"`
- `"ease-in-cubic"`, `"ease-out-cubic"`, `"ease-in-out-cubic"`
- `"ease-in-expo"`, `"ease-out-expo"`, `"ease-in-out-expo"`
- `"constant"` (jumps to the end value immediately — equivalent to `enabled = false` for the motion itself, but still goes through the animator)

Example:

```toml
[animation]
enabled = true
duration_ms = 240
easing = "ease-out-expo"
```

### `[minimize_restore]` — bringing minimized windows back

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `strategy` | `MinimizeRestoreStrategy` | `"original_slot"` | Where a minimized tiling window goes when restored. See below. |

Accepted `strategy` values:

- `"original_slot"` — Restore to the exact column/row it was minimized from.
- `"right_of_focused"` — Restore into a new slot immediately to the right of the currently focused window.
- `"append_right"` — Append to the rightmost column.

Example:

```toml
[minimize_restore]
strategy = "original_slot"
```

### `[borders]` — window border overlays

Each managed window can get a thin colored ring drawn as a separate layered
overlay window that follows the target's geometry. This produces sharp
komorebi/Hyprland-style borders that the default Windows drop-shadow cannot.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `true` | Master switch. `false` = no overlay windows are created. |
| `thickness` | `u32` | `3` | Border ring width in pixels, applied on all four sides. |
| `overlap` | `u32` | `1` | Pixels the ring overlaps the window content. `0` = ring sits entirely in the gap; `thickness` = ring fills the slot edge-to-edge (closes the DWM hairline). |
| `focused_color` | `String` | `"#00AAFF"` | Border color for the focused/active window. Hex `#RRGGBB`. |
| `unfocused_color` | `String` | `"#555555"` | Border color for tiled-but-not-focused windows. Hex `#RRGGBB`. |
| `floating_color` | `String` | `"#AA00FF"` | Border color for floating windows. Hex `#RRGGBB`. |

Example:

```toml
[borders]
enabled = true
thickness = 3
overlap = 1
focused_color = "#00AAFF"
unfocused_color = "#555555"
floating_color = "#AA00FF"
```

### `[floating]` — default floating window size

Both fields optional. When unset, the daemon falls back to a built-in policy:
60% × 80% of the monitor work area, capped at roughly 1536 × 1152 (QHD-derived)
so ultrawide / 4K monitors don't produce absurdly large popups. An explicit
pixel value below is always respected as-is — the cap applies only to the
fallback path.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_width` | `Option<u32>` | `None` | Explicit default width (pixels) for newly floated windows. |
| `default_height` | `Option<u32>` | `None` | Explicit default height (pixels) for newly floated windows. |

Example:

```toml
[floating]
default_width = 1200
default_height = 800
```

### `[focus]` — foreground reconciliation

`EVENT_SYSTEM_FOREGROUND` is a best-effort stream from Windows: under rapid
window churn (e.g. a browser closing multiple tabs), the OS can settle the
foreground without emitting the final event, leaving flow's internal focus
stuck on a stale window. The daemon closes that gap by periodically
reconciling its tracked focus against the authoritative `GetForegroundWindow()`
query.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `foreground_sync_interval_ms` | `u32` | `250` | Interval (milliseconds) between reconciliation passes. The common in-sync pass is a single microsecond-scale read that no-ops. |

Example:

```toml
[focus]
foreground_sync_interval_ms = 250
```

---

## `flow-rules.toml`

Window classification rules — which windows FlowWM should tile, float, or
ignore entirely. Rules are evaluated **top-to-bottom, first match wins**. If no
rule matches, `default_action` is used.

Schema:
[`schemas/flow-rules.schema.json`](https://github.com/CCpcalvin/flow-wm/blob/master/schemas/flow-rules.schema.json).

> FlowWM ships a set of built-in defaults for common Windows system windows
> (taskbar, Search UI, system dialogs, Task Manager, on-screen keyboard,
> Chromium legacy windows, etc.). These are embedded into the binary at
> compile time — they cannot be deleted or corrupted at runtime — and your
> `flow-rules.toml` is checked **first**, so your rules always outrank the
> built-in ones. See
> [`default-flow-rules.toml`](https://github.com/CCpcalvin/flow-wm/blob/master/default-flow-rules.toml)
> in the repo root for the full list of what's ignored by default.

### Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_action` | `WindowAction` | `"float"` | Action applied when no rule matches a new window. |

Accepted `default_action` values:

- `"tile"` — New windows enter the scrolling column layout.
- `"float"` — New windows float freely (FlowWM's default — most apps are written to float).
- `"ignore"` — FlowWM pretends the window doesn't exist. No border, no layout slot, no focus tracking.

### `[[rules]]` — individual rules

Each rule is a table in the `rules` array. Every rule has a `match` clause
(fields AND'ed together) and an `action`.

#### Match fields

All match fields are optional. Multiple fields on the same rule are combined
with AND logic — all must match.

| Field | Type | Description |
|-------|------|-------------|
| `exe` | `String` | Executable name to match (e.g. `"chrome.exe"`). Case-insensitive on the filename. |
| `title` | `String` | Window title substring to match. |
| `class` | `String` | Win32 window class name to match. Often the most reliable identifier — see note below. |

> **Prefer `class` over `exe` + `title` for system helpers.** The class is
> assigned at window creation, while the title may be set a few milliseconds
> later. Class matching is therefore race-free; title matching can miss a
> window classified at creation time before its title arrives. This is why the
> bundled defaults match `Chrome_RenderWidgetHostHWND` by class — a single
> rule covers every Chromium host (Chrome, Edge, VS Code, Electron apps…)
> regardless of exe name.

#### Action field

Each rule's `action` is one of the `WindowAction` values (`"tile"` / `"float"`
/ `"ignore"`), same as `default_action`.

### Example

```toml
#:schema ./schemas/flow-rules.schema.json

default_action = "float"

# Tile my terminal and editor unconditionally
[[rules]]
match = { exe = "WindowsTerminal.exe" }
action = "tile"

[[rules]]
match = { exe = "Code.exe" }
action = "tile"

# Float the Steam friends list
[[rules]]
match = { exe = "steamwebhelper.exe", title = "Friends List" }
action = "float"

# Ignore a misbehaving helper window from some app
[[rules]]
match = { class = "SomeHelperClass" }
action = "ignore"
```

### How classification actually works (short version)

When a new window appears, FlowWM checks, in order:

1. **User rules** — your `flow-rules.toml`, top-to-bottom, first match wins.
2. **Learned rules** — machine-written to `history-flow-rules.toml`. Whenever
   you press `Alt + T` on a floating window (or float a tiled one),
   FlowWM records your choice and applies it automatically next time you open
   that app.
3. **Bundled defaults** — `default-flow-rules.toml`, embedded in the binary.
4. **`default_action`** — if nothing above matched.

You never edit `history-flow-rules.toml` directly; it's managed by the daemon.
To "forget" a learned choice, delete the file (the daemon recreates it empty on
the next launch) or add an explicit rule for that app in `flow-rules.toml` —
user rules always outrank learned rules. For the full algorithm, see
[Classification & Learned Rules](../dev-guide/classification.md) in the
Developer Guide.

---

## Config directory resolution

The directory FlowWM reads for `flow.toml` and `flow-rules.toml` resolves in
this priority order:

1. `--config <dir>` flag on the CLI (highest priority)
2. `FLOW_CONFIG_DIR` environment variable
3. Default: `%USERPROFILE%\.config\flow\`

The `logs/` folder (where the daemon writes its date-stamped log) lives inside
the config directory, so overriding the config dir moves the logs too.

For the full lifecycle — init, load, validate, use, hot-reload — see
[Config & Persistence](../dev-guide/config-and-persistence.md) in the
Developer Guide.
