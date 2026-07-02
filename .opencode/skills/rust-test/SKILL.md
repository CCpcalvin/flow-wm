---
name: rust-test
description: >
  Teaches TestEngineer to analyze test coverage, write missing unit/integration
  tests, and run the full cargo test suite for the FlowWM Rust
  Windows binary. Load at the start of every Rust test phase.
  Do NOT load for Python pytest, TypeScript Vitest, or generic Rust crate testing
  unrelated to Win32 / tiling logic.
  Produces passing tests, coverage gap analysis, and a full `cargo test` report.
version: 2
---

# Rust Testing Guide — FlowWM (Windows Binary)

**Scope:** coverage gap analysis, writing missing tests, running the full suite.
This project is **Windows-only** — no `#[cfg(target_os)]` guards are needed. All
tests compile and run on Windows without platform conditionals.

**Test runner:** `cargo test` (native MSVC toolchain on Windows).

---

## 1 — TestEngineer Workflow

TestEngineer operates in three phases:

1. **Analyze** — Scan source modules for test coverage gaps.
2. **Create** — Write missing unit tests, integration tests, and edge-case tests.
3. **Run** — Execute the full suite and report results.

### Phase 1: Analyze Coverage

For each source module, check whether adequate tests exist:

1. List all `pub fn` and `pub struct` items in the module.
2. Check for inline `#[cfg(test)] mod tests` blocks.
3. Check for integration test files in `tests/` that exercise the module.
4. Identify functions/types with **zero** or **inadequate** test coverage.

**Adequacy criteria:**
- Every pure function: ≥ 1 nominal case + ≥ 1 edge case (zero, empty, overflow).
- Every config loader: ≥ 1 valid round-trip + ≥ 1 malformed input error test.
- Every error variant: ≥ 1 test that triggers it and verifies the variant.
- Every layout function: ≥ 1 property-based invariant test (e.g., tiles cover full area).

### Phase 2: Create Missing Tests

Write tests for every gap identified in Phase 1:

- Pure logic tests → inline `#[cfg(test)]` in the source file.
- Cross-module flow tests → integration test in `tests/`.
- Win32 wrapper tests → mock-based tests (see Section 4).

### Phase 3: Run and Report

```powershell
cargo test
```

Report: total / passed / failed / ignored. Full output for every failure.
Note any pre-existing broken tests separately from newly created ones.

---

## 2 — Test Categories

| Category | Location | What to test |
|---|---|---|
| Unit (pure) | Inline `#[cfg(test)]` in source file | Layout math, config parsing, error mapping, projection, diff |
| Integration | `tests/<subject>.rs` or `tests/<subject>/` | Cross-module flows (config → layout, hook → registry → engine) |
| Mock-based | Inline `#[cfg(test)]` or `tests/` | Win32 wrappers with mock backends |
| Layout correctness | `tests/layout_engine.rs` or inline | Property-based or table-driven split/tile correctness |
| CLI | `tests/cli.rs` + `tests/cli/*.rs` | Binary integration tests via `assert_cmd` |

---

## 3 — Inline Unit Tests

```rust
// src/workspace/scrolling_space.rs (excerpt)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::types::Rect;
    use crate::common::types::WindowId;

    // Local test helpers — see the real `test_monitor()` / `test_padding()`
    // in scrolling_space.rs for the canonical pattern.
    fn test_monitor() -> MonitorInfo { /* a small fake work area */ }
    fn test_padding() -> Padding { /* gap + up/down */ }

    #[test]
    fn add_window_creates_a_new_column() {
        let mut space = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        space.add_window(WindowId::new(1));
        assert_eq!(space.virtual_layout().columns.len(), 1);
    }

    #[test]
    fn remove_last_window_returns_empty_layout() {
        let mut space = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        space.add_window(WindowId::new(1));
        space.remove_window(&WindowId::new(1));
        assert!(space.virtual_layout().columns.is_empty());
    }

    #[test]
    fn actual_layout_with_no_windows_is_empty() {
        let space = ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4);
        assert!(space.actual_layout().entries.is_empty());
    }
}
```

Rules:
- Every layout function: ≥ 1 zero-count/empty edge case + ≥ 1 nominal case.
- Every config loader: ≥ 1 valid TOML round-trip + ≥ 1 malformed TOML error test.
- Every `FlowError` variant: ≥ 1 test that triggers it and checks the variant.

---

## 4 — Integration Tests

```rust
// tests/scrolling_space.rs
use flow_wm::workspace::ScrollingSpace;
use flow_wm::layout::types::{MonitorInfo, Padding};
use flow_wm::common::types::WindowId;

fn small_monitor() -> MonitorInfo { /* a small fake work area */ }
fn gapless_padding() -> Padding { Padding { window_gap: 0, up: 0, down: 0 } }

#[test]
fn tiles_cover_entire_monitor_area() {
    let monitor = small_monitor();
    let mut space = ScrollingSpace::new(monitor, 960, 320, gapless_padding(), 4);
    for i in 0..3 {
        space.add_window(WindowId::new(i));
    }
    let actual = space.actual_layout();
    // Verify no gaps between tiles (when gap = 0)
    // Property: sum of all tile widths == monitor width
    let _ = actual;
}

#[test]
fn tiles_do_not_overlap_with_gap() {
    let monitor = small_monitor();
    let mut space = ScrollingSpace::new(monitor, 960, 320, gapless_padding(), 4);
    for i in 0..4 {
        space.add_window(WindowId::new(i));
    }
    let actual = space.actual_layout();
    for pair in actual.entries.windows(2) {
        let right_edge = pair[0].rect.x + pair[0].rect.width;
        let next_left = pair[1].rect.x;
        assert!(right_edge <= next_left, "tiles overlap");
    }
}
```

