# FlowWM

`flow` (FlowWM) is a tiling window manager for Windows 10/11
built around a **scrolling, infinite-horizontal-canvas** layout model. Unlike
grid-based managers such as glazeWM or komorebi, windows occupy columns on a
virtual canvas that can extend wider than any single monitor. The viewport
slides left and right, bringing columns in and out of view.

The project ships as two binaries inside a single Cargo package:

| Binary | Role |
|--------|------|
| `flowd` | The daemon. Runs in the background (via `flow start`), owns all state, and accepts IPC commands. |
| `flow` | The CLI client. Sends commands to the daemon over a named pipe. |

## How to Use This Book

- **[User Guide](./user-guide/index.md)** — for people who want to *run* `flow`:
  installation, the command reference, the configuration reference, and how to
  get help or report an issue.
- **[Developer Guide](./dev-guide/index.md)** — for contributors and anyone who
  wants to understand how the system works inside. Covers architecture, the
  layout pipeline, threading model, subsystem deep dives, design decisions, and
  the roadmap.

## Quick Links

- [Installation](./user-guide/installation.md) — get FlowWM running in a few minutes.
- [Command Reference](./user-guide/dispatch-reference.md) — every `flow` command, annotated, with its default hotkey.
- [Config Reference](./user-guide/config-reference.md) — every `flow.toml` and `flow-rules.toml` field, its default, and what it does.
- [Contributing & Reporting Issues](./user-guide/contributing.md) — how to file a useful bug report, send a patch, or request a feature.
- [Report an Issue](https://github.com/CCpcalvin/flow-wm/issues) — open an issue directly on GitHub.

## Building From Source

For the full build walkthrough — prerequisites, MSVC setup, cloning, and the
contributor gates (`cargo test` / `clippy` / `fmt`, `mdbook build`) — see the dedicated
[Building from Source](./dev-guide/building-from-source.md) page in the
Developer Guide.
