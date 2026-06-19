# ScrollTilingManager Orchestrator — Architecture Spec

> **Spec ID**: `07-daemon-core-architecture`
> **Status**: Draft — for review before implementation
> **Depends on**: `00-overview`, `01-window-registry`, `02-layout-engine`, `06-implementation-roadmap`

> **⚠ Architectural evolution (post-implementation).** This spec documents the
> original daemon refactor that *introduced* `ScrollTilingManager`. That refactor
> has since itself been refactored to support a niri-style virtual-desktop model:
>
> - `LayoutEngine` → **`ScrollingSpace`**, now living in `src/workspace/scrolling_space.rs`
>   (not `src/layout/engine.rs`).
> - `FloatingManager` → **`FloatingSpace`**, now a field on `Workspace` in
>   `src/workspace/floating_space.rs` (not a top-level field on the daemon).
> - `ScrollTilingManager.layout: ScrollingSpace` + `.floating: FloatingManager`
>   → **`ScrollTilingManager.monitors: Vec<Monitor>` + `.active_monitor: usize`**.
>   Each `Monitor` owns `Vec<Workspace>`; each `Workspace` owns a `ScrollingSpace`
>   and a `FloatingSpace`. Access the active scrolling space via
>   `self.active_scrolling()` / `self.active_scrolling_mut()`.
>
> The prose, call-site pseudocode, and startup sequence below have been updated
> to the current shape. The historical "Files Changed" tables (§14, §15) describe
> the *original* refactor as implemented and are left intact for archaeology —
> the `tiling/` module listed there was never built. For the current source of
> truth, read `src/workspace/mod.rs` and `src/daemon/types.rs`.

---

## 1. Motivation

The daemon currently wires its components together ad-hoc in `main.rs`:

- `Arc<Mutex<WindowRegistry>>` is constructed, locked, and unlocked manually
- The IPC dispatch function (`dispatch_with_registry`) receives `Arc<Mutex<...>>` as a parameter
- Hook events are consumed by the registry's own `process_pending_events()` method
- Layout engine operations are completely unwired — no IPC command actually calls any layout method
- Animation does not exist yet

This works for Phase 1 (registry + IPC skeleton) but breaks down when we need to
route events **between** subsystems:

```
Win32 hook: window created → registry classifies it → if tiling, add to ScrollingSpace → animate
IPC command: stm move left → ScrollingSpace.swap_column() → animate
IPC command: stm stop → save recovery snapshot → shutdown
```

Each of these flows touches 2–3 subsystems in sequence. Without a single
coordination point, the code becomes a tangled web of cross-module calls with
no clear ownership.

---

## 2. Design: `ScrollTilingManager`

### 2.1 The Single Orchestrator

`ScrollTilingManager` is the single top-level struct that **owns** every subsystem
and acts as the **event router** between them. No subsystem knows about any other
subsystem — they only expose methods that take inputs and return outputs.

```text
┌──────────────────────────────────────────────────────────────────────┐
│                      ScrollTilingManager                             │
│                                                                      │
│  Owns:                                                               │
│  ┌────────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
│  │ WindowRegistry │  │ Vec<Monitor> │  │ WindowAnimator           │  │
│  │ (window state) │  │  └ Workspace │  │ (src/animation/)         │  │
│  │                │  │     ├ ScrollingSpace (layout math)         │  │
│  │                │  │     └ FloatingSpace (stub)                 │  │
│  └────────────────┘  └──────────────┘  └──────────────────────────┘  │
│  ┌────────────────┐  ┌──────────────┐                                │
│  │ PipeServer     │  │ AppConfig    │                                │
│  │ (IPC transport)│  │ (loaded once)│                                │
│  └────────────────┘  └──────────────┘                                │
│                                                                      │
│  Routes:                                                             │
│  • Hook events  → registry mutation → active ScrollingSpace → animator │
│  • IPC commands → active ScrollingSpace / registry query → animator   │
│  • Config reload → update MutationConfig in active ScrollingSpace     │
└──────────────────────────────────────────────────────────────────────┘
```

### 2.2 Why Not `DaemonCore`?

The name `ScrollTilingManager` was chosen over `DaemonCore` because:

1. It matches the project name and the daemon binary (`stmd` = ScrollingTilingManager daemon)
2. It is self-documenting — a newcomer immediately knows what it does
3. The struct is the **entire application** — it's not a "core" inside something larger

### 2.3 Struct Definition

```rust
/// The single orchestrator for the ScrollingTilingManager daemon.
///
/// Owns all subsystems and routes events between them.
/// Lives entirely on the IPC thread — no interior mutability needed.
pub struct ScrollTilingManager {
    // --- Subsystems ---
    registry: WindowRegistry,
    monitors: Vec<Monitor>,       // workspace hierarchy (workspace::Monitor)
    active_monitor: usize,        // index into `monitors`
    animator: animation::WindowAnimator,

    // --- IPC ---
    server: PipeServer,

    // --- Config ---
    config: AppConfig,
    config_dir: PathBuf,

    // --- Hook state ---
    hook_receiver: Receiver<HookEvent>,
    _hook_handle: HookThreadHandle,

    // --- Shutdown ---
    shutting_down: bool,
}
```

Each `Monitor` owns `Vec<Workspace>`; each `Workspace` owns a `ScrollingSpace`
(the tiling engine, formerly known as `LayoutEngine`) plus a `FloatingSpace`
stub. The active workspace's scrolling space is reached via the
`active_scrolling()` / `active_scrolling_mut()` accessors.

