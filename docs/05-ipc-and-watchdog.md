# IPC Server & Watchdog (`stm-ipc`, `stm-watchdog`)

---

## stm-ipc

### Responsibility

`stm-ipc` defines the shared message types used by both `stmd` (server) and `stm` (client). It exposes:

- All `SocketMessage` variants (commands the client sends to the daemon)
- All `SocketResponse` variants (what the daemon sends back)
- JSON Schema generation for all message types (`schemars`)
- The UDS socket path convention

This crate has **no business logic**. It is a pure data definition crate depended on by both `stm-cli` and `stmd`.

### Transport

Communication uses **Unix Domain Sockets** at a fixed path:

```
\\.\pipe\stm   (Windows named pipe, UDS semantics)
```

Windows does support UDS since Windows 10 1809, but named pipes are more broadly compatible and simpler to use from external tools. The protocol is newline-delimited JSON — one JSON object per message/response.

### Message Schema

```rust
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SocketMessage {
    // Lifecycle
    Stop,
    ReloadConfig,
    CheckConfig,

    // Layout commands
    FocusLeft, FocusRight, FocusUp, FocusDown,
    SwapLeft, SwapRight, SwapUp, SwapDown,
    SwapWithOffscreen { direction: Direction },
    ScrollLeft, ScrollRight,
    ExpandColumn, ShrinkColumn,
    SetColumnWidth { eighths: u8 },
    MergeColumnLeft, MergeColumnRight,
    ToggleFloat,
    ToggleMonocle,
    PlaceAbove,
    Promote,
    CloseWindow,

    // Queries
    QueryWindowsAll,
    QueryLayoutVirtual,
    QueryLayoutActual,
    QueryState,

    // Config mutations
    SetConfigValue { key: String, value: serde_json::Value },

    // Persist
    ForgetApp { exe: String },
    ForgetAllApps,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SocketResponse {
    Ok,
    Data { payload: serde_json::Value },
    Error { message: String },
}
```

The JSON Schema for `SocketMessage` is exported via `stm socket-schema` and used by third-party tools and status bar integrations.

### Event Subscriptions

The daemon supports **event subscriptions** for tools that want a live feed (e.g. status bars). A client sends `{"type": "subscribe"}` and receives a stream of `SocketEvent` JSON objects as they occur:

```rust
pub enum SocketEvent {
    WindowAdded { hwnd: u64, exe: String, state: String },
    WindowRemoved { hwnd: u64 },
    FocusChanged { hwnd: u64 },
    LayoutChanged,
    ViewportScrolled { offset: i32 },
    StateChanged { key: String },
}
```

---

## stm-watchdog

### Responsibility

`stm-watchdog` is a small, intentionally minimal binary that `stmd` spawns as a child process at startup. If `stmd` exits unexpectedly (crash, OOM kill, etc.), the watchdog detects the parent process death and runs emergency recovery.

### Implementation

```
stmd spawns stm-watchdog --parent-pid <pid> --recovery-path <path>

stm-watchdog loops:
  every 2 seconds:
    if parent process is no longer running:
      call SetWindowPos on every HWND in recovery snapshot
        → restore each window to its pre_manage_rect
      exit
```

`stm-watchdog` reads the recovery snapshot directly. It does **not** communicate with `stmd` over IPC (the daemon may be dead). It requires no daemon, no IPC, no config — just the snapshot JSON and Win32 calls.

### Startup Integration

```bash
stmd start
  → writes recovery snapshot path to env
  → spawns stm-watchdog --parent-pid $$ --recovery-path %APPDATA%\stm\stm-recovery.json
  → watchdog runs silently in background
  → if stmd crashes, watchdog restores all windows to pre_manage_rect
```

The watchdog exits cleanly when `stmd` shuts down normally (graceful shutdown sends `SIGTERM`/`TerminateProcess` to the watchdog before exiting).

