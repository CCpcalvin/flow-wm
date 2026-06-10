---
name: rust-review
description: >
  Teaches CodeReviewer to review Rust code for the ScrollingTilingManager
  Windows binary — module boundaries, Win32 safety, error handling, Clippy
  compliance, and test adequacy. Load at the start of every Rust review phase.
  Do NOT load for Python, TypeScript, or generic Rust crate reviews unrelated
  to Win32 / tiling logic.
  Produces an Approved or Rejected verdict with file + line references.
version: 2
---

# Rust Code Review Guide — ScrollingTilingManager (Windows Binary)

**Scope:** all `.rs` files from this batch's CoderAgents + test files and results.
Do NOT compile or run the binary — review source and test output only.

---

## 1 — Module Boundaries

Verify the dependency direction is respected:

```
main.rs → registry/ + layout/ + config/ + ipc/ + animation/ + daemon/
registry/ → common/ (for StmError, WindowId)
layout/   → common/ (for WindowId) — NO registry/ or windows crate imports
animation/ → layout/types (for AnimationHint) + registry/ (for Win32 backends)
common/   → (no stm-internal imports — foundation layer)
```

- [ ] `src/layout/` contains zero `use windows` or `use std::os::windows` imports
- [ ] `src/registry/` contains no layout/business logic — only Win32 wrappers, HWND↔WindowId mapping, and window tracking
- [ ] `src/main.rs` contains no raw Win32 calls — delegates to `registry/` modules
- [ ] `src/config/` contains no Win32 imports
- [ ] `src/common/` imports nothing from other stm modules (foundation layer)
- [ ] `src/animation/` does not import layout engine internals — only `layout::types`

## 2 — Unsafe Correctness

- [ ] Every `unsafe` block is the minimal scope — wraps a single Win32 call expression, not a whole function body
- [ ] No `unsafe fn` signatures on public functions unless truly unavoidable (document why)
- [ ] No `std::mem::transmute` — typed `windows-rs` conversions used instead
- [ ] HWND is not stored as a raw pointer across thread boundaries; stored as `isize` and reconstructed at call site

## 3 — Error Handling

- [ ] No `.unwrap()` or `.expect()` outside of `#[cfg(test)]` blocks
- [ ] Win32 errors mapped to `StmError::Registry(String)` at the registry boundary
- [ ] I/O errors convert automatically via `From<std::io::Error>` impl
- [ ] `StmError` variants cover Config, Layout, Io, and Registry domains
- [ ] `panic!` used only for true logic invariants with a comment explaining why recovery is impossible
- [ ] `main` logs `StmError` and exits with a non-zero code on fatal errors

## 4 — Win32 Patterns

- [ ] `GetMessageW` return checked as `i32`: -1 (error), 0 (WM_QUIT), positive (continue)
- [ ] `ShowWindow(hwnd, SW_RESTORE)` called before `MoveWindow` on any window that may be maximised
- [ ] `IVirtualDesktopManager` used to filter windows not on the active virtual desktop
- [ ] Elevated-window `MoveWindow` failures caught and logged, not panicked
- [ ] No `#[cfg(target_os = "windows")]` guards — the project is Windows-only

## 5 — Rust Quality

- [ ] All public items have `///` doc comments explaining the "why", not just the "what"
- [ ] All functions have explicit parameter types and return types
- [ ] No bare `_` ignoring a `Result` — use `let _ =` only if intentional and commented
- [ ] No `println!` / `eprintln!` in production paths — `log` or `tracing` macros used
- [ ] `#[must_use]` present on functions returning `Result` or layout `Vec<Rect>`
- [ ] No nightly features used — `#![feature(...)]` absent
- [ ] Integer arithmetic used for pixel math — no `f32`/`f64` in layout calculations

## 6 — Cargo / Build

- [ ] `Cargo.toml` lists only the specific `windows` feature flags needed — no blanket includes
- [ ] `build.rs` panics for non-Windows targets (the sole platform gate)
- [ ] `edition = "2024"` set
- [ ] No `[patch.crates-io]` overrides without justification comment

## 7 — Clippy & Formatting

- [ ] `cargo clippy -- -D warnings` would exit 0 (verify via test output)
- [ ] `cargo fmt --check` would exit 0
- [ ] No `#[allow(clippy::...)]` without an inline comment explaining the exception

## 8 — Test Coverage

- [ ] Every new layout function has ≥ 1 happy-path + ≥ 1 edge-case unit test
- [ ] Every new Win32 wrapper has at least a mock test
- [ ] Integration tests (`tests/*.rs`) exist for cross-module interactions
- [ ] TestEngineer has analyzed coverage gaps and written missing tests
- [ ] No `#[cfg(target_os)]` guards in test code

---

## Outcome

| Decision | Condition |
|---|---|
| ✅ Approved | All checks pass |
| ❌ Rejected | Any check fails — provide file + line number feedback |

Reject for: `unsafe` scope wider than one call, `.unwrap()` in production, Win32 logic in `layout/`, layout logic in `registry/`, missing error mapping, `println!` in production, missing `///` docs on public items, blanket `windows` feature flags, `#[cfg(target_os)]` guards in source code.

Do not reject for style choices not defined in this skill.

---

## Gotchas

- **`unsafe` scope creep is the most common rejection**: Developers habitually wrap entire loop bodies or multi-call sequences in one `unsafe` block. Always check that each `unsafe` block contains exactly one Win32 call expression — reject anything wider.
- **`.unwrap()` in test helpers leaks into production**: Developers copy test fixture patterns into `src/`. Search for `.unwrap()` and `.expect()` in `src/` (not `tests/`) before approving — IDEs and Clippy don't flag these by default.
- **Blanket `"Win32_Everything"` feature flag**: A blanket feature flag compiles in thousands of unused Win32 bindings, bloating binary and compile time. Reject if any feature is not referenced in the codebase.
- **`layout/` importing `windows` crate indirectly via a `use` re-export**: A developer may add `pub use windows::Win32::Foundation::RECT;` in `registry/mod.rs` and then import that from `layout/`. The import chain still breaks the module boundary — check `use` paths in layout files, not just direct `use windows` statements.
- **`StmError::Win32` does not exist**: The project does not have a `Win32` error variant. Win32 errors are mapped to `StmError::Registry(String)` at the boundary. Reject code that introduces a `Win32(windows::core::Error)` variant.
