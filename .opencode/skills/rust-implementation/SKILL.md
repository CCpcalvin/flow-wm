---
name: rust-implementation
description: >
  Teaches CoderAgent how to write Rust code for this ScrollingTilingManager
  binary targeting Windows — modules, commands, window management, Win32 API
  calls, message loops, and workspace tiling logic. Load at the start of every
  Rust implementation phase.
  Do NOT load for Python, TypeScript, or cross-platform crate work.
  Produces correct, idiomatic Rust that compiles cleanly on Windows with
  `cargo build` (native MSVC toolchain, x86_64-pc-windows-msvc).
version: 2
---

# Rust Implementation Guide — ScrollingTilingManager (Windows Binary)

## Tech Stack

- **Rust stable (MSRV 1.82+)** — edition 2024
- **Target:** `x86_64-pc-windows-msvc` (MSVC toolchain, not GNU). Native build — no `--target` flag needed.
- **windows-rs** (`windows` crate 0.62.x) — Win32 / Windows UI Automation bindings
- **serde + serde_json** — config serialisation/deserialisation
- **clap** (derive) — CLI argument parsing
- **thiserror** — ergonomic error types
- **crossbeam-channel** — multi-producer multi-consumer channels
- **log + env_logger** — structured logging
- **toml** — TOML config parsing
- **regex** — pattern matching for window classification
- **schemars** — JSON Schema generation for config

This project is **Windows-only**. A `build.rs` gate prevents compilation on other platforms. Do NOT add `#[cfg(target_os = "windows")]` guards anywhere — the entire codebase assumes Windows.

---

## 1 — Naming

| Symbol | Style | Example |
|---|---|---|
| Module/file | `snake_case` | `tiling_manager.rs` |
| Struct | `PascalCase` | `WorkspaceLayout`, `WindowHandle` |
| Enum | `PascalCase` | `TileDirection`, `SplitAxis` |
| Trait | `PascalCase` | `Tiler`, `WindowProvider` |
| Function/method | `snake_case` | `apply_layout`, `get_hwnd` |
| Constant | `SCREAMING_SNAKE` | `MAX_COLUMNS`, `DEFAULT_GAP_PX` |
| Binary crate root | `main.rs` | — |
| Integration test file | `tests/<subject>.rs` | `tests/cli.rs` |

---

## 2 — Module Structure

```
src/
├── main.rs              # stmd daemon entry: parse args, init runtime, run event loop
├── lib.rs               # Library crate — re-exports all pub modules
│
├── common/              # Shared types and error definitions
│   ├── error.rs         # StmError enum, StmResult<T> alias
│   └── types.rs         # WindowId (platform-independent HWND bridge)
│
├── layout/              # Pure tiling logic — NO windows crate imports
│   ├── engine.rs        # LayoutEngine: virtual layout state + mutations
│   ├── types.rs         # VirtualLayout, ActualLayout, ActualEntry, AnimationHint
│   ├── projection.rs    # Virtual → Actual coordinate projection (pure fn)
│   ├── diff.rs          # ActualLayout diff → Vec<WindowMove> instructions
│   └── mutations.rs     # High-level mutation API (add_window, remove_window, etc.)
│
├── registry/            # Window tracking and Win32 bridge
│   ├── core.rs          # WindowRegistry: HWND ↔ WindowId mapping, window metadata
│   ├── classification.rs # Window class/title matching rules
│   ├── hooks.rs         # SetWinEventHook / shell hook setup
│   ├── desktop.rs       # Virtual-desktop detection (IVirtualDesktopManager COM)
│   ├── win32.rs         # Low-level Win32 FFI wrappers (MoveWindow, ShowWindow, etc.)
│   └── types.rs         # Registry-specific types (WindowInfo, WindowState)
│
├── config/              # Configuration loading and validation
│   ├── types.rs         # Serde config structs (AppConfig, LayoutConfig, etc.)
│   ├── lifecycle.rs     # Config file watching and hot-reload
│   ├── schema.rs        # JSON Schema generation via schemars
│   └── dirs.rs          # XDG-style config/log directory resolution
│
├── ipc/                 # Named-pipe IPC for stm CLI ↔ stmd daemon
│   ├── message.rs       # Request/Response enums
│   ├── transport.rs     # Named-pipe read/write framing
│   └── dispatch.rs      # Message → LayoutEngine mutation dispatch
│
├── animation/           # Window move animation system
│   ├── animator.rs      # Animation orchestration
│   ├── backend/         # Backend implementations
│   │   ├── win32.rs     # SetWindowPos-based animation backend
│   │   └── mock.rs      # Test-only animation backend
│   ├── batch.rs         # Batch animation scheduling
│   ├── config.rs        # Animation timing configuration
│   ├── easing.rs        # Easing functions (linear, ease-in-out, etc.)
│   ├── interpolation.rs # Frame interpolation
│   ├── metrics.rs       # Animation performance metrics
│   └── types.rs         # AnimationHint, AnimationFrame types
│
├── daemon/              # Daemon orchestration (event loop, startup, shutdown)
│   └── mod.rs
│
└── floating/            # Floating window management (non-tiled windows)
    └── mod.rs
```

