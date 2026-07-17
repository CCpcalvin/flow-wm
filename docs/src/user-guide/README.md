# User Guide

FlowWM is a scrolling tiling window manager for Windows. This guide covers the
day-to-day surface: the binaries, how to start, and the full command reference.

> **New here?** Start with the README's
> [*Getting started*](https://github.com/CCpcalvin/flow-wm#getting-started) — a
> 60-second guided tour (install AutoHotkey, `flow config init --ahk`,
> `flow start --ahk`, and the default hotkeys). Then come back here for the
> complete command list.

## Binaries

| Binary | Purpose |
|--------|---------|
| `flowd` | The daemon. Run in the background via `flow start` (or foreground for debugging). |
| `flow` | The CLI client. Sends commands to the daemon over a named pipe. |

## Quick start

```powershell
flow config init --ahk   # write flow.toml + flow.ahk (defaults)
flow start --ahk         # start the daemon and the hotkey script
flow query all           # see what the daemon sees (JSON)
flow stop                # shut it down
```

## Where to go next

- **[Command Reference](./dispatch-reference.md)** — every `flow dispatch` action,
  annotated, with its default hotkey.
- **Hotkeys** — `flow.ahk` is yours to edit. The modifier is one `ModKey` line at
  the top; see the README's *Why ScrollLock?* section.
- **Configuration** — [`flow.toml` and `flow-rules.toml`](../dev-guide/config-and-persistence.md).

For architecture, the layout pipeline, and subsystem deep dives, see the
[Developer Guide](../dev-guide/README.md).
