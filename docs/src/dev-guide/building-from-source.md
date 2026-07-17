# Building from Source

FlowWM is a native Windows binary built with the MSVC toolchain. This page is
for anyone who wants to run FlowWM before the `winget` / `cargo install`
packages exist, or who wants to contribute.

> If you only want to *use* FlowWM, the high-level overview lives in the
> project [`README.md`](../../../README.md). This page is the build detail.

## Prerequisites

- **Rust** (stable, MSVC toolchain) — install via [rustup](https://rustup.rs/).
- **Visual Studio Build Tools** — the "Desktop development with C++" workload.
  FlowWM links against Win32, so the MSVC linker and the Windows SDK are
  required. The MinGW / GNU toolchains will **not** work.

Set the MSVC target as your default so every `cargo` invocation uses it:

```powershell
rustup default stable-x86_64-pc-windows-msvc
```

## Clone and Build

```powershell
git clone https://github.com/CCpcalvin/flow-wm.git
cd flow-wm
cargo build               # debug build
cargo build --release     # optimised, stripped binaries
```

The two binaries — `flowd.exe` (daemon) and `flow.exe` (CLI client) — are
written to `target\debug\` or `target\release\`.

### Putting `flow` on your PATH

`flow config init` and the other commands assume `flow.exe` is reachable from
your shell. Either copy the built binaries into a directory that is already on
your `PATH`, or add the release folder to `PATH`:

```powershell
# Add the release folder to PATH for the current PowerShell session:
$env:PATH += ";$PWD\target\release"

# (Or, for a permanent user-scoped PATH update, use:
#  [Environment]::SetEnvironmentVariable("PATH", $env:PATH, "User") )
```

## First Run

```powershell
flow config init          # writes default config to %USERPROFILE%\.config\flow\
flow config init --ahk    # also writes flow.ahk — a ready-to-use AutoHotkey v2 script
flow start                # launches the daemon in the background
```

The default config directory follows the XDG-style convention
`%USERPROFILE%\.config\flow\` (so it reads as `~/.config/flow/` on a Linux
mental model). You can override it with `--config <dir>` on `flow start` or
the `FLOW_CONFIG_DIR` environment variable. See
[Config & Persistence](./config-and-persistence.md) for the full resolution
chain.

Once the daemon is running, start your hotkey daemon (`flow start --ahk`, or
`flow enable-autostart --ahk` to auto-launch both at login) and you're tiling.
See the [`README.md`](../../../README.md) for usage.

## Contributor Gates

Before sending a patch, make sure the usual gates pass:

```powershell
cargo test                       # unit + integration tests
cargo clippy -- -D warnings      # lint (warnings are errors)
cargo fmt --check                # formatting check
```

Documentation builds:

```powershell
mdbook build docs                                  # the mdBook you are reading
cargo doc --no-deps --document-private-items       # API docs (rustdoc)
```

When adding or changing a config field, update **both** the `Default` impl in
[`src/config/types.rs`](../../../src/config/types.rs) **and** the example file
[`default-config.toml`](../../../default-config.toml) in the repo root — a test
(`default_config_toml_matches_compiled_defaults`) enforces they stay in sync.
See [Config & Persistence](./config-and-persistence.md) and
[Design Decisions](./design-decisions.md) for why code is the single source of
truth for defaults.
