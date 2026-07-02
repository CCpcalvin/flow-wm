# Win32 API Reference — FlowWM

## Window Enumeration

```rust
// Enumerate all top-level visible windows
EnumWindows(Some(enum_windows_proc), LPARAM(0))
```

- Filter: `IsWindowVisible(hwnd)` and `!IsIconic(hwnd)` and style has `WS_VISIBLE`.
- Skip windows with extended style `WS_EX_TOOLWINDOW` (toolbars, trays).
- Skip HWND whose title is empty (`GetWindowTextLengthW` == 0) unless your policy allows untitled windows.

## Work Area Query

```rust
// Get usable monitor area (excludes taskbar)
let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
GetMonitorInfoW(hmonitor, &mut info);
let work_rect = info.rcWork; // RECT: left, top, right, bottom
```

Convert `RECT` → `layout::types::Rect` at the `win32/` boundary — never pass `RECT` into `layout/`.

## Shell Hook for Window Events

```rust
// Register for shell hook messages
let hwnd_msg = CreateWindowExW(...); // message-only window
RegisterShellHookWindow(hwnd_msg);
let WM_SHELLHOOKMESSAGE: u32 = RegisterWindowMessageW(w!("SHELLHOOK"));
```

Shell hook codes relevant to tiling:
| Code | Meaning |
|---|---|
| HSHELL_WINDOWCREATED (1) | New window appeared |
| HSHELL_WINDOWDESTROYED (2) | Window removed |
| HSHELL_WINDOWACTIVATED (4) | Focus changed |
| HSHELL_RUDEAPPACTIVATED (0x8004) | Fullscreen app — pause tiling |

## DPI Awareness

- Call `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` at process startup before any window or monitor calls.
- All pixel values from Win32 are physical pixels once DPI-aware — pass them directly to `MoveWindow`.
- Do NOT scale by `GetDeviceCaps(hdc, LOGPIXELSX)` after enabling per-monitor DPI awareness.

## Known Gotchas

- **`MoveWindow` on maximised windows**: Call `ShowWindow(hwnd, SW_RESTORE)` before `MoveWindow`; otherwise the move is silently ignored.
- **UWP / Store apps**: Their HWNDs are hosted inside a `ApplicationFrameWindow`; targeting the frame HWND moves the chrome but not the content. Use `FindWindowExW` to locate the inner `Windows.UI.Core.CoreWindow` child if needed.
- **Elevated windows**: A non-elevated `flow.exe` cannot `MoveWindow` an elevated (admin) window. The call returns `FALSE` with `ERROR_ACCESS_DENIED` — log and skip; never crash.
- **Virtual desktops**: `IVirtualDesktopManager::IsWindowOnCurrentVirtualDesktop` (COM) is required to filter windows not on the active desktop. Without this check, tiling will include invisible windows.
