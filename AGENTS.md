- Use `cargo add` to handle dependencies and versions. Do NOT edit `Cargo.toml` for it. 
- Remember to write good docstring extensively, so that `cargo doc` can generate good documentation for us. The documentation should also explain the logic, design decision etc. Treat them like a wiki page for the project.
- **Config defaults: CODE is the single source of truth.** Default values live in the `Default` impls of each config struct in `src/config/types.rs`. Each struct carries `#[serde(default)]` at the container level, so a user's `stm.toml` may be partial, empty, or nested-partial — serde fills the gaps from the `Default` impl. `default-config.toml` in the repo root is a hand-written EXAMPLE copied to users by `stm config init`; it is NOT read at runtime. When you add or change a config field, update BOTH the `Default` impl in `src/config/types.rs` AND `default-config.toml` — the `default_config_toml_matches_compiled_defaults` test enforces they stay in sync.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

When the user types `/graphify`, invoke the `skill` tool with `skill: "graphify"` before doing anything else.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- Dirty graphify-out/ files are expected after hooks or incremental updates; dirty graph files are not a reason to skip graphify. Only skip graphify if the task is about stale or incorrect graph output, or the user explicitly says not to use it.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
