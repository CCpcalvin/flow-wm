---
name: rust-test
description: >
  Teaches TestEngineer to write unit, integration, and layout-correctness tests
  for the ScrollingTilingManager Rust Windows binary and run the full cargo test
  suite. Load at the start of every Rust test phase.
  Do NOT load for Python pytest, TypeScript Vitest, or generic Rust crate testing
  unrelated to Win32 / tiling logic.
  Produces passing tests and a full `cargo test` report.
version: 1
---

# Rust Testing Guide — ScrollingTilingManager (Windows Binary)

**Scope:** unit tests (inline), integration tests (`tests/`), layout correctness, config round-trips, and full suite run.
CoderAgent owns per-function inline unit tests. Don't rewrite unless inadequate.

**Test runner:** `cargo test` (native MSVC toolchain on Windows).

---

## 1 — Test Categories

| Category | Location | What to test |
|---|---|---|
| Unit (pure) | Inline `#[cfg(test)]` in source file | Layout math, config parsing, error mapping |
| Integration | `tests/<subject>.rs` | Cross-module flows (config → layout, hook → manager) |
| Win32 mock | `tests/win32_mock.rs` | Win32 wrappers with mock HWND values (compile + logic only) |
| Layout correctness | `tests/layout_engine.rs` | Property-based or table-driven split/tile correctness |

Win32-dependent tests MUST be `#[cfg(target_os = "windows")]` so they compile-skip on Linux CI.

---

## 2 — Inline Unit Tests

```rust
// src/layout/engine.rs (excerpt)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::types::{Rect, Axis};

    #[test]
    fn split_rect_horizontal_two_tiles_no_gap() {
        let parent = Rect { x: 0, y: 0, width: 1920, height: 1080 };
        let tiles = split_rect(parent, 2, Axis::Horizontal, 0);
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].width, 960);
        assert_eq!(tiles[1].x, 960);
        assert_eq!(tiles[1].width, 960);
    }

    #[test]
    fn split_rect_zero_count_returns_empty() {
        let parent = Rect { x: 0, y: 0, width: 1920, height: 1080 };
        assert!(split_rect(parent, 0, Axis::Horizontal, 0).is_empty());
    }

    #[test]
    fn split_rect_gap_reduces_tile_width() {
        let parent = Rect { x: 0, y: 0, width: 1000, height: 500 };
        let tiles = split_rect(parent, 2, Axis::Horizontal, 10);
        // total gap = 10, total tile width = 990, each tile = 495
        assert_eq!(tiles[0].width, 495);
        assert_eq!(tiles[1].x, 505); // 495 + 10
    }
}
```

Rules:
- Every layout function: ≥ 1 zero-count/empty edge case + ≥ 1 nominal case.
- Every config loader: ≥ 1 valid JSON round-trip + ≥ 1 malformed JSON error test.
- Every error variant: ≥ 1 test that triggers it and checks the `StmError` variant.

---

## 3 — Integration Tests

```rust
// tests/layout_engine.rs
use scrolling_tiling_manager::layout::{engine::split_rect, types::{Rect, Axis}};

#[test]
fn full_monitor_tiles_cover_entire_area() {
    let monitor = Rect { x: 0, y: 0, width: 2560, height: 1440 };
    let tiles = split_rect(monitor, 3, Axis::Horizontal, 0);
    let covered_width: i32 = tiles.iter().map(|r| r.width).sum();
    assert_eq!(covered_width, monitor.width);
}

#[test]
fn tiles_do_not_overlap_with_gap() {
    let monitor = Rect { x: 0, y: 0, width: 1920, height: 1080 };
    let tiles = split_rect(monitor, 4, Axis::Horizontal, 8);
    for i in 0..tiles.len() - 1 {
        let right_edge = tiles[i].x + tiles[i].width;
        let next_left  = tiles[i + 1].x;
        assert!(right_edge < next_left, "tiles {i} and {} overlap", i + 1);
    }
}
```

---

## 4 — Win32 Mock Tests

Win32 wrappers cannot be called without a real Windows session. Test them by:
1. Extracting pure logic from the wrapper into a testable helper.
2. Wrapping the Win32 call behind a trait; inject a mock in tests.

