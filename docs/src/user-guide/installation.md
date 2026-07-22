# Installation

FlowWM is a Windows-only tiling window manager. Getting it running takes about
five minutes: install the binaries, install a hotkey daemon, generate your
config, and start.

> Already installed and just want the command list? Jump to the
> [Command Reference](./dispatch-reference.md).

## Prerequisites

FlowWM is a native Windows application and links against Win32, so you need:

- **Windows 10 or 11.** (FlowWM does not run on Linux, macOS, or WSL — it uses
  Win32 APIs directly.)
- **The Rust MSVC toolchain** — install via [rustup](https://rustup.rs/) and
  select `stable-x86_64-pc-windows-msvc`. The MinGW / GNU toolchains **will
  not** link.
- **Visual Studio Build Tools** — the "Desktop development with C++" workload
  (supplies the MSVC linker and the Windows SDK). Even the `cargo install` path
  compiles locally, so this is required either way.
- **[AutoHotkey v2](https://www.autohotkey.com/)** (recommended) — FlowWM ships
  no keybinder of its own. AutoHotkey v2 is the companion the `--ahk` flag
  generates a script for. Anything that can run a command on a keypress will
  also work (whkd, PowerToys, etc.), but you'd write that integration yourself.

## Step 1 — Install the binaries

You have two paths. **`cargo install` is recommended** because it puts `flow`
and `flowd` on your `PATH` automatically.

### From crates.io (recommended)

```powershell
cargo install flow-wm
```

That's it. Both binaries are installed onto your `PATH`.

### From source

```powershell
git clone https://github.com/CCpcalvin/flow-wm.git
cd flow-wm
cargo build --release
```

The binaries land in `target\release\` — you'll need to add that folder to your
`PATH` (or copy the binaries somewhere already on it). For the full build
walkthrough, see
[Building from Source](../dev-guide/building-from-source.md) in the Developer
Guide.

Verify the install:

```powershell
flow --version
```

## Step 2 — Generate your config and hotkeys

```powershell
flow config init --ahk
```

This writes two files into your config directory:

| File | Purpose |
|------|---------|
| `%USERPROFILE%\.config\flow\flow.toml` | Application config — column counts, padding, animation, borders, etc. Sensible defaults; heavily commented. |
| `%USERPROFILE%\.config\flow\flow.ahk` | A ready-to-use AutoHotkey v2 script binding every command to a `ScrollLock + <key>` chord. |

> **Existing files are never overwritten.** Re-running `flow config init --ahk`
> is safe — it only writes files that don't yet exist. To regenerate, delete
> the file first.

If you'd rather not use AutoHotkey, run `flow config init` (without `--ahk`)
and wire up your own keybinder. The [Command Reference](./dispatch-reference.md)
lists every `flow dispatch <command>` call you'd need to bind.

The config directory resolves in this priority order: `--config <dir>` flag →
`FLOW_CONFIG_DIR` env var → default `%USERPROFILE%\.config\flow\`. See
[Config & Persistence](../dev-guide/config-and-persistence.md) in the Developer
Guide for the full resolution chain.

## Step 3 — Start the daemon

```powershell
flow start --ahk
```

This launches the `flowd` daemon in the background, and once it's listening,
starts your `flow.ahk` script alongside it. You should now be able to press
`ScrollLock + T` to tile the focused window.

Want this on every login? Run this once:

```powershell
flow enable-autostart --ahk
```

It registers both the daemon and the hotkey script to launch at logon. (Use
`flow disable-autostart` to undo.)

## Step 4 — Sanity check

```powershell
flow query all     # dump the daemon's view of every window/workspace (JSON)
flow dispatch focus right    # same as ScrollLock + l
flow stop          # shut it down
```

If `flow query all` returns JSON, the daemon is up and the named-pipe IPC is
working. From here, the [Command Reference](./dispatch-reference.md) is the
canonical list of what you can do, and the
[Config Reference](./config-reference.md) covers customising `flow.toml`.

## Troubleshooting

### `flow` isn't recognised

The binaries aren't on your `PATH`. If you installed via `cargo install`, check
that `%USERPROFILE%\.cargo\bin` is on your `PATH` (rustup usually adds it). If
you built from source, see
[Building from Source → Putting `flow` on your PATH](../dev-guide/building-from-source.md).

### The daemon doesn't respond to hotkeys

Two things have to be running: `flowd` (the daemon) *and* `flow.ahk` (the
hotkey script). `flow start --ahk` starts both; `flow start` alone starts only
the daemon. Check both processes are alive in Task Manager.

If you changed `ModKey` in `flow.ahk`, make sure the key you chose is one
AutoHotkey recognises — see the
[AutoHotkey key list](https://www.autohotkey.com/docs/v2/KeyList.htm).

### The daemon won't start

Capture an isolated log and inspect it:

```powershell
flow start --log-file <path>
```

The file is reset on each launch, so it contains only that run. To raise the
verbosity, set `RUST_LOG=trace` first. See
[Contributing & Reporting Issues](./contributing.md) for how to file a useful
bug report once you have a log.

## Where to go next

- [Command Reference](./dispatch-reference.md) — every command, annotated.
- [Config Reference](./config-reference.md) — every config field, its default,
  and what it does.
- [Contributing & Reporting Issues](./contributing.md) — bug reports, feature
  requests, pull requests.
