# Evals — rust-implementation

## Eval 1 — New Win32 wrapper function

**Trigger:** "Implement a `focus_window(hwnd: HWND)` function in `registry/win32.rs`"

**Expected output:**
- File: `src/registry/win32.rs` — new `pub fn focus_window(hwnd: HWND) -> windows::core::Result<()>`
- Uses `SetForegroundWindow` from `windows::Win32::UI::WindowsAndMessaging`
- `unsafe` block wraps only the single Win32 call
- Returns `windows::core::Result<()>`, no `.unwrap()`
- Has `///` doc comment

**Pass/fail checks:**
- [ ] No `.unwrap()` or `.expect()` in production code
- [ ] `unsafe` scope is a single expression, not the whole function body
- [ ] `cargo clippy -- -D warnings` exits 0 after addition
- [ ] No `#[cfg(target_os)]` guards added

---

## Eval 2 — New layout algorithm

**Trigger:** "Add a `spiral_layout` function to `src/layout/mutations.rs` that tiles windows in a Fibonacci spiral"

**Expected output:**
- File: `src/layout/mutations.rs` — new `pub fn spiral_layout(parent: Rect, count: usize, gap_px: i32) -> Vec<Rect>`
- Pure function: no `use windows`, no I/O
- Uses integer arithmetic only (no `f32`/`f64`)
- Has inline `#[cfg(test)]` unit tests covering count=0, count=1, count=4
- Follows the 3-layer pipeline: virtual layout mutation → projection → diff

**Pass/fail checks:**
- [ ] No `windows` crate import in `layout/` files
- [ ] Unit tests present and pass with `cargo test`
- [ ] Function is `#[must_use]`
- [ ] No `#[cfg(target_os)]` guards

---

## Eval 3 — Config struct

**Trigger:** "Add a `GapConfig` struct to `src/config/types.rs` with `inner_gap: u32` and `outer_gap: u32`"

**Expected output:**
- Struct derives `Serialize, Deserialize, Debug, Clone, JsonSchema`
- Integration with existing config loading pipeline in `config/lifecycle.rs`
- Error mapped to `StmError::Config`, not propagated as a foreign error type
- Unit test: round-trip serialise → deserialise

**Pass/fail checks:**
- [ ] `serde` derives present, not hand-rolled impl
- [ ] Error mapped to `StmError::Config`, not a foreign error type
- [ ] `cargo fmt --check` exits 0
- [ ] `schemars::JsonSchema` derive present for JSON Schema support
