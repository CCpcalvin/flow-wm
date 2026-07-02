## Rust workflow gate (mandatory)

The product is a Rust Windows binary under `src/`. For non-trivial Rust work the full
workflow (phased planning, clippy/fmt gates, TestEngineer, CodeReviewer) is NOT optional.
The trigger is a file pattern, not a "does this feel heavy" judgment — that heuristic is
what caused Test/Review to get skipped in practice.

A Rust task is **non-trivial** if ANY is true:
- edits `src/**/*.rs` adding or modifying types, module exports, or Win32 calls;
- touches more than one file under `src/`; or
- adds/changes a config field, layout algorithm, or IPC command.

When non-trivial, load `skill("rust-workflow")` and follow the workflow it defines. The
skill is the single source of truth for the phases, quality gates, and done-criteria.

**Trivial Rust edits** — single-file, <40 lines, no new types, module exports, or Win32
calls — may skip the workflow, but `cargo check` must still pass. **When in doubt, treat
as non-trivial.**

- Use `cargo add` to handle dependencies and versions. Do NOT edit `Cargo.toml` for it.

## Documentation strategy

This project uses a **3-layer documentation model**. Each layer has a distinct job — keep the boundaries clean so nothing is documented twice.

| Layer | Location | Job |
|-------|----------|-----|
| **mdBook** | `docs/` (`docs/src/dev-guide/*.md`) | **Why & How It Fits** — architecture, design decisions, cross-cutting invariants, data flow, lifecycle, algorithms, diagrams (Mermaid), onboarding, roadmap. Narrative prose. This is the primary reference for understanding the project. |
| **`///` docstrings** | source files | **What & Contract** — a one-line summary of the item, non-obvious constraints, and `# Errors` / `# Panics` / `# Safety` sections only when they apply. When deeper context lives in mdBook, add a prose cross-link with the path in parens (e.g. ``(`docs/src/dev-guide/window-registry.md`)``); do NOT use markdown links (they break in rendered rustdoc). Keep docstrings short — the rationale belongs in mdBook, not here. |
| **`//` inline comments** | source files | **Why Here** — only the tricky local decision, invariant, or magic number that a reader cannot reconstruct from the code itself. Do not restate what the code already says. |

Rules:
- Do NOT explain design decisions in `///` — that is mdBook's job. Link there instead.
- Every public item must still have at least one-line `///` (`#![warn(missing_docs)]` is set in `src/lib.rs`).
- ASCII diagrams belong in mdBook (rendered as Mermaid), never in docstrings.
- Build the book with `mdbook build docs/`; render rustdoc with `cargo doc --no-deps --document-private-items`.

- **Config defaults: CODE is the single source of truth.** Default values live in the `Default` impls of each config struct in `src/config/types.rs`. Each struct carries `#[serde(default)]` at the container level, so a user's `flow.toml` may be partial, empty, or nested-partial — serde fills the gaps from the `Default` impl. `default-config.toml` in the repo root is a hand-written EXAMPLE copied to users by `flow config init`; it is NOT read at runtime. When you add or change a config field, update BOTH the `Default` impl in `src/config/types.rs` AND `default-config.toml` — the `default_config_toml_matches_compiled_defaults` test enforces they stay in sync.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

When the user types `/graphify`, invoke the `skill` tool with `skill: "graphify"` before doing anything else.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- Dirty graphify-out/ files are expected after hooks or incremental updates; dirty graph files are not a reason to skip graphify. Only skip graphify if the task is about stale or incorrect graph output, or the user explicitly says not to use it.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
