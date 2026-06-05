<!-- Context: project-intelligence/technical | Priority: critical | Version: 2.0 | Updated: 2026-06-05 -->

# Technical Domain

**Purpose**: Tech stack, architecture, and coding patterns for ScrollingTilingManager (STM).
**Last Updated**: 2026-06-05

## Quick Reference
**Update Triggers**: New modules | Architecture changes | Dependency additions
**Audience**: Developers, AI agents writing Rust for this project

## Primary Stack

| Layer | Technology | Version | Rationale |
|-------|-----------|---------|-----------|
| Language | Rust | Edition 2024 | Performance-critical native Windows binary |
| Target | x86_64-pc-windows-msvc | — | Windows-only tiling window manager |
| Serialization | serde + serde_yaml + schemars | 1.x | YAML config with JSON Schema support |
| Error Handling | thiserror | 2.x | Derive error enums |
| Logging | log + env_logger | 0.4/0.11 | Structured logging |
| Build Profile | LTO + strip | release | Optimized, small binary |

## Binaries

| Binary | Role |
|--------|------|
| `stmd` | Daemon — owns all state, manages windows |
| `stm` | CLI client — sends commands via IPC |
| `stm-watchdog` | Crash-recovery — restores windows if daemon dies |

## Architecture: Mutation Pipeline

Every layout operation flows through the same 3-step functional pipeline:

```rust
fn apply_mutation(&mut self, new_layout: VirtualLayout) -> LayoutDiff {
    let new_actual = projection::project(&new_layout, &self.monitor, ...);
    let moves = diff::diff(&self.prev_actual, &new_actual);
    LayoutDiff { virtual_layout, actual_layout, moves }
}
```

The 3-layer model:
```
VirtualLayout (logical, no pixels)
       ↓ projection::project()
ActualLayout (pixel-accurate rects)
       ↓ diff::diff()
Vec<WindowMove> (animation instructions)
```

Key rules: mutations return new `VirtualLayout` (never mutate in place). `LayoutEngine` is pure math — zero Win32. `WindowId` bridges engine ↔ registry.

## Module Structure

```
src/
├── main.rs, lib.rs       # Daemon entry, library root
├── bin/                  # stm CLI, stm-watchdog
├── common/               # Cross-cutting types (Rect, WindowId, StmError)
├── config/               # YAML config loading & validation
├── layout/               # Pure layout logic (engine, mutations, projection, diff)
├── ipc/                  # IPC between CLI ↔ daemon (stub)
├── input/                # Keyboard/mouse interception (stub)
├── persist/              # State persistence (stub)
├── registry/             # HWND ↔ WindowId, Win32 bridge (stub)
└── animation/            # Animation timing & easing (stub)
```

Convention: each module has `mod.rs` + `types.rs`. `common/` is vocabulary only. Platform code isolated in `registry/`, `input/`.

## Naming Conventions

| Type | Convention | Example |
|------|-----------|---------|
| Files | `snake_case.rs` | `engine.rs`, `mutations.rs` |
| Structs / Enums | `PascalCase` | `LayoutEngine`, `AnimationHint` |
| Functions | `snake_case` | `add_window()`, `scroll_left()` |
| Config fields | `snake_case` (YAML) | `window_rules`, `duration_ms` |
| Serde reserved | `match_` field + `#[serde(rename)]` | `WindowRule.match_` → YAML `match` |

## Code Standards

- `#![warn(missing_docs)]` — every public item must have doc comments
- Module docs (`//!`) explain architecture; item docs (`///`) include examples
- `#[must_use]` on all pure functions and constructors
- Single error enum: `StmError` + `StmResult<T>` alias project-wide
- Serde defaults: `#[serde(default = "fn_name")]` for all config fields
- Config validation: `StmConfig::validate()` for semantic checks beyond serde
- Tests in same file via `#[cfg(test)] mod tests`, annotated `// Positive:` / `// Negative:`
- **After each session, AI must synchronize docstrings to reflect any code changes**

## Security & Safety

- Safe Rust by default — `unsafe` only at Win32 FFI boundary
- Any `unsafe` wrapped in safe abstraction with safety comments
- No `.unwrap()` in daemon code — use `StmResult<T>` / `Option<T>` propagation
- Graceful degradation — malformed config falls back to defaults, logs error
- Watchdog recovery — no orphaned hidden windows after crash

## 📂 Codebase References

| Module | Files | Purpose |
|--------|-------|---------|
| Common | `src/common/types.rs`, `src/common/error.rs` | Shared types, error enum |
| Config | `src/config/types.rs`, `src/config/schema.rs` | YAML config, validation |
| Layout | `src/layout/{engine,mutations,projection,diff,types}.rs` | Pure layout pipeline |
| Build | `Cargo.toml` | Dependencies, targets, release profile |

## Related Files

- `business-domain.md` — Why this project exists
- `decisions-log.md` — Architecture decisions with rationale
