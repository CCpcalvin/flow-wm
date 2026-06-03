---
name: workflow-rust
description: >
  Orchestrates multi-phase Rust binary feature implementation for the
  ScrollingTilingManager on Windows — layout engine, Win32 wrappers, config,
  and IPC. Load proactively on any new feature request for this project.
  Do NOT load for Python/FastAPI features, Svelte frontend work, or cross-platform
  Rust crates. Replaces the Python/Svelte workflow skill for this project.
  Produces a phased plan with CoderAgent, TestEngineer, and CodeReviewer assignments.
version: 1
---

# Rust Feature Workflow — ScrollingTilingManager (Windows Binary)

## 1 — TaskManager

Skip for trivial tasks (single-file, < 40 lines, no new types or module exports, no new Win32 calls).

Phase rules:
- Each phase must be independently compilable with `cargo check`.
- Same-batch phases touch non-overlapping file sets.
- Cross-batch only when phase B needs phase A output (e.g., new types used by Win32 wrappers).
- Each phase should use 100k–120k tokens. Adjust scope to fit.

Naming: `1a, 1b` (parallel) → `2a, 2b` (parallel, after batch 1) → `3a` (after batch 2)

Each CoderAgent is assigned to **one module layer** — `layout/`, `win32/`, `config.rs`, or `main.rs`. A single phase NEVER spans two layers that have a dependency relationship.

Example — *"Add spiral tiling + config hot-reload"*:

| Phase | Layer | Task | Deps |
|---|---|---|---|
| 1a | `layout/` | `spiral_layout` fn + inline unit tests | — |
| 1b | `config.rs` | `watch_config` file-watcher + reload signal | — |
| 2a | `win32/hook.rs` | Wire config reload signal to message loop | 1b |
| 2b | `win32/window.rs` | Apply spiral layout on `HSHELL_WINDOWCREATED` | 1a |
| 3a | `main.rs` | Wire hot-reload + spiral into startup path | 2a, 2b |

---

## 2 — Skill Assignment

Each agent loads the skill matching its role:

| Agent | Skill to load |
|---|---|
| CoderAgent | `rust-implementation` |
| TestEngineer | `rust-test` |
| CodeReviewer | `rust-review` |

There is no frontend layer for this project. Never spawn a Svelte/frontend agent.

---

## 3 — Execution Order

**Coding Phase** — run all CoderAgents first (parallel where deps allow):
- Each CoderAgent loads `rust-implementation`.
- Each CoderAgent handles: code + `cargo clippy -- -D warnings` clean + `cargo fmt --check` clean + related inline unit tests passing.
- No handoff until all CoderAgents in the batch report `cargo check` success.

**Testing Phase** — once per batch:
- TestEngineer loads `rust-test`. Writes integration/layout-correctness tests. Runs `cargo test`.
- Win32-dependent tests are `#[cfg(target_os = "windows")]` — TestEngineer notes which tests require Windows CI.

**Reviewing Phase** — once per batch, all reviewers run in parallel:
- CodeReviewer loads `rust-review`. Reviews code + test results.
- **All reviewers must approve** before the batch is done.

```
CoderAgents ──────────────────────────────────────────────────┐
  (layout/ + win32/ + config.rs, parallel where deps allow)   │
                                                               ▼
TestEngineer (cargo test suite) ──────────────────────────────┤
                                                               ▼
                    ┌─── CodeReviewer ────┐
                    │                     ├─── approved? → ✅ Done
                    └─────────────────────┘
                              ↑ rejected
                         CoderAgent ◄──────────────────────────┘
```

---

## 4 — Re-spawn Policy

| Case | Action |
|---|---|
| Fix ≤ ~5 lines, no test impact | Main Agent fixes directly |
| Fix isolated to one module, no test impact | CoderAgent only |
| Fix touches tested layout logic | CoderAgent → TestEngineer |
| Win32 unsafe scope violation | CoderAgent only → CodeReviewer re-reviews |
| Same issue × 3 | Escalate to user |

---

## 5 — Parallel Rules

- Never two agents editing the same `.rs` file simultaneously.
- No shared new types between parallel phases — promote to an earlier phase's `layout/types.rs` or `config.rs`.
- `layout/` and `win32/` can always be built in parallel (different module trees).
- The `Rect` type is the cross-layer contract — it MUST NOT change shape after it is used by both layers.
- Next batch's CoderAgent integrates parallel outputs by updating `main.rs` wiring only.

---

## 6 — Feature Done Checklist

**Rust Binary**
- [ ] All CoderAgents: `cargo clippy --target x86_64-pc-windows-msvc -- -D warnings` clean
- [ ] All CoderAgents: `cargo fmt --check` clean
- [ ] All CoderAgents: related inline unit tests passing
- [ ] TestEngineer: `cargo test` (pure) 0 failures on host
- [ ] TestEngineer: Win32-dependent tests tagged `#[cfg(target_os = "windows")]`
- [ ] CodeReviewer: approved
- [ ] `cargo build --target x86_64-pc-windows-msvc --release` produces `stm.exe` (on Windows)
- [ ] No `.unwrap()` / `.expect()` in production code paths
- [ ] All public items have `///` doc comments
- [ ] `build.rs` Windows-only guard present

**Cross-phase**
- [ ] `layout/types::Rect` shape unchanged from the established contract
- [ ] No new `windows` feature flags added without explicit justification
- [ ] All new modules registered in `src/main.rs` or parent `mod.rs`

---

## Gotchas

- **`layout/types::Rect` shape changes break both layers simultaneously**: If Rect gains a field (e.g., `dpi_scale: f64`) after both `layout/` and `win32/` use it, both modules break and require a coordinated multi-agent fix. Treat Rect as a frozen contract once two layers depend on it — promote any shape change to a dedicated phase 0.
- **`cargo check` passing does not mean `cargo test` passes on Windows**: CoderAgents may report "compiles" without running tests. Always require `cargo test` output in the testing phase report — not just a clean `cargo check`.
- **Parallel agents on `main.rs`**: The wiring phase always touches `main.rs`. Never assign two parallel agents to phases that both need to edit `main.rs` — one agent must own that file exclusively per batch.
- **Skipping TestEngineer when only `unsafe` scope changed**: A smaller `unsafe` block that wraps the same call differently can change semantics (e.g., what is protected by the scope). Always run `cargo test` after unsafe refactors, even if no logic changed. The re-spawn policy "CoderAgent only" refers to respawning the fixer — TestEngineer still runs the full suite.
- **Win32 feature flag drift across phases**: When two parallel CoderAgents each add a `windows` feature flag to `Cargo.toml`, one will overwrite the other's change. Require agents to produce a unified diff of `Cargo.toml` changes and merge manually in the next batch's wiring phase.