### 2.4 Key Design Property: No `Arc<Mutex<>>`

The previous architecture used `Arc<Mutex<WindowRegistry>>` because the IPC
loop and hook events were consumed by different parts of the code. With
`ScrollTilingManager` owning everything, the threading model simplifies:

```
Hook Thread (background):          IPC Thread (main):
  SetWinEventHook ×3                owns ScrollTilingManager (all fields)
  GetMessageW loop                  ├─ process_hook_events()
      ↓ callback                    ├─ dispatch IPC command
  sender.send(HookEvent)            ├─ process_hook_events()
                                    └─ ... (repeat)
```

**The hook thread never touches any STM field.** It only sends `HookEvent`
through the `mpsc` channel. The IPC thread reads the channel and calls methods
on `registry`, the active workspace's `scrolling` space, and `animator` directly — no mutex, no locking,
no deadlocks.

Since all subsystem methods take `&mut self`, the borrow checker enforces
exclusive access at compile time. This is strictly safer than `Mutex` (which
only enforces at runtime and can deadlock).

---

## 3. Event Pipelines Examples

### 3.1 Win32 Hook Pipeline

When a window is created by the OS, the event flows through:

```text
Windows OS
    │
    │ EVENT_OBJECT_CREATE
    ▼
Hook Thread: hook_callback()
    │
    │ sender.send(HookEvent::Created { hwnd })
    ▼
IPC Thread: ScrollTilingManager::process_hook_events()
    │
    │ try_recv() loop
    ▼
Created { hwnd } → ScrollTilingManager::on_window_created(hwnd)
    │
    ├── registry.handle_created(hwnd) → Option<WindowId>
    │       │
    │       ├── If invisible/untitled/owned/already-tracked → return None
    │       ├── Query WindowInfo, classify via pipeline
    │       ├── If Ignored (maximized/fullscreen/explicit rule) → return None
    │       ├── If Floating → register, return None
    │       └── If Tiling → register as Tiling::Active, return Some(WindowId)
    │
    ├── If None → done (no layout change needed)
    │
    └── If Some(window_id) →
            scrolling.add_window(window_id) → LayoutDiff
            self.animate_diff(diff)
```

#### 3.1.1 Window Destroyed

```text
HookEvent::Destroyed { hwnd }
    │
    ▼
ScrollTilingManager::on_window_destroyed(hwnd)
    │
    ├── Check if registry has this window
    ├── Check if window was in tiling state
    │       │
    │       ├── If tiling →
    │       │     scrolling.remove_window(WindowId(hwnd)) → LayoutDiff
    │       │     self.animate_diff(diff)
    │       │
    │       └── If floating/ignored → no layout change
    │
    └── registry.remove_window(hwnd)
```

#### 3.1.3 Window Minimized

```text
HookEvent::MinimizeStart { hwnd }
    │
    ▼
ScrollTilingManager::on_window_minimized(hwnd)
    │
    ├── registry.minimize_window(hwnd)
    │
    └── If window was tiling-active →
            scrolling.remove_window(WindowId(hwnd)) → LayoutDiff
            self.animate_diff(diff)
```

#### 3.1.4 Window Restored

```text
HookEvent::MinimizeEnd { hwnd }
    │
    ▼
ScrollTilingManager::on_window_restored(hwnd)
    │
    ├── registry.restore_window(hwnd)
    │
    └── If window is now Tiling::Active →
            scrolling.add_window(WindowId(hwnd)) → LayoutDiff
            self.animate_diff(diff)
```

#### 3.1.5 Focus Changed

```text
HookEvent::Foreground { hwnd }
    │
    ▼
ScrollTilingManager::on_focus_changed(hwnd)
    │
    ├── registry.set_focused(hwnd)
    │
    └── If window is tiling →
            scrolling.set_focus(WindowId(hwnd)) → LayoutDiff
            self.animate_diff(diff)
```

### 3.2 IPC Command Pipeline

When the user runs `stm move left`:

```text
stm CLI
    │
    │ SocketMessage::SwapLeft
    ▼
Named Pipe → PipeServer.read_message()
    │
    ▼
ScrollTilingManager::dispatch(msg) → SocketResponse
    │
    ├── match SocketMessage::SwapLeft →
    │       self.active_scrolling_mut().swap_column(Direction::Left) → Option<LayoutDiff>
    │       if Some(diff) → self.animate_diff(diff)
    │       SocketResponse::Ok
    │
    ├── match SocketMessage::QueryWindowsAll →
    │       SocketResponse::Data { registry.to_json_value() }
    │
    ├── match SocketMessage::Stop →
    │       self.shutting_down = true
    │       SocketResponse::Ok
    │
    └── ... (all other commands)
```

### 3.3 The `animate_diff` Bridge Method

This is the critical conversion point between the layout engine's output
and the animation system's input:

```rust
/// Convert a LayoutDiff into animation targets and submit to the animator.
fn animate_diff(&mut self, diff: LayoutDiff) {
    if diff.moves.is_empty() {
        return;
    }

    let targets: Vec<WindowTarget> = diff.moves.iter().map(|wm| {
        // Convert STM Rect { x, y, width, height } → animation Rect { x, y, w, h }
        WindowTarget::new(
            WindowRef(wm.window_id.0),   // WindowId(isize) → WindowRef(isize)
            IVec2::new(wm.to.x, wm.to.y),
            IVec2::new(wm.to.width, wm.to.height),
        )
    }).collect();

    if let Err(e) = self.animator.animate(targets) {
        log::warn!("animation error: {e}");
    }
}
```

