---
name: rust-workflow
description: >
  Orchestration reference for multi-phase Rust feature work on the
  ScrollingTilingManager Windows binary (layout, registry/Win32, config, IPC,
  animation). Holds phase planning, parallel rules, module dependency layers,
  re-spawn policy, and the feature-done checklist.

  TRIGGER: loaded by the "Rust workflow gate" rule in AGENTS.md — do not decide
  independently whether to load this skill. If you have been routed here, you are
  running a non-trivial Rust task; read this file in full.

  Scope: Windows binary under src/ only. Not for Python/FastAPI, Svelte frontend,
  or cross-platform Rust crates. Produces CoderAgent / TestEngineer / CodeReviewer
  assignments.
version: 2
---

# Rust Feature Workflow — ScrollingTilingManager (Windows Binary)

> **When to load this skill** is decided by the "Rust workflow gate" in `AGENTS.md`,
> not by this file. If you are reading this, a non-trivial Rust task is in progress.
> This file covers the **how**: phase planning, parallel rules, and done-criteria.

## 1 — TaskManager

You may skip formal TaskManager phasing for simple non-trivial tasks — a single module, one phase, no cross-layer dependency. Everything below assumes phasing is warranted.

Phase rules:
- Each phase must be independently compilable with `cargo check`.
- Same-batch phases touch non-overlapping file sets.
- Cross-batch only when phase B needs phase A output (e.g., new types used by registry wrappers).
- Each phase should use 100k–120k tokens. Adjust scope to fit.

Naming: `1a, 1b` (parallel) → `2a, 2b` (parallel, after batch 1) → `3a` (after batch 2)

Each CoderAgent is assigned to **one module layer** — `layout/`, `registry/`, `config/`, `ipc/`, `animation/`, or `main.rs`. A single phase NEVER spans two layers that have a dependency relationship.

Module layers and their dependencies:

```
common/    → (no stm imports — foundation layer)
layout/    → common/ only
config/    → common/ only
registry/  → common/ + layout/types (for Rect conversion)
animation/ → layout/types + registry/ (for Win32 backends)
ipc/       → common/ + layout/ (for dispatch to engine)
daemon/    → all modules (orchestration)
main.rs    → all modules (wiring)
```

Example — *"Add spiral tiling + config hot-reload"*:

| Phase | Layer | Task | Deps |
|---|---|---|---|
| 1a | `layout/` | `spiral_layout` fn + inline unit tests | — |
| 1b | `config/` | `watch_config` file-watcher + reload signal | — |
| 2a | `registry/hooks.rs` | Wire config reload signal to event loop | 1b |
| 2b | `registry/win32.rs` | Apply spiral layout on `HSHELL_WINDOWCREATED` | 1a |
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
- TestEngineer loads `rust-test`.
- TestEngineer analyzes coverage gaps, writes missing tests, runs `cargo test`.
- TestEngineer reports: coverage gaps found, tests written, suite results.

**Reviewing Phase** — once per batch, all reviewers run in parallel:
- CodeReviewer loads `rust-review`. Reviews code + test results.
- **All reviewers must approve** before the batch is done.

```
CoderAgents ──────────────────────────────────────────────────┐
  (layout/ + registry/ + config/, parallel where deps allow)  │
                                                                ▼
TestEngineer (coverage analysis + cargo test) ─────────────────┤
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
- No shared new types between parallel phases — promote to an earlier phase's `common/types.rs` or `layout/types.rs`.
- `layout/` and `registry/` can always be built in parallel (different module trees, layout/ is pure).
- The `WindowId` type in `common/types.rs` is the cross-layer contract — it MUST NOT change shape after it is used by both layers.
- Next batch's CoderAgent integrates parallel outputs by updating `main.rs` wiring only.

---

## 6 — Feature Done Checklist

**Rust Binary**
- [ ] All CoderAgents: `cargo clippy -- -D warnings` clean
- [ ] All CoderAgents: `cargo fmt --check` clean
- [ ] All CoderAgents: related inline unit tests passing
- [ ] TestEngineer: coverage gap analysis completed
- [ ] TestEngineer: `cargo test` (full suite) 0 failures
- [ ] TestEngineer: missing tests written for identified gaps
- [ ] CodeReviewer: approved
- [ ] `cargo build --release` produces `stmd.exe`, `stm.exe`, `stm-watchdog.exe`
- [ ] No `.unwrap()` / `.expect()` in production code paths
- [ ] All public items have `///` doc comments
- [ ] `build.rs` Windows-only guard present
- [ ] No `#[cfg(target_os)]` guards in source or test code

**Cross-phase**
- [ ] `common/types::WindowId` shape unchanged from the established contract
- [ ] No new `windows` feature flags added without explicit justification
- [ ] All new modules registered in `src/lib.rs` or parent `mod.rs`

---

## Gotchas

- **`WindowId` shape changes break both layers simultaneously**: If `WindowId` gains a field after both `layout/` and `registry/` use it, both modules break and require a coordinated multi-agent fix. Treat `WindowId` as a frozen contract once two layers depend on it — promote any shape change to a dedicated phase 0.
- **`cargo check` passing does not mean `cargo test` passes**: CoderAgents may report "compiles" without running tests. Always require `cargo test` output in the testing phase report — not just a clean `cargo check`.
- **Parallel agents on `main.rs`**: The wiring phase always touches `main.rs`. Never assign two parallel agents to phases that both need to edit `main.rs` — one agent must own that file exclusively per batch.
- **Skipping TestEngineer when only `unsafe` scope changed**: A smaller `unsafe` block that wraps the same call differently can change semantics. Always run `cargo test` after unsafe refactors, even if no logic changed. The re-spawn policy "CoderAgent only" refers to respawning the fixer — TestEngineer still runs the full suite.
- **Win32 feature flag drift across phases**: When two parallel CoderAgents each add a `windows` feature flag to `Cargo.toml`, one will overwrite the other's change. Require agents to produce a unified diff of `Cargo.toml` changes and merge manually in the next batch's wiring phase.
