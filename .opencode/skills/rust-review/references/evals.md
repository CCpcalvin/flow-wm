# Evals — rust-review

## Eval 1 — Approve a correct patch

**Trigger:** Review a PR that adds `fn focus_window(hwnd: HWND) -> windows::core::Result<()>` in `registry/win32.rs` with minimal unsafe, proper error mapping, and a doc comment.

**Expected verdict:** ✅ Approved

**Pass/fail checks:**
- [ ] Reviewer identifies all 8 checklist sections as passed
- [ ] No false rejections for style outside the skill's scope
- [ ] Output includes file name in verdict line

---

## Eval 2 — Reject an unsafe violation

**Trigger:** Review code where `unsafe { ... }` wraps an entire `fn apply_layout()` body including a for-loop over HWNDs.

**Expected verdict:** ❌ Rejected

**Pass/fail checks:**
- [ ] Reviewer cites Section 2 ("unsafe scope wider than one call")
- [ ] Reviewer provides the specific file + line range
- [ ] Reviewer does not reject any other unrelated aspects

---

## Eval 3 — Reject layout-layer pollution

**Trigger:** Review `layout/engine.rs` that imports `windows::Win32::Foundation::HWND` to call `GetWindowRect` directly.

**Expected verdict:** ❌ Rejected

**Pass/fail checks:**
- [ ] Reviewer cites Section 1 (module boundary) and Section 2 (Win32 in layout/)
- [ ] Reviewer does not approve conditionally — this is a hard reject
- [ ] Reviewer suggests the fix: move the call to `registry/win32.rs`, pass `Rect` into layout

---

## Eval 4 — Reject cfg guard usage

**Trigger:** Review code that adds `#[cfg(target_os = "windows")]` guards around test functions.

**Expected verdict:** ❌ Rejected

**Pass/fail checks:**
- [ ] Reviewer cites Section 4 (no cfg guards needed — project is Windows-only)
- [ ] Reviewer explains that `build.rs` is the sole platform gate
- [ ] Reviewer does not reject the tests themselves — only the cfg guards