---

## 4. Module Structure

### 4.1 New Module Map

```text
src/
├── main.rs                  # Thin: args → ScrollTilingManager::new(args).run()
├── lib.rs                   # pub mod animation; pub mod workspace; pub mod daemon;
├── common/                  # Unchanged
├── config/                  # Unchanged
├── registry/                # Refactored (see §5.1)
├── layout/                  # Extended (see §5.2) — pure layout math (mutations, projection, diff)
├── ipc/                     # Refactored (see §5.3)
├── animation/               # NEW — embedded window-animation crate
│   ├── mod.rs               #   Re-exports (was lib.rs)
│   ├── animator.rs
│   ├── backend/
│   │   ├── mod.rs
│   │   ├── win32.rs
│   │   └── mock.rs
│   ├── config.rs
│   ├── easing.rs
│   ├── interpolation.rs
│   ├── batch.rs
│   ├── metrics.rs
│   └── types.rs
├── workspace/               # NEW — workspace hierarchy
│   ├── mod.rs               #   WorkspaceId, Workspace
│   ├── scrolling_space.rs   #   ScrollingSpace (was layout/engine.rs — the tiling engine)
│   ├── floating_space.rs    #   FloatingSpace stub
│   └── monitor.rs           #   Monitor (owns Vec<Workspace>)
└── daemon/                  # NEW — ScrollTilingManager
    └── mod.rs
```

### 4.2 New `lib.rs` Declarations

```rust
pub mod animation;   // Embedded window-animation crate
pub mod common;
pub mod config;
pub mod daemon;      // ScrollTilingManager orchestrator
pub mod ipc;
pub mod layout;
pub mod registry;
pub mod workspace;   // ScrollingSpace + FloatingSpace + Workspace + Monitor
```

---

## 5. Refactoring Existing Code

### 5.1 WindowRegistry Changes

**Goal**: Decouple the registry from the mpsc channel. Instead of consuming
events internally, the registry exposes individual handler methods that
`ScrollTilingManager` calls.

#### 5.1.1 Remove `process_pending_events()`

This method currently takes `&Receiver<HookEvent>` and loops over events
internally. It will be removed. `ScrollTilingManager` reads the channel and
dispatches to individual methods.

#### 5.1.2 Change `handle_created()` Signature

Currently `handle_created()` is private and returns `()`. New signature:

```rust
/// Handle a window creation event.
///
/// Queries Win32 metadata, classifies the window, and registers it.
///
/// Returns `Some(WindowId)` if the window was classified as tiling,
/// `None` if it was ignored, floating, or skipped.
pub fn handle_created(&mut self, hwnd_val: isize) -> Option<WindowId>
```

The return value tells `ScrollTilingManager` whether to pass this window to
the layout engine. The implementation is identical to the current private
method — only the return type changes:

```rust
pub fn handle_created(&mut self, hwnd_val: isize) -> Option<WindowId> {
    // ... existing logic (visibility check, title check, owner check) ...

    self.register_window_from_info(&info);

    // Check the classification result
    let key = hwnd_val;
    if let Some(window) = self.windows.get(&key) {
        match &window.state {
            WindowState::Tiling(_) => Some(WindowId(hwnd_val)),
            _ => None,  // floating or ignored
        }
    } else {
        None
    }
}
```

#### 5.1.3 Add `tiling_window_ids()` Accessor

Needed by `initialize_windows()` for the startup batch:

```rust
/// Returns the WindowIds of all currently tiling-active windows.
///
/// Used at startup to build the initial layout in one batch operation.
pub fn tiling_window_ids(&self) -> Vec<WindowId> {
    self.windows.iter()
        .filter_map(|(key, w)| {
            match &w.state {
                WindowState::Tiling(TilingState::Active { .. }) => Some(WindowId(*key)),
                _ => None,
            }
        })
        .collect()
}
```

#### 5.1.4 Add Helper to Check if Window Was Tiling

```rust
/// Check if a window is in tiling state (before removal).
pub fn is_tiling(&self, hwnd_val: isize) -> bool {
    self.windows.get(&hwnd_val)
        .map(|w| matches!(w.state, WindowState::Tiling(_)))
        .unwrap_or(false)
}
```

#### 5.1.5 Summary of Registry Changes

| Method | Change |
|--------|--------|
| `process_pending_events()` | **Removed** — STM reads channel directly |
| `handle_created()` | **Public**, returns `Option<WindowId>` |
| `tiling_window_ids()` | **New** — returns IDs for batch init |
| `is_tiling()` | **New** — check before layout removal |
| `remove_window()` | Unchanged |
| `minimize_window()` | Unchanged |
| `restore_window()` | Unchanged |
| `set_focused()` | Unchanged |
| `to_json_value()` | Unchanged |

### 5.2 ScrollingSpace Changes

> **Historical.** This subsection documents the original daemon refactor's
> addition of the `initialize_windows()` batch operation to the then-named
> `LayoutEngine`. The method still exists with the same signature on
> `ScrollingSpace` (`src/workspace/scrolling_space.rs`); only the type name and
> file path have changed. The narrative below is preserved as history.

