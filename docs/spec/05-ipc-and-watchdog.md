# IPC & Watchdog (`src/ipc`, `src/bin/stm-watchdog.rs`)

---

## IPC Module

### Responsibility

`src/ipc` defines the shared message types and transport helpers used by both `stmd` and `stm`. It contains:

- all command message enums
- all response and event enums
- JSON Schema generation for message types
- transport helpers for the named pipe / socket connection

This module is part of the shared library crate in `src/lib.rs`, not a separate crate. The daemon and CLI both depend on it through the package's internal library.

### Transport

Communication uses a local IPC transport with newline-delimited JSON messages. The daemon exposes a single endpoint that the CLI connects to.

Preferred endpoint on Windows:

```text
\\.\pipe\stm
```

This keeps the transport local to the machine and easy to consume from external tools.

### Message Schema

```rust
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SocketMessage {
    Stop,
    ReloadConfig,
    CheckConfig,

    FocusLeft, FocusRight, FocusUp, FocusDown,
    SwapLeft, SwapRight, SwapUp, SwapDown,
    SwapColumn { direction: Direction },
    MoveWindow { direction: Direction },
    ScrollLeft, ScrollRight,
    ExpandColumn, ShrinkColumn,
    SetColumnWidth { width_px: u32 },
    ToggleFloat,
    ToggleMonocle,
    PlaceAbove,
    Promote,
    CloseWindow,

    QueryWindowsAll,
    QueryLayoutVirtual,
    QueryLayoutActual,
    QueryState,

    SetConfigValue { key: String, value: serde_json::Value },
    ForgetApp { exe: String },
    ForgetAllApps,
}
```

The module also defines `SocketResponse` and `SocketEvent` types. A JSON Schema is exported so external tools can inspect the protocol.

---

## Watchdog Binary

### Responsibility

`stm-watchdog` is a small recovery binary that `stmd` spawns as a child process during startup. If `stmd` exits unexpectedly, the watchdog restores windows from the recovery snapshot.

### Location

```text
src/bin/stm-watchdog.rs
```

The watchdog may use shared helper code from:

- `src/registry/` for snapshot parsing types
- `src/common/` for geometry and Win32 helpers

### Runtime Model

```text
stmd spawns stm-watchdog --parent-pid <pid> --recovery-path <path>

stm-watchdog loop:
  every 2 seconds:
    if parent process no longer exists:
      read stm-recovery.json
      call SetWindowPos on each HWND
      restore to pre_manage_rect
      exit
```

The watchdog does not talk to the daemon after startup. It is intentionally tiny and resilient.

### Why a Separate Binary

The watchdog must survive even if the daemon is corrupted, hung, or crashed. Making it a separate binary gives it:

- a separate process lifetime
- no dependency on the daemon main loop
- a minimal code path focused only on recovery

This is one of the few places where a separate binary is clearly justified.
