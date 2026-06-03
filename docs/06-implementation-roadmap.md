# Implementation Roadmap

## Guiding Principle

Build the system in layers where each layer is independently testable and usable. Do not add mouse interaction or config complexity until the core layout loop works.

---

## Phase 1 — Core Loop (MVP)

Get a working tiling WM with keyboard control. No mouse gestures, no config file, no animation.

**Tasks:**
- `stm-ipc`: Define `SocketMessage` and `SocketResponse` types. Set up named pipe transport.
- `stm-registry`: Register `WinEventHook`s. Implement `EVENT_OBJECT_CREATE/DESTROY`, `EVENT_SYSTEM_FOREGROUND`. Basic `Tiling`/`Floating`/`Ignored` state machine. Hardcode classification rules.
- `stm-layout`: Implement `VirtualLayout` and `ActualLayout` data structures. Implement `ScrollLeft/Right`, `FocusLeft/Right/Up/Down`. Virtual → Actual projection. Direct `SetWindowPos` calls (no animation yet).
- `stmd`: Main event loop wiring all subsystems. Accept IPC messages, dispatch to layout.
- `stm-cli`: Implement `stm start/stop` and basic layout commands.

**Exit criteria**: User can open windows, scroll left/right through columns, and move focus with keyboard commands sent via `stm` CLI.

---

## Phase 2 — Window States & Recovery

Handle the full window state lifecycle.

**Tasks:**
- `stm-registry`: Add `EVENT_SYSTEM_MINIMIZESTART/END`, `EVENT_SYSTEM_MOVESIZEEND`. Implement fullscreen/maximize detection. Write recovery snapshot on every mutation.
- `stm-layout`: Implement `SwapLeft/Right/Up/Down`, `SwapWithOffscreen`, `ExpandColumn/ShrinkColumn`, `MergeColumn`. Handle empty-column cleanup (column disappears when last window closes).
- `stm-watchdog`: Implement watchdog binary. Wire into `stmd` startup.
- `stm-cli`: Implement `stm restore`.

**Exit criteria**: All window state transitions handled correctly. Crash recovery restores windows to `pre_manage_rect`.

---

## Phase 3 — Animation

Wire in `window-animation` crate.

**Tasks:**
- `stm-layout`: Replace direct `SetWindowPos` with `LayoutDiff` → `Vec<WindowMove>` → `window-animation` pipeline. Implement `AnimationHint` variants.
- Tune easing per hint type (Snap, Displaced, ScrollEnter, ScrollExit).

**Exit criteria**: All layout mutations animate smoothly. `Restore` hint skips animation.

---

## Phase 4 — Config & Persistence

Make the WM configurable without recompilation.

**Tasks:**
- `stm-config`: Implement YAML parser. Generate JSON Schema via `schemars`. Write schema to `%APPDATA%\stm\`. Implement `stm check-config` and `stm set`.
- `stm-persist`: Implement persist store. Wire precedence: persist > rules > default.
- `stmd`: Hot-reload config on `ReloadConfig` message.

**Exit criteria**: Users can configure hotkeys, window rules, gaps, and animation settings. Per-app float/tile preference persists across launches.

---

## Phase 5 — Mouse Input

Add `Super+LMB` drag and `Super+RMB` resize.

**Tasks:**
- `stm-input`: Register `LowLevelKeyboardProc` for Super key tracking. Register `LowLevelMouseProc`. Implement `DragSession` and `ResizeSession`. Real-time window following during drag. Emit `DragEnd` / `ResizeEnd` events.
- `stm-layout`: Implement `MoveSnap` and `ResizeSnap`. Column insertion vs merge heuristic. Neighbor adjustment on resize.
- Wire native titlebar drag compatibility path via `EVENT_SYSTEM_MOVESIZEEND`.

**Exit criteria**: User can drag tiling windows to new column slots with animated snap. Resize snaps to eighths grid.

---

## Phase 6 — IPC Subscriptions & Integrations

Enable status bars and external tools.

**Tasks:**
- `stm-ipc`: Implement event subscription stream. Emit `SocketEvent`s on all state changes.
- `stm-cli`: Implement `stm query` commands.
- Export `socket-schema` and `config-schema` commands.
- Documentation for status bar integration (yasb, glazebar).

---

## Phase 7 — GUI Config (Later)

A visual configuration editor.

**Tasks:**
- Decide: Tauri app vs. embedded web UI vs. native Win32.
- Config editor with live preview.
- Window rules builder with drag-and-drop.
- Hotkey recorder.

This phase has no timeline. The CLI + YAML path must remain fully functional regardless of GUI config status.

---

## Known Deferred Decisions

| Decision | When to revisit |
|---|---|
| Multi-monitor: one canvas per monitor vs. one shared canvas | Phase 1 (must decide before `ActualLayout` projection) |
| Overlay layer implementation detail | Phase 2 |
| Snap preview overlay window | Phase 5 |
| Lua config support | Phase 7 or user demand |
| `toggle_monocle` behavior (maximized single window vs WM-managed fill) | Phase 2 |

