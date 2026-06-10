# Evals — rust-workflow

## Eval 1 — Simple single-layer feature

**Trigger:** "Add a `MasterStack` layout to the layout engine"

**Expected output:**
- Single phase: CoderAgent (layout/) only
- TestEngineer runs inline/integration layout tests
- CodeReviewer checks module purity and test adequacy
- No registry/ phase needed

**Pass/fail checks:**
- [ ] Workflow does NOT spawn a registry/ CoderAgent (no Win32 calls needed for pure layout)
- [ ] Phase table has exactly 1 coding phase + test + review
- [ ] Checklist includes `cargo clippy` and `cargo test` verification

---

## Eval 2 — Cross-layer feature requiring new type

**Trigger:** "Add per-monitor gap config that the Win32 monitor enumeration feeds into layout"

**Expected output:**
- Phase 1a: config/types.rs — new `MonitorGapConfig` struct
- Phase 1b: layout/ — new layout fn consuming `MonitorGapConfig`
- Phase 2a: registry/win32.rs — pass queried monitor info through config type
- Phase 3a: main.rs — wire together
- TestEngineer: config round-trip + layout correctness tests
- CodeReviewer: boundary check (layout/ must not import `windows` crate)

**Pass/fail checks:**
- [ ] `MonitorGapConfig` defined in phase 1 (before both layout/ and registry/ use it)
- [ ] Phase 2a depends on 1a (not parallel with it)
- [ ] Reviewer checklist includes "no windows import in layout/"

---

## Eval 3 — Re-spawn after unsafe violation

**Trigger:** CodeReviewer rejects because `unsafe { ... }` wraps a 30-line function body in `registry/win32.rs`

**Expected output:**
- Re-spawn policy: CoderAgent (registry/ only) to fix the unsafe scope
- After fix: CodeReviewer re-reviews registry/win32.rs only
- TestEngineer NOT re-spawned (no logic change, only unsafe scope shrink)

**Pass/fail checks:**
- [ ] Only registry/ CoderAgent spawned for the fix
- [ ] TestEngineer explicitly skipped per re-spawn policy
- [ ] CodeReviewer re-review is scoped to the changed file only

---

## Eval 4 — Test coverage gap analysis

**Trigger:** "Analyze test coverage for the projection module and write missing tests"

**Expected output:**
- TestEngineer analyzes `layout/projection.rs` for coverage gaps
- Identifies untested functions and edge cases
- Writes missing tests
- Full `cargo test` suite passes

**Pass/fail checks:**
- [ ] Every `pub fn` in projection.rs has ≥ 1 test case
- [ ] Edge cases (zero windows, single window, max columns) covered
- [ ] No `#[cfg(target_os)]` guards in any test
- [ ] `cargo test` exits 0
