# Contributing & Reporting Issues

Contributions are welcome — bug reports, feature ideas, and pull requests all
help. This page is the user-facing short version. The canonical contributor
reference is the repo's [`CONTRIBUTING.md`](https://github.com/CCpcalvin/flow-wm/blob/master/CONTRIBUTING.md),
and the architecture is documented in the
[Developer Guide](../dev-guide/index.md).

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

- **Config files if relevant** (`flow.toml`, `flow-rules.toml`) — but redact any
  personal or sensitive content first.

### Raising the log level

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

### A note on config location

The config directory resolves in this priority order: `--config <dir>` flag →
`FLOW_CONFIG_DIR` env var → default `%USERPROFILE%\.config\flow\`. If you
override it, the `logs/` folder moves with it — mention your config dir in the
report.

> **Where to file:** open an issue at
> <https://github.com/CCpcalvin/flow-wm/issues>.

## Requesting features

Feature requests are welcome too. The roadmap lives at
[Roadmap](../dev-guide/roadmap.md) — check there first to see whether your
idea is already planned or explicitly out of scope. If it isn't, open an issue
and describe the workflow you're trying to enable, not just the implementation
you imagine. "Here's the friction I hit" is more useful than "add this flag".

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
- **Config changes must update two places.** Default values live in the
  `Default` impls in `src/config/types.rs` (the single source of truth). If you
  add or change a field, also update `default-config.toml` — the
  `default_config_toml_matches_compiled_defaults` test enforces that they stay
  in sync.
- **Adding dependencies:** use `cargo add <crate>` rather than hand-editing
  `Cargo.toml`, so versions and the lockfile stay consistent.

For the full conventions (including the 3-layer documentation model and
contributor gates), see
[`CONTRIBUTING.md`](https://github.com/CCpcalvin/flow-wm/blob/master/CONTRIBUTING.md)
in the repo root.

## Understanding the codebase

For architecture, threading, the layout engine, and subsystem write-ups, see
the [Developer Guide](../dev-guide/index.md). Build it locally:

```
mdbook build docs/
```

The [Building from Source](../dev-guide/building-from-source.md) page covers
prerequisites, MSVC setup, and the contributor gates in detail.
