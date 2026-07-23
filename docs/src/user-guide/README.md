# User Guide

FlowWM is a scrolling tiling window manager for Windows. This guide covers the
day-to-day surface: how to install it, the full command reference, and how to
configure and extend it.

> **New here?** The [Installation](./installation.md) page walks through the
> 60-second setup (install AutoHotkey, `flow config init --ahk`,
> `flow start --ahk`, and the default hotkeys). Then come back here for the
> complete command and config reference.

## Quick start

```powershell
flow config init --ahk   # write flow.toml + flow.ahk (defaults)
flow start --ahk         # start the daemon and the hotkey script
flow query all           # see what the daemon sees (JSON)
flow stop                # shut it down
```

## Where to go next

- **[Installation](./installation.md)** — prerequisites, the two install paths
  (crates.io or from source), and first-run setup.
- **[Command Reference](./dispatch-reference.md)** — every `flow dispatch`
  action, annotated, with its default hotkey.
- **[Config Reference](./config-reference.md)** — every `flow.toml` and
  `flow-rules.toml` field, its default, and what it does.
- **[Contributing & Reporting Issues](./contributing.md)** — how to file a
  useful bug report, send a patch, or request a feature.
- **Hotkeys** — `flow.ahk` is yours to edit. The modifier is one `ModKey` line
  at the top; see the README's *Can I use the Win key?* section.

For architecture, the layout pipeline, and subsystem deep dives, see the
[Developer Guide](../dev-guide/index.md).