#### 5.2.1 Add `initialize_windows()` Batch Operation

Currently, adding windows to the layout requires calling `add_window()` in a loop,
which recomputes projection and diff for each window. For startup (when the registry
already has N tiling windows), we need a single batch operation:

```rust
/// Initialize the layout with multiple windows in one operation.
///
/// Creates one column per window, assigns default widths, and computes
/// a single LayoutDiff covering all windows. This is more efficient than
/// calling `add_window()` N times because it does a single projection + diff.
///
/// Used at daemon startup when the registry already has tracked windows
/// from the init scan.
pub fn initialize_windows(&mut self, ids: Vec<WindowId>) -> LayoutDiff {
    let new_layout = mutations::initialize_windows(&self.virtual_layout, &ids, &self.config);

    // Focus the first window (or last — design choice, see note below).
    self.focused = ids.last().copied();

    self.apply_mutation(new_layout)
}
```

The corresponding mutation in `mutations.rs`:

```rust
/// Build a complete virtual layout from a list of window IDs.
///
/// Creates one column per window with the default width. Does not
/// disturb any existing virtual layout (called on an empty layout).
#[must_use]
pub fn initialize_windows(
    layout: &VirtualLayout,
    ids: &[WindowId],
    config: &MutationConfig,
) -> VirtualLayout {
    let columns: Vec<Column> = ids.iter()
        .map(|&id| Column::new(config.column_width as i32, id))
        .collect();

    VirtualLayout {
        columns,
        viewport_offset: 0,
    }
}
```

#### 5.2.2 Summary of ScrollingSpace Changes

| Method | Change |
|--------|--------|
| `initialize_windows()` | **New** — batch layout init from list of WindowIds |
| All existing methods | **Unchanged** — pure layout math stays pure |
| `mutations::initialize_windows()` | **New** — pure function, no side effects |

### 5.3 IPC Dispatch Changes

#### 5.3.1 Remove `dispatch_with_registry()`

The standalone function `dispatch_with_registry()` is removed. Instead,
`ScrollTilingManager` has a `dispatch()` method that has direct `&mut` access
to all subsystems:

```rust
impl ScrollTilingManager {
    /// Dispatch an IPC command and return the response.
    fn dispatch(&mut self, msg: &SocketMessage) -> SocketResponse {
        match msg {
            SocketMessage::Stop => {
                self.shutting_down = true;
                SocketResponse::Ok
            }

            SocketMessage::QueryWindowsAll => {
                SocketResponse::Data { payload: self.registry.to_json_value() }
            }

            SocketMessage::FocusLeft => self.dispatch_focus(Direction::Left),
            SocketMessage::FocusRight => self.dispatch_focus(Direction::Right),
            SocketMessage::FocusUp => self.dispatch_focus(Direction::Up),
            SocketMessage::FocusDown => self.dispatch_focus(Direction::Down),

            SocketMessage::SwapLeft => self.dispatch_swap(Direction::Left),
            SocketMessage::SwapRight => self.dispatch_swap(Direction::Right),
            SocketMessage::SwapUp => self.dispatch_swap(Direction::Up),
            SocketMessage::SwapDown => self.dispatch_swap(Direction::Down),

            SocketMessage::ScrollLeft => self.dispatch_scroll_left(),
            SocketMessage::ScrollRight => self.dispatch_scroll_right(),

            SocketMessage::ExpandColumn => self.dispatch_expand(),
            SocketMessage::ShrinkColumn => self.dispatch_shrink(),

            // ... remaining commands ...
            _ => SocketResponse::Error {
                message: format!("command not yet implemented"),
            },
        }
    }

    fn dispatch_focus(&mut self, dir: Direction) -> SocketResponse {
        match self.active_scrolling_mut().focus(dir) {
            Some(focused) => {
                // LayoutDiff is produced internally if viewport scrolled
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "no window to focus in that direction".into(),
            },
        }
    }

    fn dispatch_swap(&mut self, dir: Direction) -> SocketResponse {
        match self.active_scrolling_mut().swap_column(dir) {
            Some(diff) => {
                self.animate_diff(diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot swap in that direction".into(),
            },
        }
    }

    // ... etc for scroll, expand, shrink, monocle ...
}
```

### 5.4 Main.rs Simplification

`main.rs` becomes a thin wrapper:

```rust
fn main() {
    env_logger::init();
    let args = Args::parse();

    if let Err(e) = run(args) {
        log::error!("stmd: fatal error: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), String> {
    // Config resolution stays here (pre-STM construction).
    let config_dir = dirs::resolve_config_dir(args.config.as_deref().map(Path::new));
    log::info!("using config directory: {}", config_dir.display());

    init_config_dir(&config_dir)?;

    let app_config = load_app_config(&dirs::user_app_config_path_in(&config_dir));
    let user_rules = load_rules_config(&dirs::user_rules_path_in(&config_dir));
    let default_rules = load_default_rules();

    // Optional: switch to test desktop.
    #[cfg(debug_assertions)]
    if let Some(ref name) = args.desktop {
        desktop::switch_to_desktop(name)?;
    }
    #[cfg(debug_assertions)]
    let desktop_name = args.desktop.clone();
    #[cfg(not(debug_assertions))]
    let desktop_name = None;

    // Build and run — everything from here is STM's responsibility.
    let stm = ScrollTilingManager::new(
        app_config, user_rules, default_rules, config_dir, desktop_name,
    )?;
    stm.run();

    Ok(())
}
```

