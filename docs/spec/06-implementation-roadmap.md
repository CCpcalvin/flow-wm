# Implementation Roadmap

## Guiding Principle

Build the system in layers that can evolve inside a single package. Start with modules, not workspace crates. Extract a module into its own crate only when there is a concrete reason.

---

## Package Structure Goal

The implementation roadmap assumes this package structure:

```text
stm/
├── Cargo.toml
└── src/
    ├── main.rs
    ├── lib.rs
    ├── bin/
    │   ├── stm.rs
    │   └── stm-watchdog.rs
    ├── registry/
    ├── layout/
    ├── input/
    ├── config/
    ├── persist/
    ├── ipc/
    ├── animation/
    └── common/
```

---

## Phase 1 — Core Loop (MVP)

Get a working tiling WM with keyboard control. No mouse gestures, no config file editing, no animation polish.

**Tasks:**
- `src/ipc`: Define `SocketMessage` and `SocketResponse` types. Set up named pipe transport.
- `src/registry`: Register `WinEventHook`s. Implement create/destroy/focus handling. Hardcode classification rules.
- `src/layout`: Implement `VirtualLayout` and `ActualLayout`. Implement focus and scroll operations.
- `src/main.rs`: Wire the daemon event loop.
- `src/bin/stm.rs`: Implement `stm start`, `stm stop`, and basic command forwarding.

**Exit criteria:** User can open windows, scroll through columns, and move focus with keyboard commands.

---

## Phase 2 — Window States & Recovery

**Tasks:**
- Add minimize, restore, maximize, and fullscreen handling in `src/registry`
- Add swap, merge, resize-width logic in `src/layout`
- Implement recovery snapshot write/read helpers
- Implement `src/bin/stm-watchdog.rs`
- Implement `stm restore`

**Exit criteria:** Crash recovery works and all major window states are tracked correctly.

---

## Phase 3 — Animation

**Tasks:**
- Add `src/animation` as the bridge to the animation implementation
- Replace direct positioning in layout mutations with diff + animate
- Define animation hints for snap, scroll enter/exit, displaced, and restore

**Exit criteria:** Layout changes animate smoothly and consistently.

---

## Phase 4 — Config & Persistence

**Tasks:**
- Implement YAML parser and JSON Schema generation in `src/config`
- Implement persist store in `src/persist`
- Implement `stm check-config`, `stm set`, and `stm reload-config`
- Wire precedence: persist > rules > default

**Exit criteria:** Users can configure behavior without recompiling, and app-specific preferences are remembered.

---

## Phase 5 — Mouse Input

**Tasks:**
- Add keyboard and mouse hooks in `src/input`
- Implement `DragSession` and `ResizeSession`
- Add `MoveSnap` and `ResizeSnap` to `src/layout`
- Support native titlebar drag as a compatibility path

**Exit criteria:** Super-drag and Super-resize work with snapping and animation.

---

## Phase 6 — IPC Subscriptions & Integrations

**Tasks:**
- Add live event subscriptions in `src/ipc`
- Implement `stm query ...`
- Export protocol schema for external tools

**Exit criteria:** Status bars and other tools can observe daemon state.

---

## When to Extract a Crate Later

Move a module out of the single package only if at least one of these becomes true:

- the code is reusable outside `stm`
- another project or binary needs it as a clean dependency boundary
- compile times become a real bottleneck
- the API has stabilized and deserves a stronger separation boundary

Likely future extraction candidates:

- `src/animation` if the animation system becomes reusable elsewhere
- `src/ipc` if external tools need a standalone crate

Modules like `registry`, `layout`, and `input` should remain internal until a strong reason appears.
