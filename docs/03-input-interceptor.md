# InputInterceptor (`stm-input`)

## Responsibility

`InputInterceptor` owns all low-level input handling that cannot be expressed as discrete CLI commands. It:

- Registers a `LowLevelKeyboardProc` hook for hotkey detection and Super-key interception
- Registers a `LowLevelMouseProc` hook for `Super+LMB` drag and `Super+RMB` resize gestures
- Manages `DragSession` and `ResizeSession` state (the `InteractionController`)
- Emits typed `InputEvent`s consumed by the daemon's main event loop

The crate is **not** split further. The Super+drag/resize functionality is tightly coupled to both the keyboard hook (detecting Super held down) and the mouse hook (tracking cursor position). Separating them would require complex cross-crate shared state. Keeping them together makes the interaction state machine coherent.

---

## The Super Key Concept

`stm` does not literally use the Windows key as its modifier. Instead, the user **defines their own Super key** — typically `CapsLock`, a spare `F-key`, or a side mouse button. This avoids conflicts with Windows' own `Win+*` shortcuts.

### How it works

1. The user remaps their chosen physical key to a **synthetic keycode** at the keyboard firmware or OS level (e.g. via AutoHotkey, kanata, or a programmable keyboard). This is outside `stm`'s scope.
2. The user tells `stm` which keycode to treat as Super in config:

```yaml
super_key: VK_F24       # or any virtual key code
```

3. `InputInterceptor`'s keyboard hook intercepts every keydown/keyup event. When the Super key is held:
   - If the full chord (Super + key) matches a configured hotkey → consume the event, emit `InputEvent::Hotkey`, suppress the keystroke from reaching the OS
   - If no hotkey matches → pass the event through as-is (do not swallow unbound keys)

This means `stm` never silently swallows input. Unbound Super+key combinations reach the system as their original keycodes.

---

## Hotkey Configuration

Hotkeys are defined in config:

```yaml
hotkeys:
  focus_left:         "Super+H"
  focus_right:        "Super+L"
  focus_up:           "Super+K"
  focus_down:         "Super+J"
  swap_left:          "Super+Shift+H"
  swap_right:         "Super+Shift+L"
  scroll_left:        "Super+Left"
  scroll_right:       "Super+Right"
  toggle_float:       "Super+Space"
  toggle_monocle:     "Super+M"
  close_window:       "Super+Q"
  reload_config:      "Super+Shift+R"
  # ... more
```

All hotkey values are strings parsed into `(modifiers, vk_code)` at config load time. JSON Schema provides autocomplete for the key names (see `stm-config`).

---

## Super+LMB Drag — DragSession

When `Super` is held and the user presses `LMB` over a **tiling window**, `InputInterceptor` starts a `DragSession`.

```rust
pub struct DragSession {
    pub hwnd: HWND,
    pub initial_virtual_slot: (usize, usize),  // (col, row)
    pub cursor_offset: Point,                  // cursor pos relative to window origin
    pub last_cursor: Point,
}
```

During the drag:
- `InputInterceptor` suppresses the native `WM_NCLBUTTONDOWN` so Windows does not start its own titlebar drag
- The window moves in real-time via `SetWindowPos` with `SWP_NOACTIVATE | SWP_NOZORDER`, following the cursor with `cursor_offset` applied
- Every ~16ms (60fps), cursor position is sampled and the window's position updated — this is the only place `SetWindowPos` is called outside `window-animation`

On `LMB` release:
1. `DragSession` ends
2. `InputInterceptor` emits `InputEvent::DragEnd { hwnd, final_rect }`
3. `LayoutEngine.move_snap(hwnd, final_rect)` is called
4. The snap animation takes over — `window-animation` moves the window to its computed slot

**Note**: `Super+LMB` over a **floating window** is also supported. Floating windows are moved freely; on release they stay at their dropped position (no snap). `LayoutEngine` is not involved.

---

## Super+RMB Resize — ResizeSession

When `Super` is held and the user presses `RMB` over any managed window, `InputInterceptor` starts a `ResizeSession`.

```rust
pub struct ResizeSession {
    pub hwnd: HWND,
    pub initial_rect: Rect,
    pub anchor_corner: Corner,  // closest corner to cursor at session start
    pub last_cursor: Point,
}
```

The resize direction is determined by the cursor's position relative to the window center at session start — the closest corner becomes the drag anchor. This is the standard Linux WM convention (e.g. i3, Sway).

During resize, the window size follows the cursor in real-time. On `RMB` release:
1. `ResizeSession` ends
2. `InputInterceptor` emits `InputEvent::ResizeEnd { hwnd, final_rect }`
3. `LayoutEngine.resize_snap(hwnd, final_rect)` is called
4. Affected windows animate to snapped positions

---

## Snap Preview (Optional / Later)

During a `DragSession`, `InputInterceptor` can emit `InputEvent::DragPreview { target_slot }` every ~100ms (throttled), which the daemon uses to render a transparent overlay window showing where the window would snap. This is a later-stage feature; the core drag/snap logic does not depend on it.

---

## Native Titlebar Drag (Compatibility Path)

`Super+LMB` is the primary drag path. However, `InputInterceptor` also listens to `EVENT_SYSTEM_MOVESIZEEND` from the OS (via `WindowRegistry`'s event feed) to handle cases where the user drags the native titlebar without Super held. This path calls the same `LayoutEngine.move_snap()` pipeline, but without real-time window-following. The window snaps only after the user releases.

