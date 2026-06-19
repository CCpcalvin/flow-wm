# WindowRegistry (`stm-registry`)

## Responsibility

`WindowRegistry` is the authoritative source of truth for every window the daemon is aware of. It:

- Hooks into the Windows OS event system to detect window creation, destruction, focus changes, minimize, restore, maximize, and fullscreen transitions
- Classifies each window as `Tiling`, `Floating`, or `Ignored` based on config rules
- Maintains per-window state in a `HashMap<HWND, Window>`
- Emits typed `WindowEvent`s consumed by `ScrollingSpace` (via the daemon) and `InputInterceptor`
- Writes and reads the **recovery snapshot** (`stm-recovery.json`)

---

## OS Hooks Used

All hooks are registered via `SetWinEventHook` (accessible in Rust via `windows-rs`):

| Event constant | Meaning | Action |
|---|---|---|
| `EVENT_OBJECT_CREATE` | New window appeared | Classify and register |
| `EVENT_OBJECT_DESTROY` | Window closed | Remove from registry, release virtual slot |
| `EVENT_SYSTEM_FOREGROUND` | Focus changed | Update focused HWND |
| `EVENT_SYSTEM_MOVESIZESTART` | User started dragging/resizing | Notify InputInterceptor |
| `EVENT_SYSTEM_MOVESIZEEND` | User finished dragging/resizing | Trigger snap computation |
| `EVENT_SYSTEM_MINIMIZESTART` | Window minimized | Transition to `Minimized`, release slot |
| `EVENT_SYSTEM_MINIMIZEEND` | Window restored from taskbar | Trigger restore placement logic |
| `EVENT_OBJECT_LOCATIONCHANGE` | Window moved/resized | Used for fullscreen detection (debounced) |

An additional `LowLevelMouseProc` hook is registered by `InputInterceptor` for `Super+LMB/RMB` drag gestures. This is separate from the `WinEventHook` path.

---

## Window Classification

On `EVENT_OBJECT_CREATE`, the registry evaluates the new window against the ordered rule list from config:

```yaml
window_rules:
  - match:
      exe: "explorer.exe"
      title_contains: "Open"
    action: ignore

  - match:
      exe: "steam.exe"
    action: float

  - match:
      class: "Chrome_WidgetWin_1"
      exe: "chrome.exe"
    action: tile
```

Rules are evaluated **top to bottom, first match wins**. If no rule matches, the default is `tile` (configurable via `default_window_action`).

Match fields available:

| Field | Type | Notes |
|---|---|---|
| `exe` | string (exact or glob) | e.g. `"code.exe"`, `"steam*"` |
| `title` | string (exact) | Window title bar text |
| `title_contains` | string (substring) | Case-insensitive |
| `title_regex` | string (regex) | Full regex match on title |
| `class` | string (exact) | Win32 window class name |
| `process_path` | string (glob) | Full executable path |

**Per-app learned state** (from `stm-persist`) can override the config rule. See `04-persistence.md`.

---

## Window Struct

```rust
pub struct Window {
    pub hwnd: HWND,
    pub exe: String,
    pub title: String,
    pub class: String,
    pub process_path: PathBuf,

    pub state: WindowState,
    pub pre_manage_rect: Rect,       // position before stm ever touched this window
    pub last_natural_size: Size,     // preferred unmanaged size, updated on explicit user resize
    pub last_virtual_slot: Option<VirtualSlot>, // remembered for minimize/restore
}

pub enum WindowState {
    Tiling(TilingState),
    Floating(FloatingState),
    Ignored(IgnoredReason),
}

pub enum TilingState {
    Active { col: usize, row: usize },
    Minimized,
}

pub enum FloatingState {
    Active { rect: Rect },
    Minimized,
}

pub enum IgnoredReason {
    Maximized,
    Fullscreen,
    ExplicitRule,   // matched an `ignore` rule
}
```

---

## Fullscreen Detection

`IsZoomed(hwnd)` reliably detects `WS_MAXIMIZE`. Exclusive or borderless fullscreen requires checking:

1. `GetWindowRect(hwnd)` equals the monitor's full rect
2. Window style does **not** include `WS_CAPTION | WS_THICKFRAME` (no titlebar, no resize border)

This check runs in the `EVENT_OBJECT_LOCATIONCHANGE` handler, debounced to 200ms to avoid false positives during window animation.

On transition into `Ignored::Fullscreen`, all other tiling windows **keep their positions** — the layout is not collapsed.

---

## Recovery Snapshot

The registry writes `%APPDATA%\stm\stm-recovery.json` atomically (write to `.tmp`, then `rename()`) on every state mutation.

```json
{
  "schema_version": 1,
  "viewport_offset": 1200,
  "windows": [
    {
      "hwnd": 12345678,
      "exe": "code.exe",
      "title": "main.rs — stm",
      "state": "Tiling::Active",
      "virtual_col": 1,
      "virtual_row": 0,
      "parked_x": -8800,
      "parked_y": 0,
      "pre_manage_rect": { "x": 100, "y": 100, "w": 1200, "h": 900 },
      "last_natural_size": { "w": 1200, "h": 900 }
    }
  ]
}
```

On daemon startup, if a recovery snapshot exists, the registry rehydrates from it before scanning live windows. On `stm restore` (daemon not running), `stm-cli` reads this file directly and calls `SetWindowPos` on every HWND to bring them back to `pre_manage_rect`.