---

## 6. TilingManager Wrapper (Optional Layer)

The session notes mention a "thin TilingManager wrapper" between
`ScrollTilingManager` and `ScrollingSpace + WindowAnimator`. This layer
is optional and can be introduced later if the coordination logic in
`ScrollTilingManager` becomes too complex. For the initial implementation,
the methods live directly on `ScrollTilingManager`:

```text
Option A (simpler, recommended for now):
    ScrollTilingManager
        ├── registry.handle_created() → Option<WindowId>
        ├── layout.add_window() → LayoutDiff
        └── animator.animate(targets)

Option B (if STM grows too large):
    ScrollTilingManager
        └── TilingManager
              ├── layout: ScrollingSpace
              ├── animator: WindowAnimator
              └── add_and_animate(id)  // coordinates layout + animation
```

We start with **Option A** and extract `TilingManager` only if
`ScrollTilingManager` exceeds a comfortable size.

---

## 7. Animation Module (`src/animation/`)

### 7.1 Embedding Strategy

The `window-animation` crate from `C:\Users\Projects\window-animation\` is
copied into `src/animation/` as a module. **This is a full copy** including:

- All source files (`animator.rs`, `types.rs`, `config.rs`, `easing.rs`,
  `interpolation.rs`, `batch.rs`, `metrics.rs`, `backend/mod.rs`,
  `backend/win32.rs`, `backend/mock.rs`)
- All unit tests within those files
- All dependencies (`crossbeam-channel`, etc.) added to STM's `Cargo.toml`

The crate's `lib.rs` becomes `src/animation/mod.rs`.

### 7.2 Path Migration

Every `use crate::xxx` inside the animation files changes to
`use crate::animation::xxx`:

| Original (standalone crate) | Embedded (STM module) |
|-----------------------------|-----------------------|
| `use crate::animator::` | `use crate::animation::animator::` |
| `use crate::backend::` | `use crate::animation::backend::` |
| `use crate::config::` | `use crate::animation::config::` |
| `use crate::types::` | `use crate::animation::types::` |
| `use crate::batch::` | `use crate::animation::batch::` |
| `use crate::metrics::` | `use crate::animation::metrics::` |
| `use crate::easing::` | `use crate::animation::easing::` |
| `use crate::interpolation::` | `use crate::animation::interpolation::` |

### 7.3 Type Coexistence

STM has `crate::common::Rect` with fields `{x, y, width, height}`.
Animation has `crate::animation::types::Rect` with fields `{x, y, w, h}`.

These are **different types** and will coexist. The conversion happens only
at the bridge point (`animate_diff`):

```rust
// STM Rect → animation IVec2 (position)
IVec2::new(stm_rect.x, stm_rect.y)

// STM Rect → animation IVec2 (size)
IVec2::new(stm_rect.width, stm_rect.height)
```

No renaming or refactoring of either `Rect` type is needed. The module
boundary keeps them separate.

### 7.4 Public API Surface

The `animation` module re-exports only what STM needs:

```rust
// src/animation/mod.rs
pub(crate) mod animator;
pub(crate) mod backend;
pub(crate) mod config;
pub(crate) mod easing;
pub(crate) mod interpolation;
pub(crate) mod metrics;
pub(crate) mod types;
pub(crate) mod batch;

// Public re-exports for use by ScrollTilingManager
pub use animator::WindowAnimator;
pub use config::AnimatorConfig;
pub use types::{IVec2, Rect, WindowRef, WindowTarget};
```

### 7.5 Win32 Backend Feature Flag

The animation crate's `backend/win32.rs` uses `DwmFlush` from
`Win32::Graphics::Dwm`. STM's `Cargo.toml` needs this feature flag:

```toml
windows = { version = "0.62.2", features = [
    # ... existing features ...
    "Win32_Graphics_Dwm",
] }
```

---

## 8. FloatingSpace Stub

Floating windows are tracked by the registry but not managed by the layout
engine. For now, `FloatingSpace` is an empty placeholder:

```rust
// src/workspace/floating_space.rs

/// Manager for floating (non-tiled) windows.
///
/// Currently a stub — floating windows are tracked by WindowRegistry
/// but their positioning is left to the OS. Future work may add
/// floating window stacking, smart placement, etc.
pub struct FloatingSpace;

