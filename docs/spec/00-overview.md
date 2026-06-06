# ScrollingTilingManager — Developer Wiki

## Project Overview

`stm` (ScrollingTilingManager) is a tiling window manager for Windows 10/11 built around a **scrolling, infinite-horizontal-canvas** layout model. Unlike grid-based managers such as glazeWM or komorebi, windows occupy columns on a virtual canvas that can extend wider than any single monitor. The viewport slides left and right, bringing columns in and out of view.

The project ships as three binaries inside a **single Cargo package**:

| Binary | Role |
|---|---|
| `stmd` | The daemon process. Runs in the background and owns all state. |
| `stm` | The CLI client. Sends commands to the daemon, queries state, and edits config. |
| `stm-watchdog` | The crash-recovery helper. Restores windows if the daemon dies unexpectedly. |

This project intentionally starts as **one package with multiple binaries and one shared library crate**. The repository does **not** begin as a multi-crate workspace because the internal subsystem boundaries are still evolving. Shared functionality lives in `src/lib.rs` and its modules. If some subsystem later becomes independently reusable or needs stronger isolation, it can be extracted into its own crate.

---

## Repository Layout

```text
stm/
├── Cargo.toml
├── config/
│   └── stm.example.yaml
├── docs/
│   └── wiki/
└── src/
    ├── main.rs                  # stmd daemon binary
    ├── lib.rs                   # shared library used by all binaries
    ├── bin/
    │   ├── stm.rs               # CLI client binary
    │   └── stm-watchdog.rs      # recovery helper binary
    ├── registry/                # WindowRegistry — OS sync, window state
    ├── layout/                  # LayoutEngine — virtual + actual layout, animation diff
    ├── input/                   # InputInterceptor — hotkeys, Super-drag
    ├── config/                  # Config parser, schema generation, config mutation helpers
    ├── persist/                 # Window memory (per-app learned state)
    ├── ipc/                     # Shared IPC message types and transport helpers
    ├── animation/               # Animation bridge (or wrapper around window-animation)
    └── common/                  # Shared types, geometry, error types, utilities
```

---

## Why a Single Package

The project has multiple binaries, but that does **not** require multiple crates. Rust supports one package containing:

- one library crate (`src/lib.rs`)
- one default binary (`src/main.rs`)
- additional binaries (`src/bin/*.rs`)

This layout is the recommended starting point for `stm` because:

- the daemon, CLI, and watchdog all share internal types and logic
- the internal architecture is still changing
- module boundaries are sufficient right now
- it keeps Cargo configuration and refactoring overhead low

The project should move to a workspace only if a module becomes independently reusable, needs a stricter API boundary, or causes meaningful compile-time pressure.

---

## Subsystem Map

```text
┌───────────────────────────────────────────────────────────────┐
│                         stmd (main.rs)                        │
│                                                               │
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────────┐   │
│  │ IPC Server   │   │ InputInter-  │   │ WindowRegistry   │   │
│  │ (src/ipc)    │   │ ceptor       │   │ (src/registry)   │   │
│  │              │   │ (src/input)  │   │                  │   │
│  └──────┬───────┘   └──────┬───────┘   └────────┬─────────┘   │
│         │                  │                    │             │
│         └──────────────────▼────────────────────▼             │
│                      ┌─────────────┐                          │
│                      │LayoutEngine │                          │
│                      │(src/layout) │                          │
│                      └──────┬──────┘                          │
│                             │                                 │
│                      ┌──────▼──────────────┐                  │
│                      │ Animation Bridge    │                  │
│                      │ (src/animation)     │                  │
│                      └─────────────────────┘                  │
│                                                               │
│  ┌──────────────┐   ┌──────────────┐                          │
│  │ src/config   │   │ src/persist  │                          │
│  └──────────────┘   └──────────────┘                          │
└───────────────────────────────────────────────────────────────┘
         ▲                         ▲
         │ shared lib.rs           │ shared lib.rs
         │                         │
         ▼                         ▼
┌────────────────┐        ┌──────────────────┐
│   stm CLI      │        │  stm-watchdog    │
│ (src/bin/stm)  │        │ (src/bin/...)    │
└────────────────┘        └──────────────────┘
```

---

## Core Concepts

### Virtual Layout vs Actual Layout

The **Virtual Layout** is the complete description of all tiling windows on an infinite horizontal canvas. Every tiling window has a column index, row index within that column, and a proportional width.

The **Actual Layout** is the subset of the Virtual Layout that maps to real screen pixel coordinates. It is computed by slicing the virtual layout at the current viewport offset and projecting column widths to pixel values.

Only windows present in the Actual Layout receive real positioning updates. Off-screen windows are parked at a large negative or positive X offset so they are physically invisible but still alive.

### Window States

```text
Window
├── Tiling
│   ├── Active
│   └── Minimized
├── Floating
│   ├── Active
│   └── Minimized
└── Ignored
    ├── Maximized
    └── Fullscreen
```

Transitions between `Tiling` and `Floating` are explicit commands only. They are not triggered by mouse dragging.

---

## Binary CLI Reference

Full command documentation: see `05-ipc-and-watchdog.md` and command examples in later wiki pages.

### `stmd`

```text
stmd                        Start the daemon (foreground)
stmd --config <path>        Use a custom config file path
```

### `stm`

```text
stm start
stm stop
stm enable-autostart
stm disable-autostart
stm reload-config
stm check-config
stm set <key> <value>
stm restore
stm query windows all
stm query layout virtual
stm query layout actual
stm query state
```

### `stm-watchdog`

```text
stm-watchdog --parent-pid <pid> --recovery-path <path>
```

This binary is normally launched by `stmd`, not by users directly.

---

## Building

STM is developed and built **natively on Windows** using the MSVC toolchain. No cross-compilation is needed.

### Prerequisites

1. **Rust toolchain** — install via [rustup](https://rustup.rs/):
   ```powershell
   rustup default stable-x86_64-pc-windows-msvc
   ```
2. **Visual Studio Build Tools** — the MSVC linker is required. Install the
   "Desktop development with C++" workload from
   [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/).

### Build Commands

```powershell
# Debug build (fast compile, useful during development)
cargo build

# Release build (optimised, stripped binary)
cargo build --release

# Run all tests
cargo test

# Lint
cargo clippy -- -D warnings

# Format check
cargo fmt --check
```

The resulting binaries are in `target/debug/` or `target/release/`:

| Binary | Path |
|--------|------|
| `stmd.exe` | `target/release/stmd.exe` |
| `stm.exe` | `target/release/stm.exe` |
| `stm-watchdog.exe` | `target/release/stm-watchdog.exe` |
