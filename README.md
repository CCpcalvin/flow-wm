# ScrollingTilingManager (`stm`)

A scrolling, infinite-horizontal-canvas tiling window manager for Windows 10/11. Windows occupy columns on a virtual canvas that extends wider than any single monitor. The viewport slides left and right, bringing columns in and out of view.

## Binaries

| Binary | Role |
|--------|------|
| `stmd` | Daemon process — runs in the background, owns all state |
| `stm` | CLI client — sends commands to the daemon via IPC |
| `stm-watchdog` | Crash-recovery helper — restores windows if the daemon dies |

## Prerequisites

- **Rust** (stable, MSVC toolchain) — [rustup](https://rustup.rs/)
- **Visual Studio Build Tools** — "Desktop development with C++" workload

Install the MSVC target:

```powershell
rustup default stable-x86_64-pc-windows-msvc
```

## Building

```powershell
# Debug build
cargo build

# Release build (optimised, stripped)
cargo build --release
```

Binaries are output to `target/debug/` or `target/release/`.

## Testing

```powershell
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## Usage

```powershell
# Start the daemon (foreground)
stmd

# In another terminal, send commands
stm start
stm stop
stm query windows all
```

## Architecture

The system uses a 3-layer layout pipeline:

1. **Virtual Layout** — logical structure on an infinite horizontal canvas
2. **Projection** — converts virtual layout to actual screen coordinates
3. **Diff** — compares layouts to produce animated move instructions

See [docs/spec/00-overview.md](docs/spec/00-overview.md) for the full architecture documentation.

## License

All rights reserved.
