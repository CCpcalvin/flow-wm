# ScrollingTilingManager — Developer Wiki

## Project Overview

`stm` (ScrollingTilingManager) is a tiling window manager for Windows 10/11 built around a **scrolling, infinite-horizontal-canvas** layout model. Unlike grid-based managers (glazeWM, komorebi), windows occupy columns on a virtual canvas wider than any single monitor. The viewport slides left and right, bringing columns in and out of view, inspired by Niri and PaperWM.

The project ships as two binaries:

| Binary | Role |
|--------|------|
| `stmd` | The daemon process. Runs in the background, owns all state. |
| `stm` | The CLI client. Sends commands to the daemon, queries state, and manages config. |

Communication between `stm` and `stmd` uses **Unix Domain Sockets (UDS)** — the same model as komorebi's `komorebic` / `komorebi` split.

---

## Repository Layout

```
stm/
├── crates/
│   ├── stmd/                  # Daemon binary (main entry point)
│   ├── stm-cli/               # CLI client binary
│   ├── stm-ipc/               # Shared IPC message types (serde + JSON Schema)
│   ├── stm-registry/          # WindowRegistry — OS sync, window state
│   ├── stm-layout/            # LayoutEngine — virtual + actual layout, animation diff
│   ├── stm-input/             # InputInterceptor — keyboard hooks, Super-drag
│   ├── stm-config/            # Config parser, schema, persistence
│   ├── stm-persist/           # Window memory (per-app learned state)
│   ├── stm-watchdog/          # Crash recovery watchdog process
│   └── window-animation/      # (separate crate) animation primitives
├── config/
│   └── stm.example.yaml
└── docs/
    └── wiki/                  # This documentation
```

---

## Subsystem Map

```
┌────────────────────────────────────────────────────────────────┐
│                          stmd (daemon)                         │
│                                                                │
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────────┐  │
│  │ IPC Server   │   │ InputInter-  │   │ WindowRegistry   │  │
│  │ (stm-ipc)   │   │ ceptor       │   │ (stm-registry)   │  │
│  │             │   │ (stm-input)  │   │                  │  │
│  └──────┬───────┘   └──────┬───────┘   └────────┬─────────┘  │
│         │                  │                    │             │
│         └──────────────────▼────────────────────▼             │
│                      ┌─────────────┐                          │
│                      │LayoutEngine │                          │
│                      │(stm-layout) │                          │
│                      └──────┬──────┘                          │
│                             │                                 │
│                      ┌──────▼──────────────┐                  │
│                      │  window-animation   │                  │
│                      └─────────────────────┘                  │
│                                                                │
│  ┌──────────────┐   ┌──────────────┐                          │
│  │ stm-config  │   │ stm-persist  │                          │
│  └──────────────┘   └──────────────┘                          │
└────────────────────────────────────────────────────────────────┘
         ▲
         │ UDS
         ▼
┌────────────────┐
│   stm (CLI)   │
│  (stm-cli)   │
└────────────────┘
```

---

## Core Concepts

### Virtual Layout vs Actual Layout

The **Virtual Layout** is the complete description of all tiling windows on an infinite horizontal canvas. Every tiling window has a column index, row index within that column, and a proportional width (expressed as eighths of the monitor width).

The **Actual Layout** is the subset of the Virtual Layout that maps to real screen pixel coordinates. It is computed by slicing the virtual layout at the current **viewport offset** and projecting column widths to pixel values.

Only windows present in the Actual Layout receive `SetWindowPos` calls. Off-screen windows are parked at a large negative X offset (e.g. `monitor_x - 10000`) so they are physically invisible but still alive.

### Window States

```
Window
├── Tiling
│   ├── Active        (has virtual slot, on-screen or parked off-screen)
│   └── Minimized     (slot released, remembered for restore)
├── Floating
│   ├── Active
│   └── Minimized
└── Ignored
    ├── Maximized     (IsZoomed == true; layout suspended)
    └── Fullscreen    (window rect == monitor rect, no WS_OVERLAPPEDWINDOW)
```

Transitions between `Tiling` and `Floating` are **explicit keyboard commands only** — not triggered by mouse dragging.

### Columns and Rows

A column is a vertical stack of one or more tiling windows. Columns are ordered left-to-right on the virtual canvas. Each column has a **proportional width** snapped to an eighths grid (1/8 … 8/8 of monitor width). Rows within a column share the column width and divide the monitor height equally (or by user-set ratios).

---

## Binary CLI Reference

Full command documentation: see `05-cli-reference.md`.

### `stmd`

```
stmd                        Start the daemon (foreground)
stmd --config <path>        Use a custom config file path
```

### `stm`

```
stm start                   Start stmd as a background process
stm stop                    Send shutdown command to stmd
stm enable-autostart        Register stmd in Windows Task Scheduler
stm disable-autostart       Remove autostart registration
stm reload-config           Hot-reload config without restarting
stm check-config            Validate config file and print errors
stm set <key> <value>       Directly mutate a config key (writes YAML)
stm restore                 Emergency: read recovery snapshot, bring all windows on-screen
stm query windows all       Dump all Window objects as JSON
stm query layout virtual    Dump the full virtual layout as JSON
stm query layout actual     Dump the current actual (on-screen) layout as JSON
stm query state             Dump full daemon state as JSON
```

`stm query` output is machine-readable JSON, suitable for status bars and external integrations.

