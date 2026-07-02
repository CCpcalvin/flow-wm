# User Guide

> **Status: Stub.** `flow` is under active development and the user-facing
> surface (commands, configuration, hotkeys) is still being finalised. This
> section will grow once the daemon reaches a stable feature set.

## Binaries

| Binary | Purpose |
|--------|---------|
| `flowd` | The daemon. Run this in the background (or via `flow start`). |
| `flow` | The CLI client. Use this to send commands to the daemon. |
| `flow-watchdog` | Spawned automatically by `flowd` for crash recovery. |

## Quick Start

```powershell
# Start the daemon (foreground, useful for debugging)
flowd

# In another terminal
flow start
flow stop
flow query windows all
```

For configuration, see [`flow.toml` and `flow-rules.toml`](../dev-guide/config-and-persistence.md).

> The full command reference, hotkey list, and troubleshooting guide will land
> here as the feature set stabilises. In the meantime, the
> [Developer Guide](../dev-guide/README.md) documents the architecture and
> every command the daemon currently understands.