impl FloatingSpace {
    /// Create a new floating manager.
    pub fn new() -> Self {
        Self
    }
}
```

---

## 9. Startup Sequence

The full initialization order in `ScrollTilingManager::new()`:

```text
1. Create WindowRegistry (from user_rules + default_rules)
2. registry.scan_existing_windows()
3. Create ScrollingSpace (from AppConfig → MonitorInfo, column_width, padding)
4. Wrap it: Workspace::new(WorkspaceId(1), scrolling) → Monitor::new(work_area, vec![workspace], 0)
5. Get tiling_window_ids from registry
6. active_scrolling_mut().initialize_windows(tiling_window_ids)  // batch init
7. Create WindowAnimator (from AnimatorConfig + Win32Backend)
8. animate_diff(initial_diff)  // animate windows to their initial positions
9. start_hook_thread() → (hook_receiver, _hook_handle)
10. Create PipeServer
11. Return ScrollTilingManager { monitors: vec![monitor], active_monitor: 0, ... }
```

After construction, `stm.run()` enters the IPC loop (see §10).

---

## 10. Main Event Loop

```rust
impl ScrollTilingManager {
    /// Run the main event loop. Blocks until Stop command or fatal error.
    pub fn run(&mut self) {
        log::info!("stmd: daemon started, listening on named pipe");

        loop {
            // Wait for a client connection.
            if let Err(e) = self.server.wait_for_client() {
                log::error!("stmd: failed to accept client: {e}");
                break;
            }

            // Process messages from this client.
            loop {
                // 1. Drain hook events BEFORE each IPC message.
                self.process_hook_events();

                // 2. Read next IPC message (blocking).
                match self.server.read_message() {
                    Ok(msg) => {
                        let response = self.dispatch(&msg);
                        let is_stop = self.shutting_down;

                        if let Err(e) = self.server.write_response(&response) {
                            log::warn!("stmd: failed to write response: {e}");
                            break;
                        }

                        if is_stop {
                            log::info!("stmd: shutting down");
                            return;
                        }
                    }
                    Err(e) => {
                        log::debug!("stmd: client read error: {e}");
                        break;
                    }
                }
            }

            // Disconnect the client.
            if let Err(e) = self.server.disconnect() {
                log::warn!("stmd: failed to disconnect client: {e}");
            }
        }
    }

