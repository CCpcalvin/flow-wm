---
name: rust-implementation
description: >
  Teaches CoderAgent how to write Rust code for this ScrollingTilingManager
  binary targeting Windows — modules, commands, window management, Win32 API
  calls, message loops, and workspace tiling logic. Load at the start of every
  Rust implementation phase.
  Do NOT load for Python, TypeScript, or cross-platform crate work.
  Produces correct, idiomatic Rust that compiles cleanly on a Windows target
  with `cargo build --target x86_64-pc-windows-msvc`.
version: 1
---

# Rust Implementation Guide — ScrollingTilingManager (Windows Binary)

## Tech Stack

- **Rust stable (MSRV 1.78+)**
- **Target:** `x86_64-pc-windows-msvc` (MSVC toolchain, not GNU)
- **windows-rs** (`windows` crate ≥ 0.58) — Win32 / Windows UI Automation bindings
- **tokio** (optional, single-threaded `current_thread` runtime) — async I/O only
- **serde + serde_json** — config serialisation/deserialisation
- **cargo** — build, lint (`clippy`), format (`rustfmt`), test

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
| Module | `snake_case` | `mod win32_bridge;` |
| Binary crate root | `main.rs` | — |
| Integration test file | `tests/<subject>.rs` | `tests/layout_engine.rs` |

---

## 2 — Module Structure

```
src/
├── main.rs              # Entry point: parse args, init runtime, run message loop
├── config.rs            # Serde config structs + load_config()
├── layout/
│   ├── mod.rs           # pub use re-exports
│   ├── engine.rs        # Pure tiling logic (no Win32 imports)
│   └── types.rs         # Rect, Axis, Direction enums
├── win32/
│   ├── mod.rs           # pub use re-exports
│   ├── hook.rs          # SetWinEventHook / shell hook setup
│   ├── monitor.rs       # EnumDisplayMonitors, work-area queries
│   └── window.rs        # HWND wrappers, MoveWindow, ShowWindow
└── ipc.rs               # Named-pipe IPC for runtime commands (optional)
```

Rules:
- `layout/` MUST contain no `windows` crate imports. Keep tiling logic pure.
- `win32/` MUST contain no business/tiling logic — only FFI wrappers.
- `main.rs` wires the two together; no layout or Win32 logic lives there directly.

---

## 3 — Win32 Bindings

Read `references/win32-api.md` before writing any `win32/` code.

```rust
// src/win32/window.rs
use windows::Win32::Foundation::{HWND, RECT, BOOL};
use windows::Win32::UI::WindowsAndMessaging::{
    MoveWindow, ShowWindow, SW_RESTORE, WINDOW_STYLE, WS_VISIBLE,
    GetWindowRect, GetWindowLongW, GWL_STYLE,
};

pub fn move_window(hwnd: HWND, rect: &crate::layout::types::Rect, repaint: bool) -> windows::core::Result<()> {
    unsafe {
        MoveWindow(hwnd, rect.x, rect.y, rect.width, rect.height, repaint)?;
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
edition = "2021"

[[bin]]
name = "stm"
path = "src/main.rs"

[dependencies]
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_UI_WindowsAndMessaging",
    "Win32_Graphics_Gdi",
    "Win32_System_Threading",
] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
# tokio only if async IPC is needed:
# tokio = { version = "1", features = ["rt", "net", "sync"] }

[profile.release]
opt-level = 3
lto = true
strip = true

[target.'cfg(not(target_os = "windows"))'.dependencies]
# guard: this crate MUST NOT compile on non-Windows targets
# Add a compile_error! in build.rs instead of listing deps here
```

Rules:
- NEVER add features like `"Win32_Everything"` — enumerate exactly the features needed.
- `edition = "2021"` is mandatory.
- `strip = true` in release to produce a lean binary.
- Add a `build.rs` that emits `compile_error!` if target is not Windows (see `references/build-rs.md`).

---

## 5 — Message Loop Pattern

```rust
// src/main.rs (excerpt)
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, TranslateMessage, DispatchMessageW, MSG,
};

fn run_message_loop() {
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match ret.0 {
            -1 => {
                // GetMessage error — log and break
                break;
            }
            0 => break, // WM_QUIT
            _ => unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }
}
```

