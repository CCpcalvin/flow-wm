# Evals — rust-test

## Eval 1 — Layout unit tests

**Trigger:** "Write tests for the `split_rect` function in `layout/engine.rs`"

**Expected output:**
- Inline `#[cfg(test)]` block with ≥ 3 cases: nominal 2-tile, zero count, gap subtraction
- Uses only `layout::types::{Rect, Axis}` — no Win32 imports
- All tests pass with `cargo test`

**Pass/fail checks:**
- [ ] Zero-count edge case present
- [ ] Gap arithmetic verified numerically (not just non-panic)
- [ ] No `windows` crate import in test block

---

## Eval 2 — Config round-trip test

**Trigger:** "Add tests for `load_config` in `config.rs`"

**Expected output:**
- Valid JSON round-trip test using `tempfile`
- Malformed JSON test asserting `StmError::Config(_)` variant
- `tempfile` in `[dev-dependencies]` only

**Pass/fail checks:**
- [ ] Both test cases present
- [ ] `unwrap_err()` used on the error case (not `unwrap()`)
- [ ] `tempfile` not in `[dependencies]`

---

## Eval 3 — Win32 mock test

**Trigger:** "Write a test for the layout manager's `apply_layout` function that calls `WindowMover::move_to`"

**Expected output:**
- `MockWindowMover` inside `#[cfg(test)]` block
- Test verifies that `move_to` is called once per window in the layout
- No actual Win32 call in test; no `#[cfg(target_os = "windows")]` guard needed for the mock test itself

**Pass/fail checks:**
- [ ] Mock struct inside `#[cfg(test)]`
- [ ] No `unsafe` in test body
- [ ] Assertion on `calls.borrow().len()` == expected window count