### Module Boundary Rules

- **`layout/`** MUST contain zero `use windows` or `use std::os::windows` imports. It is pure Rust logic. It only knows about `WindowId`, never `HWND`.
- **`registry/`** contains all Win32 interop. `registry/win32.rs` holds raw FFI wrappers; `registry/core.rs` maps HWND ↔ WindowId and manages window state.
- **`common/`** is the shared foundation — error types and the `WindowId` bridge type. Both `layout/` and `registry/` may import from `common/`.
- **`animation/`** may import `layout/types.rs` (for `AnimationHint`) and `registry/` (for Win32 backends). It MUST NOT import layout engine internals.
- **`main.rs`** wires everything together; no layout or Win32 logic lives there directly.

---

## 3 — Win32 Bindings

Read `references/win32-api.md` before writing any `registry/win32.rs` code.

```rust
// src/registry/win32.rs
use windows::Win32::Foundation::{HWND, RECT, BOOL};
use windows::Win32::UI::WindowsAndMessaging::{
    MoveWindow, ShowWindow, SW_RESTORE, WINDOW_STYLE, WS_VISIBLE,
    GetWindowRect, GetWindowLongW, GWL_STYLE,
};

/// Move a window to the specified pixel coordinates.
pub fn move_window(hwnd: HWND, x: i32, y: i32, w: i32, h: i32, repaint: bool) -> windows::core::Result<()> {
    unsafe {
        MoveWindow(hwnd, x, y, w, h, repaint)?;
    }
    Ok(())
}
```

Rules:
- All `unsafe` blocks MUST be the smallest possible scope — wrap single Win32 calls, never entire functions.
- Return `windows::core::Result<T>` from every Win32 wrapper; never `.unwrap()` on HRESULT.
- HWND is `!Send + !Copy` — store handles as `isize` in cross-thread data structures; convert back with `HWND(handle)` at call site.
- Enable only the `windows` feature flags actually used — list them explicitly in `Cargo.toml`.

---

## 4 — Cargo.toml

```toml
[package]
name = "scrolling-tiling-manager"
version = "0.1.0"
edition = "2024"
description = "A scrolling, infinite-horizontal-canvas tiling window manager for Windows"

[[bin]]
name = "stmd"
path = "src/main.rs"

[[bin]]
name = "stm"
path = "src/bin/stm.rs"

[[bin]]
name = "stm-watchdog"
path = "src/bin/stm-watchdog.rs"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "0.8"
thiserror = "2"
log = "0.4"
env_logger = "0.11"
clap = { version = "4", features = ["derive"] }
windows = { version = "0.62", features = [
    "Win32_Foundation",
    "Win32_System_Pipes",
    "Win32_System_IO",
    "Win32_Storage_FileSystem",
    "Win32_Security",
    "Win32_UI_WindowsAndMessaging",
    "Win32_UI_Accessibility",
    "Win32_System_Threading",
    "Win32_System_StationsAndDesktops",
    "Win32_Graphics_Gdi",
    "Win32_Graphics_Dwm",
] }
regex = "1"
crossbeam-channel = "0.5"
toml = "1"

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
tempfile = "3"

[profile.release]
opt-level = 3
lto = true
strip = true

[package.metadata.docs.rs]
targets = ["x86_64-pc-windows-msvc"]
```