```rust
// src/win32/window.rs
pub trait WindowMover {
    fn move_to(&self, hwnd: isize, rect: &crate::layout::types::Rect) -> crate::StmResult<()>;
}

pub struct RealWindowMover;
impl WindowMover for RealWindowMover {
    fn move_to(&self, hwnd: isize, rect: &crate::layout::types::Rect) -> crate::StmResult<()> {
        use windows::Win32::Foundation::HWND;
        unsafe { move_window(HWND(hwnd), rect, true) }
    }
}

// In tests:
#[cfg(test)]
struct MockWindowMover { pub calls: std::cell::RefCell<Vec<(isize, crate::layout::types::Rect)>> }
#[cfg(test)]
impl WindowMover for MockWindowMover {
    fn move_to(&self, hwnd: isize, rect: &crate::layout::types::Rect) -> crate::StmResult<()> {
        self.calls.borrow_mut().push((hwnd, *rect));
        Ok(())
    }
}
```

Rules:
- `#[cfg(target_os = "windows")]` guards any test that calls actual Win32.
- Mock implementations live inside `#[cfg(test)]` blocks only.
- Never use `unsafe` in test code unless you are testing a specific unsafe invariant.

---

## 5 — Config Tests

```rust
// src/config.rs (inline test)
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_valid_config_round_trips() {
        let json = r#"{"inner_gap":8,"outer_gap":16}"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        let cfg = load_config(f.path()).unwrap();
        assert_eq!(cfg.inner_gap, 8);
        assert_eq!(cfg.outer_gap, 16);
    }

    #[test]
    fn load_malformed_json_returns_config_error() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"not json").unwrap();
        let err = load_config(f.path()).unwrap_err();
        assert!(matches!(err, crate::StmError::Config(_)));
    }
}
```

Note: `tempfile` crate is a `[dev-dependencies]` only.

---

## 6 — Running the Suite

```powershell
# On Windows (full suite including Win32 tests)
cargo test

# With output for failures
cargo test -- --nocapture
```

Report: total / passed / failed / ignored. Full output for every failure. Note any pre-existing broken tests.

---

## 7 — What to Test / Skip

| ✅ Test | ❌ Skip |
|---|---|
| Layout arithmetic correctness | windows-rs crate internals |
| Config parse + error mapping | MSVC linker / toolchain behaviour |
| Error variant construction | Win32 API return values (mock instead) |
| Cross-module integration flows | Logging output format |
| Edge cases (0 windows, negative gaps) | Release profile optimisations |

---

## Handoff Checklist

- [ ] Inline unit tests: every new layout fn has ≥ 2 cases (nominal + edge)
- [ ] Integration tests in `tests/` for cross-module flows
- [ ] Config tests: valid round-trip + malformed error case
- [ ] Win32 tests guarded with `#[cfg(target_os = "windows")]`
- [ ] `cargo test` (pure) exits 0 on host
- [ ] `cargo clippy -- -D warnings` exits 0
- [ ] `cargo fmt --check` exits 0
- [ ] No `unwrap()` in test setup beyond fixture creation

---

## Gotchas

- **`#[cfg(target_os = "windows")]` must guard the entire test function, not just the body**: If the function signature references a `windows-rs` type (e.g., `HWND`), the compiler will reject it on Linux even with a body guard. Use the cfg attribute on the `#[test]` item itself.
- **`tempfile` must stay in `[dev-dependencies]`**: It is easy to accidentally add `tempfile` to `[dependencies]` when copy-pasting. This links it into the release binary — always check `Cargo.toml` after adding config tests.
- **Layout integer rounding leaves uncovered pixels**: `split_rect` with non-divisible widths silently drops remainder pixels. Tests MUST assert `tiles.iter().map(|r| r.width).sum::<i32>() == parent.width` (when gap=0) to catch this, not just check one tile's width.
- **`cargo test` on Linux passes but `cargo test` on Windows fails**: Pure layout and config tests must pass on any OS. If a test fails only on Windows, it usually means a Win32 type leaked into test setup without a `#[cfg]` guard.
- **Mock structs outside `#[cfg(test)]` inflate binary size**: `MockWindowMover` and similar test doubles MUST be inside `#[cfg(test)]` blocks or in `tests/` — never in `src/` without the cfg guard.
