- Use `cargo add` to handle dependencies and versions. Do NOT edit `Cargo.toml` for it. 
- Remember to write good docstring extensively, so that `cargo doc` can generate good documentation for us. The documentation should also explain the logic, design decision etc. Treat them like a wiki page for the project.
- **Config defaults: TOML is the single source of truth.** Do NOT set default values via `#[serde(default)]` on config fields. Instead, set defaults in `default-config.toml` (shipped next to `stmd.exe`). This ensures that adding a new config field requires updating the TOML file — if you forget, deserialization fails with `"missing field 'xyz'"`. The `Default` impl in Rust is an emergency fallback only (for dev environments without the shipped file). Exception: `Vec` fields and per-entry boolean flags may keep `#[serde(default)]` for convenience.