Rules:
- NEVER add features like `"Win32_Everything"` — enumerate exactly the features needed.
- `edition = "2024"` is mandatory.
- `strip = true` in release to produce a lean binary.
- Use `cargo add <crate>` for adding new dependencies — do NOT edit `Cargo.toml` directly.

---

## 5 — The 3-Layer Layout Pipeline

Layout computation follows a functional, declarative pipeline. Every mutation flows through: **mutate → project → diff**.

1. **Virtual Layer** (`layout/types::VirtualLayout`) — logical structure on an infinite horizontal canvas. Columns store proportional widths (`width_eighths`), not pixel positions.

2. **Projection** (`layout/projection::project`) — pure function that converts virtual layout into actual screen coordinates, applying all padding.

3. **Diff** (`layout/diff::diff`) — compares previous and new `ActualLayout` to produce `WindowMove` instructions with `AnimationHint`s.

```rust
// Example: adding a window to the layout
use crate::layout::engine::LayoutEngine;
use crate::common::types::WindowId;

fn handle_window_created(engine: &mut LayoutEngine, wid: WindowId) {
    engine.add_window(wid);           // Mutate virtual layout
    let actual = engine.project();    // Virtual → Actual coordinates
    let moves = engine.diff(&previous, &actual); // Diff → Vec<WindowMove>
    // Apply moves via animation system
}
```

Rules:
- Layout functions MUST be pure (`fn`, not `unsafe fn`, no I/O, no Win32).
- Use integer arithmetic for pixel positions — never `f32`/`f64` in layout math.
- All layout functions MUST have unit tests (inline `#[cfg(test)]` blocks or in `tests/`).

---

## 6 — Error Handling

```rust
// src/common/error.rs (actual code)
use std::fmt;

#[derive(Debug)]
pub enum StmError {
    Config(String),
    Layout(String),
    Io(std::io::Error),
    Registry(String),
}

impl fmt::Display for StmError { /* ... */ }
impl std::error::Error for StmError {}

pub type StmResult<T> = Result<T, StmError>;
```

Rules:
- NEVER use `.unwrap()` or `.expect()` outside of tests — use `?` or explicit match.
- Map Win32 errors to `StmError::Registry(String)` at the registry boundary.
- Map I/O errors using the `From<std::io::Error>` impl — they convert automatically with `?`.
- `panic!` is reserved for logic invariants that cannot be recovered from.

---

## 7 — Rust Rules

- Rust stable only — no nightly features.
- All public items MUST have doc comments (`///` or `//!`). Write docstrings that explain the "why" and design decisions, not just the "what" — `cargo doc` is the project's wiki.
- Use `#[must_use]` on functions returning `Result` or meaningful values that callers might silently discard.
- No `std::mem::transmute` — use `windows-rs` typed conversions.
- Prefer `impl Trait` over `Box<dyn Trait>` for return types when the type is known statically.
- No `println!` in production paths — use the `log` crate with `env_logger`.
- `build.rs` MUST panic for non-Windows targets. No `#[cfg(target_os)]` guards needed in source code.

---

## 8 — Validation

After every change:

```powershell
cargo clippy -- -D warnings
cargo fmt --check
cargo test
```

Fix all issues before handoff. Never suppress a Clippy lint without an inline comment explaining why.

---

## Handoff Checklist

- [ ] Module boundaries respected (`layout/` pure, `registry/` owns Win32, `common/` shared)
- [ ] All `unsafe` blocks minimal-scope, single Win32 call each
- [ ] No `.unwrap()` / `.expect()` outside tests
- [ ] `cargo clippy -- -D warnings` exits 0
- [ ] `cargo fmt --check` exits 0
- [ ] `cargo test` passes
- [ ] All public items have `///` doc comments
- [ ] Win32 feature flags are minimal — no blanket feature includes
- [ ] No `#[cfg(target_os)]` guards in source code
