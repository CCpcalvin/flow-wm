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

The daemon owns all subsystems behind `&mut self` (no `Arc<Mutex>`); a hook thread and the IPC thread coordinate via an mpsc channel.

See the [Developer Guide](docs/src/dev-guide/README.md) for the full architecture documentation. Build the mdBook locally with `mdbook build docs/`.

## License

All rights reserved.
