# Evals — workflow-rust

## Eval 1 — Simple single-layer feature

**Trigger:** "Add a `MasterStack` layout to the layout engine"

**Expected output:**
- Single phase: CoderAgent (layout/) only
- TestEngineer runs inline/integration layout tests
- CodeReviewer checks module purity and test adequacy
- No win32/ phase needed

**Pass/fail checks:**
- [ ] Workflow does NOT spawn a win32/ CoderAgent (no Win32 calls needed for pure layout)
- [ ] Phase table has exactly 1 coding phase + test + review
- [ ] Checklist includes `cargo clippy` and `cargo test` verification

---

## Eval 2 — Cross-layer feature requiring new type

**Trigger:** "Add per-monitor gap config that the Win32 monitor enumeration feeds into layout"

**Expected output:**
- Phase 1a: config.rs — new `MonitorGapConfig` struct
- Phase 1b: layout/ — new layout fn consuming `MonitorGapConfig`
- Phase 2a: win32/monitor.rs — pass queried monitor info through config type
- Phase 3a: main.rs — wire together
- TestEngineer: config round-trip + layout correctness tests
- CodeReviewer: boundary check (layout/ must not import `windows` crate)

**Pass/fail checks:**
- [ ] `MonitorGapConfig` defined in phase 1 (before both layout/ and win32/ use it)
- [ ] Phase 2a depends on 1a (not parallel with it)
- [ ] Reviewer checklist includes "no windows import in layout/"

---

## Eval 3 — Re-spawn after unsafe violation

**Trigger:** CodeReviewer rejects because `unsafe { ... }` wraps a 30-line function body in `win32/hook.rs`

**Expected output:**
- Re-spawn policy: CoderAgent (win32/ only) to fix the unsafe scope
- After fix: CodeReviewer re-reviews win32/hook.rs only
- TestEngineer NOT re-spawned (no logic change, only unsafe scope shrink)

**Pass/fail checks:**
- [ ] Only win32/ CoderAgent spawned for the fix
- [ ] TestEngineer explicitly skipped per re-spawn policy
- [ ] CodeReviewer re-review is scoped to the changed file only
