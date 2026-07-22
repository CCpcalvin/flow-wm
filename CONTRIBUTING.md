# Contributing to FlowWM

Contributions are welcome — bug reports, feature ideas, and pull requests all help.
This file is the short version: how to file a useful bug report, and what a pull
request needs to land. For understanding how the codebase fits together, read the
[Developer Guide](https://ccpcalvin.github.io/flow-wm/dev-guide/index.html) (also
available locally via `mdbook build docs/`).

## Reporting bugs

A good bug report is reproducible. Please include:

- **FlowWM version** — `flow --version`.
- **Windows version** (e.g. Windows 11 23H2).
- **A log file.** FlowWM writes a date-stamped log by default:

  `%USERPROFILE%\.config\flow\logs\flowd-YYYY-MM-DD.log`

  (one file per day, appended across restarts).

  The cleanest way to capture a bug is to reproduce it with an isolated log:

  ```
  flow start --log-file <path>
  ```

  This redirects **all** logging to `<path>` and starts the file empty on each
  launch, so the file contains only that run. Attach that file to the issue.

  To capture more detail, raise the log level by setting `RUST_LOG` before
  starting (default is `debug` in debug builds, `info` in release):

  ```bat
  :: Windows cmd
  set RUST_LOG=trace && flow start --log-file <path>
  ```

  ```powershell
  # PowerShell
  $env:RUST_LOG='trace'; flow start --log-file <path>
  ```

- **Config files if relevant** (`flow.toml`, `flow-rules.toml`) — but redact any
  personal or sensitive content first.

> **Note on config location:** the config directory resolves in this priority
> order: `--config <dir>` flag → `FLOW_CONFIG_DIR` env var → default
> `%USERPROFILE%\.config\flow\`. If you override it, the `logs/` folder moves
> with it — mention your config dir in the report.

## Pull requests

Before opening a PR, make sure the basics are green:

```
cargo fmt
cargo clippy --all-targets
cargo test
```

A few project-wide conventions:

- **Document public items.** `#![warn(missing_docs)]` is enabled in `src/lib.rs`,
  so every public item needs at least a one-line `///` doc comment.
- **Config changes must update two places.** Default values live in the `Default`
  impls in `src/config/types.rs` (the single source of truth). If you add or
  change a field, also update `default-config.toml` — the
  `default_config_toml_matches_compiled_defaults` test enforces that they stay in
  sync.
- **Documentation follows a 3-layer model:**
  - **mdBook** (`docs/src/dev-guide/*.md`, rendered at
    <https://ccpcalvin.github.io/flow-wm/dev-guide/index.html>) — the *why* and
    *how it fits*: architecture, design decisions, data flow, diagrams (Mermaid).
  - **`///` docstrings** — the *what*: a one-line summary plus the contract
    (`# Errors` / `# Panics` / `# Safety` where relevant). Link to mdBook for
    rationale, don't duplicate it.
  - **`//` inline comments** — only the local *why* a reader can't reconstruct
    from the code.
- **Build the docs** with `mdbook build docs/`, and the API reference with
  `cargo doc --no-deps --document-private-items`.
- **Adding dependencies:** use `cargo add <crate>` rather than hand-editing
  `Cargo.toml`, so versions and the lockfile stay consistent.

## Understanding the codebase

For architecture, threading, the layout engine, and subsystem write-ups, see the
[Developer Guide](https://ccpcalvin.github.io/flow-wm/dev-guide/index.html). Build
it locally:

```
mdbook build docs/
```
