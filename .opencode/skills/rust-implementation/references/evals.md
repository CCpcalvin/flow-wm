# Evals — rust-implementation

## Eval 1 — New Win32 wrapper function

**Trigger:** "Implement a `focus_window(hwnd: HWND)` function in `win32/window.rs`"

**Expected output:**
- File: `src/win32/window.rs` — new `pub fn focus_window(hwnd: HWND) -> StmResult<()>`
- Uses `SetForegroundWindow` from `windows::Win32::UI::WindowsAndMessaging`
- `unsafe` block wraps only the single Win32 call
- Returns `StmResult<()>`, no `.unwrap()`
- Has `///` doc comment

**Pass/fail checks:**
- [ ] No `.unwrap()` or `.expect()` in production code
- [ ] `unsafe` scope is a single expression, not the whole function body
- [ ] `cargo clippy -- -D warnings` exits 0 after addition

---

## Eval 2 — New layout algorithm

**Trigger:** "Add a `spiral_layout` function to `src/layout/engine.rs` that tiles windows in a Fibonacci spiral"

**Expected output:**
- File: `src/layout/engine.rs` — new `pub fn spiral_layout(parent: Rect, count: usize, gap_px: i32) -> Vec<Rect>`
- Pure function: no `use windows`, no I/O
- Uses integer arithmetic only (no `f32`/`f64`)
- Has inline `#[cfg(test)]` unit tests covering count=0, count=1, count=4

**Pass/fail checks:**
- [ ] No `windows` crate import in `layout/engine.rs`
- [ ] Unit tests present and pass with `cargo test`
- [ ] Function is `#[must_use]`

---

## Eval 3 — Config struct

**Trigger:** "Add a `GapConfig` struct to `src/config.rs` with `inner_gap: u32` and `outer_gap: u32`, load from `stm.json`"

**Expected output:**
- Struct derives `Serialize, Deserialize, Debug, Clone`
- `load_config(path: &Path) -> StmResult<GapConfig>` in same file
- No `unwrap()` in loader; maps `serde_json::Error` to `StmError::Config`
- Unit test: round-trip serialise → deserialise

**Pass/fail checks:**
- [ ] `serde` derives present, not hand-rolled impl
- [ ] Error mapped to `StmError::Config`, not propagated as `serde_json::Error`
- [ ] `cargo fmt --check` exits 0