---

## 5 — Win32 Mock Tests

Win32 wrappers cannot be called without a real Windows desktop session. Test them via trait-based dependency injection:

```rust
// src/animation/backend/mock.rs (pattern)
pub trait AnimationBackend {
    fn animate_move(&self, hwnd: isize, from: Rect, to: Rect, hint: &AnimationHint) -> FlowResult<()>;
}

// Production: uses SetWindowPos via registry/win32.rs
// Test: records calls without calling Win32
pub struct MockAnimationBackend {
    pub calls: std::cell::RefCell<Vec<(isize, Rect, Rect)>>,
}

impl AnimationBackend for MockAnimationBackend {
    fn animate_move(&self, hwnd: isize, from: Rect, to: Rect, _hint: &AnimationHint) -> FlowResult<()> {
        self.calls.borrow_mut().push((hwnd, from, to));
        Ok(())
    }
}
```

Rules:
- Mock implementations live inside `#[cfg(test)]` blocks or in `tests/` — never in production `src/` without the cfg guard.
- Never use `unsafe` in test code unless you are testing a specific unsafe invariant.

---

## 6 — Config Tests

```rust
// src/config/types.rs (inline test)
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_valid_config_round_trips() {
        let toml_str = r#"
[layout]
inner_gap = 8
outer_gap = 16
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml_str.as_bytes()).unwrap();
        let cfg = load_config(f.path()).unwrap();
        assert_eq!(cfg.layout.inner_gap, 8);
        assert_eq!(cfg.layout.outer_gap, 16);
    }

    #[test]
    fn load_malformed_toml_returns_config_error() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"not toml {{{{").unwrap();
        let err = load_config(f.path()).unwrap_err();
        assert!(matches!(err, crate::common::error::FlowError::Config(_)));
    }
}
```

Note: `tempfile` crate is a `[dev-dependencies]` only.

---

## 7 — Running the Suite

```powershell
# Full suite (all tests)
cargo test

# With output for failures
cargo test -- --nocapture

# Single test by name
cargo test test_name_pattern

# Integration tests only
cargo test --test cli
```

Report: total / passed / failed / ignored. Full output for every failure.
Note any pre-existing broken tests.

---

## 8 — What to Test / Skip

| ✅ Test | ❌ Skip |
|---|---|
| Layout arithmetic (projection, diff, split) | windows-rs crate internals |
| Config parse + validation + error mapping | MSVC linker / toolchain behaviour |
| Error variant construction and Display impl | Win32 API return values (mock instead) |
| Cross-module integration flows (IPC → engine → projection) | Logging output format |
| Edge cases (0 windows, negative gaps, max windows) | Release profile optimisations |
| CLI argument parsing via `assert_cmd` | Animation frame timing precision |
| WindowId ↔ HWND mapping in registry | COM object lifecycle internals |

---

## Handoff Checklist

- [ ] Coverage analysis completed — gaps identified and documented
- [ ] Missing tests written for all identified gaps
- [ ] Inline unit tests: every pure fn has ≥ 2 cases (nominal + edge)
- [ ] Integration tests in `tests/` for cross-module flows
- [ ] Config tests: valid round-trip + malformed error case
- [ ] `cargo test` exits 0 (all tests pass)
- [ ] `cargo clippy -- -D warnings` exits 0
- [ ] `cargo fmt --check` exits 0
- [ ] No `#[cfg(target_os)]` guards in any test code
- [ ] No `unwrap()` in test setup beyond fixture creation

---

## Gotchas

- **Layout integer rounding leaves uncovered pixels**: `projection` with non-divisible widths silently drops remainder pixels. Tests MUST assert that tiles cover the full area when gap=0, not just check one tile's width.
- **`tempfile` must stay in `[dev-dependencies]`**: It is easy to accidentally add `tempfile` to `[dependencies]` when copy-pasting. This links it into the release binary — always check `Cargo.toml` after adding config tests.
- **Mock structs outside `#[cfg(test)]` inflate binary size**: `MockWindowMover`, `MockAnimationBackend`, and similar test doubles MUST be inside `#[cfg(test)]` blocks or in `tests/` — never in `src/` without the cfg guard.
- **`ScrollingSpace` state carries over between tests**: If tests share a space instance via `#[fixture]` or lazy statics, a mutation in one test can poison the next. Always construct a fresh `ScrollingSpace::new(test_monitor(), 960, 320, test_padding(), 4)` in each test.
- **WindowId is opaque — tests must use arbitrary values**: `WindowId::new(42)` is fine for tests. The actual HWND value does not matter for pure logic tests.
- **`assert_cmd` tests require the binary to compile first**: CLI integration tests in `tests/cli.rs` run against the compiled `flowd` binary. If `main.rs` has compile errors, the entire CLI test suite fails with an opaque "process not found" error — fix compilation first.