    /// Drain all pending hook events and route them to subsystems.
    fn process_hook_events(&mut self) {
        while let Ok(event) = self.hook_receiver.try_recv() {
            match event {
                HookEvent::Created { hwnd } => {
                    self.on_window_created(hwnd);
                }
                HookEvent::Destroyed { hwnd } => {
                    self.on_window_destroyed(hwnd);
                }
                HookEvent::Foreground { hwnd } => {
                    self.on_focus_changed(hwnd);
                }
                HookEvent::MinimizeStart { hwnd } => {
                    self.on_window_minimized(hwnd);
                }
                HookEvent::MinimizeEnd { hwnd } => {
                    self.on_window_restored(hwnd);
                }
            }
        }
    }
}
```

---

## 11. `ScrollTilingManager::new()` Constructor

```rust
impl ScrollTilingManager {
    /// Construct and initialize the daemon.
    ///
    /// Performs all startup work: config loading, window scanning,
    /// layout initialization, animation setup, and hook registration.
    ///
    /// Returns a fully initialized STM ready to call `.run()`.
    pub fn new(
        app_config: AppConfig,
        user_rules: WindowRulesConfig,
        default_rules: WindowRulesConfig,
        config_dir: PathBuf,
        desktop_name: Option<String>,
    ) -> Result<Self, String> {
        // 1. Registry
        let mut registry = WindowRegistry::new(&user_rules, &default_rules);
        registry.scan_existing_windows()?;

        // 2. ScrollingSpace — derive params from AppConfig.
        let monitor = MonitorInfo {
            work_area: registry_win32::get_primary_monitor_work_area()?,
        };
        let layout_config = Self::derive_layout_config(&app_config, &monitor);
        let mut scrolling = ScrollingSpace::new(
            monitor,
            layout_config.column_width,
            layout_config.min_column_width_px,
            layout_config.padding,
            layout_config.columns_per_screen,
        );

        // 3. Batch-initialize the scrolling space from existing tiling windows.
        let tiling_ids = registry.tiling_window_ids();
        if !tiling_ids.is_empty() {
            let diff = scrolling.initialize_windows(tiling_ids);
            // Diff will be animated after animator is created below.
            // Store the diff temporarily.
            let initial_diff = Some(diff);
        } else {
            let initial_diff = None;
        }

        // 4. Animator.
        let backend = animation::backend::win32::Win32Backend::new();
        let anim_config = Self::derive_animator_config(&app_config);
        let mut animator = WindowAnimator::new(backend, anim_config);

        // 5. Animate initial layout (snap windows to their tiling positions).
        if let Some(diff) = initial_diff {
            // Use Restore hint for initial placement — no animation, instant snap.
            // Or use normal animation for a polished startup feel.
            Self::animate_diff_static(&mut animator, diff);
        }

        // 6. Start hook thread.
        let (hook_receiver, _hook_handle) = hooks::start_hook_thread(desktop_name)?;

        // 7. IPC server.
        let server = PipeServer::create()
            .map_err(|e| format!("failed to create pipe (is another daemon running?): {e}"))?;

        // 8. Wrap the scrolling space in a Workspace, then a Monitor.
        //    Skeleton invariant: exactly one monitor, one workspace (id 1).
        //    Future work: niri-style multi-workspace with vertical scrolling.
        let workspace = Workspace::new(WorkspaceId(1), scrolling);
        let monitor = Monitor::new(monitor.work_area, vec![workspace], 0);

        Ok(Self {
            registry,
            monitors: vec![monitor],
            active_monitor: 0,
            animator,
            server,
            config: app_config,
            config_dir,
            hook_receiver,
            _hook_handle,
            shutting_down: false,
        })
    }
}
```

**Note**: The `initial_diff` variable flow above is pseudocode — the actual
implementation will handle the borrow checker by splitting the construction
into sequential steps.

---

## 12. New Win32 Helper

### 12.1 `get_primary_monitor_work_area()`

The layout engine needs the monitor work area at construction time. Currently
`main.rs` doesn't query this at all. A new function in `registry/win32.rs`:

```rust
/// Get the work area of the primary monitor (excluding taskbar).
///
/// Uses `SystemParametersInfoW` with `SPI_GETWORKAREA` which returns
/// the primary monitor's work area. For multi-monitor support, this
/// would need to be replaced with `MonitorFromPoint` + `GetMonitorInfoW`.
///
/// # Errors
///
/// Returns an error string if the Win32 call fails.
pub fn get_primary_monitor_work_area() -> Result<Rect, String> {
    let mut rect = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut _ as *mut _),
            0,
        )
    }
    .map_err(|e| format!("SystemParametersInfoW(SPI_GETWORKAREA) failed: {e}"))?;

    Ok(Rect {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}
```

This requires adding `Win32_UI_WindowsAndMessaging` feature's `SPI_GETWORKAREA`
constant to the imports in `registry/win32.rs`.

---

## 13. Dependency Changes

### 13.1 New Cargo.toml Dependencies

```toml
[dependencies]
# ... existing dependencies ...

# Required by embedded animation module (was in window-animation's Cargo.toml)
crossbeam-channel = "0.5"

# Required by Win32Backend's DwmFlush()
# (added to existing windows features list)
# "Win32_Graphics_Dwm"
```

### 13.2 Feature Flag Addition

```toml
windows = { version = "0.62.2", features = [
    "Win32_Foundation",
    "Win32_System_Pipes",
    "Win32_System_IO",
    "Win32_Storage_FileSystem",
    "Win32_Security",
    "Win32_UI_WindowsAndMessaging",
    "Win32_UI_Accessibility",
    "Win32_System_Threading",
    "Win32_System_StationsAndDesktops",
    "Win32_Graphics_Gdi",
    "Win32_Graphics_Dwm",        # NEW — DwmFlush for animation frame pacing
] }
```

---

## 14. Files Changed — Summary

### 14.1 New Files (to create)

| File | Purpose |
|------|---------|
| `src/daemon/mod.rs` | `ScrollTilingManager` struct and implementation |
| `src/animation/mod.rs` | Re-exports (was `window-animation/src/lib.rs`) |
| `src/animation/animator.rs` | Copied from window-animation |
| `src/animation/types.rs` | Copied from window-animation |
| `src/animation/config.rs` | Copied from window-animation |
| `src/animation/easing.rs` | Copied from window-animation |
| `src/animation/interpolation.rs` | Copied from window-animation |
| `src/animation/batch.rs` | Copied from window-animation |
| `src/animation/metrics.rs` | Copied from window-animation |
| `src/animation/backend/mod.rs` | Copied from window-animation |
| `src/animation/backend/win32.rs` | Copied from window-animation |
| `src/animation/backend/mock.rs` | Copied from window-animation |
| `src/workspace/mod.rs` | `WorkspaceId`, `Workspace` (id + scrolling + floating) |
| `src/workspace/scrolling_space.rs` | `ScrollingSpace` — the tiling engine (moved from `layout/`) |
| `src/workspace/floating_space.rs` | `FloatingSpace` stub |
| `src/workspace/monitor.rs` | `Monitor` — owns `Vec<Workspace>` + active index |

### 14.2 Modified Files

| File | Change |
|------|--------|
| `src/main.rs` | Simplify to thin `ScrollTilingManager::new().run()` |
| `src/lib.rs` | Add `pub mod animation;`, `pub mod daemon;`, `pub mod workspace;` |
| `src/registry/core.rs` | Remove `process_pending_events()`, make `handle_created()` public with `Option<WindowId>` return, add `tiling_window_ids()`, add `is_tiling()` |
| `src/registry/win32.rs` | Add `get_primary_monitor_work_area()` |
| `src/workspace/scrolling_space.rs` | Add `initialize_windows()` method |
| `src/layout/mutations.rs` | Add `initialize_windows()` pure function |
| `src/ipc/dispatch.rs` | Remove `dispatch_with_registry()`, keep `dispatch()` as fallback |
| `Cargo.toml` | Add `crossbeam-channel = "0.5"`, add `"Win32_Graphics_Dwm"` feature |

### 14.3 Unchanged Files

| File | Why |
|------|-----|
| `src/common/types.rs` | `Rect`, `WindowId`, `Direction` — stable vocabulary types |
| `src/layout/types.rs` | `VirtualLayout`, `ActualLayout`, `LayoutDiff` — stable |
| `src/layout/projection.rs` | Pure math — no changes needed |
| `src/layout/diff.rs` | Pure math — no changes needed |
| `src/registry/types.rs` | `Window`, `WindowState` — no changes needed |
| `src/registry/classification.rs` | Pure logic — no changes needed |
| `src/registry/hooks.rs` | Thread setup — no changes needed (STM reads the receiver) |
| `src/ipc/transport.rs` | `PipeServer` — no changes needed |
| `src/ipc/message.rs` | `SocketMessage` / `SocketResponse` — no changes needed |
| `src/config/*` | Config loading — no changes needed |

---

## 15. Implementation Phases

### Phase 1a — Embed Animation Module (parallel with 1b)

**Files**: 11 new files in `src/animation/`
**Risk**: Low — pure copy + path migration

1. Copy all source files from `window-animation/src/` into `src/animation/`
2. Rename `lib.rs` → `mod.rs`
3. Replace all `use crate::xxx` → `use crate::animation::xxx`
4. Add `crossbeam-channel = "0.5"` and `Win32_Graphics_Dwm` to `Cargo.toml`
5. Add `pub mod animation;` to `src/lib.rs`
6. `cargo build` — must compile
7. `cargo test` — all animation unit tests must pass

### Phase 1b — Registry & Layout Extensions (parallel with 1a)

**Files**: `registry/core.rs`, `registry/win32.rs`, `layout/engine.rs`, `layout/mutations.rs`

1. Add `get_primary_monitor_work_area()` to `registry/win32.rs`
2. Change `handle_created()` to public + `Option<WindowId>` return
3. Add `tiling_window_ids()` and `is_tiling()` to registry
4. Remove `process_pending_events()` from registry (keep tests updated)
5. Add `mutations::initialize_windows()` pure function
6. Add `ScrollingSpace::initialize_windows()` method
7. `cargo test` — all existing tests must still pass

### Phase 2 — ScrollTilingManager Core

**Files**: `src/daemon/mod.rs`, `src/workspace/floating_space.rs`, `src/main.rs`, `src/ipc/dispatch.rs`, `src/lib.rs`

1. Create `src/workspace/floating_space.rs` stub
2. Create `src/daemon/mod.rs` with `ScrollTilingManager` struct
3. Implement `new()` constructor with full startup sequence
4. Implement `run()` main loop
5. Implement `process_hook_events()` and individual `on_*` methods
6. Implement `dispatch()` with all IPC command routing
7. Implement `animate_diff()` bridge method
8. Simplify `main.rs` to thin wrapper
9. Add `pub mod daemon;`, `pub mod workspace;` to `lib.rs`
10. `cargo build` — must compile
11. `cargo test` — all tests pass

### Phase 3 — Integration Testing

1. Run daemon on test desktop with real Win32 dummy windows
2. Verify: windows snap to tiling positions on startup
3. Verify: opening a new window adds it to the layout with animation
4. Verify: closing a window removes it with animation
5. Verify: `stm move left/right` swaps columns with animation
6. Verify: `stm query windows all` returns correct registry state
7. Verify: `stm stop` shuts down cleanly

---

## 16. Open Questions

### 16.1 Startup Animation

Should the initial layout (from `initialize_windows`) animate or snap?
- **Snap** (instant): Feels more robust, no visible animation on startup
- **Animate**: Looks polished but may feel slow if there are many windows

**Recommendation**: Snap for now (use `AnimationHint::Restore` which maps to
zero-duration). Can be made configurable later.

### 16.2 `initialize_windows` Ordering

The order of window IDs determines column assignment. Options:
- **Insertion order** (order from `EnumWindows`): arbitrary but deterministic
- **Match current positions** (minimize total displacement): better UX, more complex

**Recommendation**: Start with insertion order (simpler). Position-matching is
a future enhancement tracked separately.

### 16.3 Focus on Startup

After `initialize_windows`, which window gets focus?
- **Last added**: Consistent with `add_window()` behavior
- **None**: Let the OS decide (whatever was focused before)
- **Previously focused**: Requires persistence (Phase 4)

**Recommendation**: Last added, for consistency with the `add_window` path.

### 16.4 Borrow Checker in Constructor

The `new()` method needs to:
1. Create the animator
2. Call `animate_diff` with the initial diff

This requires careful sequencing because `animate_diff` takes `&mut self`.
The constructor will need to either:
- Use a standalone function that takes `&mut animator` + `diff`
- Or defer the initial animation to the first `run()` iteration

**Recommendation**: Use a standalone `animate_diff_raw(&mut animator, diff)`
function for the constructor, and the `&mut self` method for the runtime loop.

---

## 17. Test Strategy

### 17.1 Unit Tests (no changes expected)

- All existing registry tests continue to pass (methods unchanged except `process_pending_events`)
- All existing layout engine tests (25+) continue to pass unchanged
- Animation crate's unit tests pass after path migration

### 17.2 New Unit Tests Needed

- `mutations::initialize_windows()` — empty list, single window, multiple windows
- `ScrollingSpace::initialize_windows()` — verify LayoutDiff has correct entries
- `ScrollTilingManager::animate_diff()` — verify conversion from LayoutDiff to WindowTarget
- `WindowRegistry::tiling_window_ids()` — mixed tiling/floating/ignored windows
- `WindowRegistry::is_tiling()` — all states

### 17.3 Integration Tests

- Full daemon lifecycle: start → open window → move → close → stop
- Hook event pipeline: verify registry + layout engine stay in sync
- IPC command pipeline: verify layout commands produce correct animation targets

---

## Appendix A: Type Conversion Reference

The bridge between STM types and animation types:

```text
STM                              Animation
─────────────────────────────    ─────────────────────────────
WindowId(isize)           →     WindowRef(isize)     [direct cast]
common::Rect { width, height }  →   IVec2 { x: width, y: height }  [field rename]
layout::AnimationHint            (not used by animator — diff already resolved)
LayoutDiff.moves                 →   Vec<WindowTarget>   [see animate_diff]
```

## Appendix B: Error Handling Strategy

| Error Source | Strategy |
|-------------|----------|
| Win32 API failure (registry) | Log warning, skip window, continue |
| Win32 API failure (animator backend) | Log warning, animation may be jarring |
| Layout operation returns `None` | Return `SocketResponse::Error` to CLI |
| PipeServer failure | Log error, break client loop, accept next client |
| Hook thread dies | Log error, daemon continues (no new hook events) |
| Constructor failure | Return `Err(String)`, `main.rs` exits with code 1 |
