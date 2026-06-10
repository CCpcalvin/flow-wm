# Evals — rust-test

## Eval 1 — Layout unit tests

**Trigger:** "Write tests for the layout engine's `add_window` and `remove_window` methods"

**Expected output:**
- Inline `#[cfg(test)]` block with ≥ 3 cases: nominal add, zero windows, remove last
- Uses only `layout` and `common` types — no Win32 imports
- All tests pass with `cargo test`

**Pass/fail checks:**
- [ ] Zero-window edge case present
- [ ] Tile coverage property verified (tiles cover full area)
- [ ] No `windows` crate import in test block
- [ ] No `#[cfg(target_os)]` guards

---

## Eval 2 — Config round-trip test

**Trigger:** "Add tests for config loading in `config/types.rs`"

**Expected output:**
- Valid TOML round-trip test using `tempfile`
- Malformed TOML test asserting `StmError::Config(_)` variant
- `tempfile` in `[dev-dependencies]` only

**Pass/fail checks:**
- [ ] Both test cases present
- [ ] `unwrap_err()` used on the error case (not `unwrap()`)
- [ ] `tempfile` not in `[dependencies]`

---

## Eval 3 — Coverage gap analysis

**Trigger:** "Analyze test coverage and write any missing tests for the layout module"

**Expected output:**
- List of all `pub fn` items in `layout/` modules
- Identification of functions without test coverage
- Newly written tests for every gap
- Full `cargo test` run showing all tests pass

**Pass/fail checks:**
- [ ] Every `pub fn` in `layout/` has ≥ 1 test case
- [ ] Edge cases (empty input, zero count, max values) covered
- [ ] No `#[cfg(target_os)]` guards in any test
- [ ] `cargo test` exits 0

---

## Eval 4 — Win32 mock test

**Trigger:** "Write a test for the animation system's `animate_move` function using a mock backend"

**Expected output:**
- `MockAnimationBackend` inside `#[cfg(test)]` block
- Test verifies that `animate_move` is called once per window in the layout
- No actual Win32 call in test

**Pass/fail checks:**
- [ ] Mock struct inside `#[cfg(test)]`
- [ ] No `unsafe` in test body
- [ ] Assertion on `calls.borrow().len()` == expected window count