Rules:
- Check `GetMessageW` return as `i32`: -1 = error, 0 = WM_QUIT, positive = continue.
- NEVER call `std::thread::sleep` inside the message loop — it blocks Windows message delivery.
- Post `WM_QUIT` via `PostQuitMessage(0)` for graceful shutdown; never `std::process::exit`.

---

## 6 — Layout Engine (Pure Rust)

```rust
// src/layout/engine.rs
use crate::layout::types::{Rect, Axis};

/// Divide `parent` into `count` tiles along `axis` with `gap_px` between each.
pub fn split_rect(parent: Rect, count: usize, axis: Axis, gap_px: i32) -> Vec<Rect> {
    if count == 0 { return vec![]; }
    match axis {
        Axis::Horizontal => {
            let total_gap = gap_px * (count as i32 - 1);
            let tile_w = (parent.width - total_gap) / count as i32;
            (0..count).map(|i| Rect {
                x: parent.x + i as i32 * (tile_w + gap_px),
                y: parent.y,
                width: tile_w,
                height: parent.height,
            }).collect()
        }
        Axis::Vertical => {
            let total_gap = gap_px * (count as i32 - 1);
            let tile_h = (parent.height - total_gap) / count as i32;
            (0..count).map(|i| Rect {
                x: parent.x,
                y: parent.y + i as i32 * (tile_h + gap_px),
                width: parent.width,
                height: tile_h,
            }).collect()
        }
    }
}
```

Rules:
- Layout functions MUST be pure (`fn`, not `unsafe fn`, no I/O, no Win32).
- Use integer arithmetic for pixel positions — never `f32`/`f64` in layout math (rounding errors accumulate).
- All layout functions MUST have unit tests in `tests/layout_engine.rs` or inline `#[cfg(test)]` blocks.

---

## 7 — Error Handling

```rust
use std::fmt;

#[derive(Debug)]
pub enum StmError {
    Win32(windows::core::Error),
    Config(String),
    Layout(String),
}

impl fmt::Display for StmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win32(e)   => write!(f, "Win32 error: {e}"),
            Self::Config(s)  => write!(f, "Config error: {s}"),
            Self::Layout(s)  => write!(f, "Layout error: {s}"),
        }
    }
}

impl From<windows::core::Error> for StmError {
    fn from(e: windows::core::Error) -> Self { Self::Win32(e) }
}

pub type StmResult<T> = Result<T, StmError>;
```

Rules:
- NEVER use `.unwrap()` or `.expect()` outside of tests — use `?` or explicit match.
- Propagate `StmError` up to `main`, where it is logged before exiting with a non-zero code.
- `panic!` is reserved for logic invariants that cannot be recovered from.

---

## 8 — Rust Rules

- Rust stable only — no nightly features.
- All public items MUST have doc comments (`///`).
- Use `#[must_use]` on functions returning `Result` or meaningful values that callers might silently discard.
- No `std::mem::transmute` — use `windows-rs` typed conversions.
- Prefer `impl Trait` over `Box<dyn Trait>` for return types when the type is known statically.
- `clippy::pedantic` lints enabled in CI — fix or explicitly `#[allow]` with a comment.
- No `println!` in production paths — use the `log` crate with `env_logger` or `tracing`.

---

## 9 — Validation

After every change:

```powershell
cargo clippy --target x86_64-pc-windows-msvc -- -D warnings
cargo fmt --check
cargo test --target x86_64-pc-windows-msvc
```

Fix all issues before handoff. Never suppress a Clippy lint without an inline comment explaining why.

---

## Handoff Checklist

- [ ] Module boundaries respected (`layout/` pure, `win32/` FFI only)
- [ ] All `unsafe` blocks minimal-scope, single Win32 call each
- [ ] No `.unwrap()` / `.expect()` outside tests
- [ ] `cargo clippy --target x86_64-pc-windows-msvc -- -D warnings` exits 0
- [ ] `cargo fmt --check` exits 0
- [ ] `cargo test` passes (pure layout tests run on host; Win32 tests skipped on non-Windows CI)
- [ ] All public items have `///` doc comments
- [ ] Win32 feature flags are minimal — no blanket feature includes
