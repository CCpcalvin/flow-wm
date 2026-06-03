---
name: rust-review
description: >
  Teaches CodeReviewer to review Rust code for the ScrollingTilingManager
  Windows binary — module boundaries, Win32 safety, error handling, Clippy
  compliance, and test adequacy. Load at the start of every Rust review phase.
  Do NOT load for Python, TypeScript, or generic Rust crate reviews unrelated
  to Win32 / tiling logic.
  Produces an Approved or Rejected verdict with file + line references.
version: 1
---

# Rust Code Review Guide — ScrollingTilingManager (Windows Binary)

**Scope:** all `.rs` files from this batch's CoderAgents + test files and results.
Do NOT compile or run the binary — review source and test output only.

---

## 1 — Module Boundaries

Verify the dependency direction is respected:

```
main.rs → win32/ + layout/ + config.rs
win32/  → layout/types  (for Rect conversion only)
layout/ → (no win32 imports, no I/O)
```

- [ ] `src/layout/` contains zero `use windows` or `use std::os::windows` imports
- [ ] `src/win32/` contains no tiling/business logic — only FFI wrappers and type conversions
- [ ] `main.rs` contains no raw Win32 calls — delegates to `win32/` modules
- [ ] `config.rs` contains no Win32 imports

## 2 — Unsafe Correctness

- [ ] Every `unsafe` block is the minimal scope — wraps a single Win32 call expression, not a whole function body
- [ ] No `unsafe fn` signatures on public functions unless truly unavoidable (document why)
- [ ] No `std::mem::transmute` — typed `windows-rs` conversions used instead
- [ ] HWND is not stored as a raw pointer across thread boundaries; stored as `isize` and reconstructed at call site
- [ ] `SetProcessDpiAwarenessContext` called before any monitor/window queries

## 3 — Error Handling

- [ ] No `.unwrap()` or `.expect()` outside of `#[cfg(test)]` blocks
- [ ] All Win32 wrappers return `StmResult<T>` (or `windows::core::Result<T>` at the FFI boundary)
- [ ] `StmError` variants cover Win32, Config, and Layout domains
- [ ] `panic!` used only for true logic invariants with a comment explaining why recovery is impossible
- [ ] `main` logs `StmError` and exits with a non-zero code on fatal errors

## 4 — Win32 Patterns

- [ ] `GetMessageW` return checked as `i32`: -1 (error), 0 (WM_QUIT), positive (continue)
- [ ] `ShowWindow(hwnd, SW_RESTORE)` called before `MoveWindow` on any window that may be maximised
- [ ] `IVirtualDesktopManager` or equivalent used to filter windows not on the active virtual desktop
- [ ] Elevated-window `MoveWindow` failures are caught (return `FALSE` + `ERROR_ACCESS_DENIED`) and logged, not panicked
- [ ] UWP `ApplicationFrameWindow` child windows handled explicitly if tiling UWP apps is in scope

## 5 — Rust Quality

- [ ] All public items have `///` doc comments
- [ ] All functions have explicit parameter types and return types
- [ ] No bare `_` ignoring a `Result` — use `let _ =` only if intentional and commented
- [ ] No `println!` / `eprintln!` in production paths — `log` or `tracing` macros used
- [ ] `#[must_use]` present on functions returning `Result` or layout `Vec<Rect>`
- [ ] No nightly features used — `#![feature(...)]` absent
- [ ] Integer arithmetic used for pixel math — no `f32`/`f64` in layout calculations

## 6 — Cargo / Build

- [ ] `Cargo.toml` lists only the specific `windows` feature flags needed — no blanket includes
- [ ] `build.rs` emits `compile_error!` (or `panic!`) for non-Windows targets
- [ ] `edition = "2021"` set
- [ ] No `[patch.crates-io]` overrides without justification comment

## 7 — Clippy & Formatting

- [ ] `cargo clippy --target x86_64-pc-windows-msvc -- -D warnings` would exit 0 (verify via test output)
- [ ] `cargo fmt --check` would exit 0
- [ ] No `#[allow(clippy::...)]` without an inline comment explaining the exception

## 8 — Test Coverage

- [ ] Every new layout function has ≥ 1 happy-path + ≥ 1 edge-case unit test (count=0, gap=0, etc.)
- [ ] Every new Win32 wrapper has at least a compile-check or mock test
- [ ] Integration tests (`tests/*.rs`) exist for cross-module interactions (e.g., config load → layout apply)
- [ ] No test uses real HWNDs obtained at test time — mock or skip Win32-dependent paths on non-Windows CI

---

## Outcome

| Decision | Condition |
|---|---|
| ✅ Approved | All checks pass |
| ❌ Rejected | Any check fails — provide file + line number feedback |

Reject for: `unsafe` scope wider than one call, `.unwrap()` in production, Win32 logic in `layout/`, layout logic in `win32/`, missing error mapping, `println!` in production, missing `///` docs on public items, blanket `windows` feature flags.

Do not reject for style choices not defined in this skill.

---

## Gotchas

- **`unsafe` scope creep is the most common rejection**: Developers habitually wrap entire loop bodies or multi-call sequences in one `unsafe` block. Always check that each `unsafe` block contains exactly one Win32 call expression — reject anything wider.
- **`.unwrap()` in test helpers leaks into production**: Developers copy test fixture patterns into `src/`. Search for `.unwrap()` and `.expect()` in `src/` (not `tests/`) before approving — IDEs and Clippy don't flag these by default.
- **Blanket `"Win32_Everything"` feature flag**: Some developers copy a full feature list from examples. A blanket feature flag compiles in thousands of unused Win32 bindings, bloating the binary and increasing compile time. Reject if any feature is not referenced in the codebase.
- **`layout/` importing `windows` crate indirectly via a `use` re-export**: A developer may add `pub use windows::Win32::Foundation::RECT;` in `win32/mod.rs` and then import that from `layout/`. The import chain still breaks the module boundary — check `use` paths in layout files, not just direct `use windows` statements.
- **Missing `#[cfg(target_os = "windows")]` on Win32 test fixtures**: If a test file sets up a real HWND or calls `GetForegroundWindow`, it will fail to compile on Linux CI. Check every file in `tests/` that touches `win32/` code.
