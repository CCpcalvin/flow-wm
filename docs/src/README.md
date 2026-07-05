# FlowWM

`flow` (FlowWM) is a tiling window manager for Windows 10/11
built around a **scrolling, infinite-horizontal-canvas** layout model. Unlike
grid-based managers such as glazeWM or komorebi, windows occupy columns on a
virtual canvas that can extend wider than any single monitor. The viewport
slides left and right, bringing columns in and out of view.

The project ships as three binaries inside a single Cargo package:

| Binary | Role |
|--------|------|
| `flowd` | The daemon process. Runs in the background and owns all state. |
| `flow` | The CLI client. Sends commands to the daemon, queries state, edits config. |
| `flow-watchdog` | The crash-recovery helper. Restores windows if the daemon dies. |

## How to Use This Book

- **[User Guide](./user-guide/README.md)** — for people who want to *run* `flow`.
  Currently a stub: features are still being finalised.
- **[Developer Guide](./dev-guide/README.md)** — for contributors and anyone who
  wants to understand how the system works inside. Covers architecture, the
  layout pipeline, threading model, subsystem deep dives, design decisions, and
  the roadmap.

## Quick Links

- [Architecture overview](./dev-guide/architecture.md)
- [Layout: virtual canvas & camera model](./dev-guide/layout/overview.md)
- [Window classification algorithm](./dev-guide/window-registry.md)
- [Threading model (no `Arc<Mutex>`)](./dev-guide/threading-model.md)
- [Roadmap](./dev-guide/roadmap.md)

## Building From Source

For the full build walkthrough — prerequisites, MSVC setup, cloning, and the
contributor gates (`cargo test` / `clippy` / `fmt`, `mdbook build`,
`cargo doc`) — see the dedicated
[Building from Source](./dev-guide/building-from-source.md) page in the
Developer Guide.
