# User Guide

> **Status: Stub.** `stm` is under active development and the user-facing
> surface (commands, configuration, hotkeys) is still being finalised. This
> section will grow once the daemon reaches a stable feature set.

## Binaries

| Binary | Purpose |
|--------|---------|
| `stmd` | The daemon. Run this in the background (or via `stm start`). |
| `stm` | The CLI client. Use this to send commands to the daemon. |
| `stm-watchdog` | Spawned automatically by `stmd` for crash recovery. |

## Quick Start

```powershell
# Start the daemon (foreground, useful for debugging)
stmd

# In another terminal
stm start
stm stop
stm query windows all
```

For configuration, see [`stm.toml` and `stm-rules.toml`](../dev-guide/config-and-persistence.md).

> The full command reference, hotkey list, and troubleshooting guide will land
> here as the feature set stabilises. In the meantime, the
> [Developer Guide](../dev-guide/README.md) documents the architecture and
> every command the daemon currently understands.
