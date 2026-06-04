# Implementation Status

## Summary

Phase 1 is complete: the OS-independent core of `stm` (ScrollingTilingManager). Three modules — `common`, `config`, and `layout` — are implemented in pure Rust with zero Win32 dependencies. All logic is testable on macOS and cross-compiles cleanly to `x86_64-pc-windows-msvc`. **132 tests pass** with zero clippy warnings and zero formatting issues.

---

## Module Map

| Module | Path | Status | Description |
|--------|------|--------|-------------|
| Common | `src/common/` | ✅ Implemented | Rect, Size, Point, Direction, WindowId, StmError |
| Config | `src/config/` | ✅ Implemented | YAML parsing, serde, schemars JSON Schema |
| Layout | `src/layout/` | ✅ Implemented | VirtualLayout, ActualLayout, projection, mutations, diff, engine |
| Registry | `src/registry/` | 🔲 Not started | WindowRegistry — OS sync, window state |
| Input | `src/input/` | 🔲 Not started | InputInterceptor — hotkeys, drag/resize |
| Persist | `src/persist/` | 🔲 Not started | Per-app learned state |
| IPC | `src/ipc/` | 🔲 Not started | SocketMessage types, named pipe transport |
| Animation | `src/animation/` | 🔲 Not started | Animation bridge |

---

## Implemented Modules — Detail

### Common (`src/common/`) — 312 lines

**`types.rs` (260 lines)** — Geometry primitives and platform-independent types:

```rust
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn is_empty(self) -> bool;
    pub fn right(self) -> i32;
    pub fn bottom(self) -> i32;
    pub fn overlaps(self, other: Rect) -> bool;
}

pub struct Size { pub w: i32, pub h: i32 }
pub struct Point { pub x: i32, pub y: i32 }

pub enum Direction { Left, Right, Up, Down }

pub struct WindowId(pub isize);  // opaque OS handle
```

**`error.rs` (52 lines)** — Project-wide error type:

```rust
pub enum StmError {
    Config(String),
    Layout(String),
    Io(std::io::Error),
}

pub type StmResult<T> = Result<T, StmError>;
```

---

### Config (`src/config/`) — 643 lines

**`types.rs` (460 lines)** — Top-level config with full serde + schemars support:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StmConfig {
    pub super_key: String,                    // default: "VK_F24"
    pub default_window_action: WindowAction,  // Tile | Float | Ignore
    pub default_column_width_eighths: u8,     // default: 4
    pub gaps: Gaps,                           // inner: 8, outer: 16
    pub hotkeys: Hotkeys,                     // 13 bindings
    pub window_rules: Vec<WindowRule>,
    pub animation: AnimationConfig,
    pub minimize_restore: MinimizeRestore,
}
```

Key sub-types:

| Type | Fields | Default |
|------|--------|---------|
| `Gaps` | `inner: i32`, `outer: i32` | 8, 16 |
| `Hotkeys` | 13 String bindings (focus_left, scroll_right, etc.) | Vim-style `Super+H/J/K/L` |
| `WindowRule` | `match_: MatchRule`, `action: WindowAction`, `initial_width_eighths`, `override_persist` | — |
| `MatchRule` | `exe`, `title`, `title_contains`, `title_regex`, `class`, `process_path` (all `Option<String>`) | — |
| `AnimationConfig` | `enabled: bool`, `duration_ms: u32`, `easing: String` | true, 180, "ease-out-expo" |
| `MinimizeRestore` | `strategy: MinimizeRestoreStrategy` | OriginalSlot |

The `match` field uses `#[serde(rename = "match")]` so the YAML key is `match` while the Rust field is `match_`.

**`schema.rs` (183 lines)** — JSON Schema generation for editor autocomplete:

```rust
pub fn generate_config_schema() -> StmResult<String>;
```

Note: Full YAML round-trip tested. Config from empty YAML `{}` produces all defaults.

---

### Layout (`src/layout/`) — 3001 lines

**`types.rs` (224 lines)** — Core layout types:

```rust
pub struct Column {
    pub width_eighths: WidthEighths,   // 1–8
    pub rows: Vec<WindowId>,
    pub row_ratios: Vec<f32>,          // sums to 1.0
}

pub struct VirtualLayout {
    pub columns: Vec<Column>,
    pub viewport_offset: i32,
}

pub struct ActualLayout {
    pub entries: Vec<ActualEntry>,
}

pub struct ActualEntry {
    pub window_id: WindowId,
    pub rect: Rect,
}

pub struct WindowMove {
    pub window_id: WindowId,
    pub from: Rect,
    pub to: Rect,
    pub hint: AnimationHint,
}

pub enum AnimationHint {
    Snap,         // <500px horizontal move
    Displaced,    // neighbor pushed aside
    ScrollEnter,  // entering viewport (>500px right)
    ScrollExit,   // leaving viewport (>500px left)
    Restore,      // crash/minimize restore — instant
}

pub struct LayoutDiff {
    pub virtual_layout: VirtualLayout,
    pub actual_layout: ActualLayout,
    pub moves: Vec<WindowMove>,
}
```

**`projection.rs` (507 lines)** — Virtual → Actual projection with geometric parking:

```rust
pub fn project(
    virtual_layout: &VirtualLayout,
    monitor: &MonitorInfo,
    gaps: &Gaps,
) -> ActualLayout;

pub fn canvas_width(layout: &VirtualLayout, monitor_width: i32, gaps: &Gaps) -> i32;
pub fn column_step_width(column: &Column, monitor_width: i32, inner_gap: i32) -> i32;
```

Parking model:
- Off-screen left: `monitor_left - col_width - outer_gap`
- Off-screen right: `monitor_right + outer_gap`

**`diff.rs` (549 lines)** — Layout diff with animation hint classification:

```rust
pub fn diff(prev: &ActualLayout, next: &ActualLayout) -> Vec<WindowMove>;
pub fn removed_windows(prev: &ActualLayout, next: &ActualLayout) -> Vec<WindowId>;
```

Classification: `|dx| > 500` → `ScrollEnter`/`ScrollExit`; otherwise `Snap`.

**`mutations.rs` (1099 lines)** — All pure mutation functions:

```rust
pub struct MutationConfig {
    pub monitor_width: i32,
    pub default_column_width_eighths: u8,
    pub gaps: Gaps,
}

// Scroll
pub fn scroll_left(layout: &VirtualLayout, config: &MutationConfig) -> Option<VirtualLayout>;
pub fn scroll_right(layout: &VirtualLayout, config: &MutationConfig) -> Option<VirtualLayout>;

// Focus (auto-scrolls into off-screen columns)
pub fn focus(layout: &VirtualLayout, focused: WindowId, direction: Direction, config: &MutationConfig) -> Option<FocusResult>;

// Swap
pub fn swap(layout: &VirtualLayout, focused: WindowId, direction: Direction) -> Option<VirtualLayout>;
pub fn swap_with_offscreen(layout: &VirtualLayout, focused: WindowId, direction: Direction, config: &MutationConfig) -> Option<VirtualLayout>;

// Resize
pub fn expand_column(layout: &VirtualLayout, focused: WindowId, direction: Direction) -> Option<VirtualLayout>;
pub fn shrink_column(layout: &VirtualLayout, focused: WindowId, direction: Direction) -> Option<VirtualLayout>;
pub fn set_column_width(layout: &VirtualLayout, focused: WindowId, eighths: u8, config: &MutationConfig) -> Option<VirtualLayout>;

// Merge
pub fn merge_column_left(layout: &VirtualLayout, focused: WindowId) -> Option<VirtualLayout>;
pub fn merge_column_right(layout: &VirtualLayout, focused: WindowId) -> Option<VirtualLayout>;

// Monocle
pub fn toggle_monocle(layout: &VirtualLayout, focused: WindowId, saved_width: Option<u8>) -> Option<(VirtualLayout, Option<u8>)>;

// Window lifecycle
pub fn add_window(layout: &VirtualLayout, window: WindowId, config: &MutationConfig) -> VirtualLayout;
pub fn add_window_to_column(layout: &VirtualLayout, col_idx: usize, window: WindowId) -> VirtualLayout;
pub fn remove_window(layout: &VirtualLayout, window: WindowId, config: &MutationConfig) -> VirtualLayout;
```

**`engine.rs` (622 lines)** — `LayoutEngine` orchestrator:

```rust
pub struct LayoutEngine { /* virtual_layout, focused, monitor, config, monocle_saved_width */ }

// Pipeline: apply mutation → project → diff → return LayoutDiff
```

Methods mirror all mutation operations, managing focus and monocle state internally.

---

## Key Design Decisions

1. **Container Model** — Column is a VerticalContainer (rows stacked with `row_ratios`). Layout is a HorizontalContainer (columns placed left-to-right). Horizontal resize → adjust `width_eighths`. Vertical resize → adjust `row_ratios`.

2. **Geometric Parking** — Off-screen columns parked exactly one column-width + padding beyond viewport edge. Deterministic, proportional, no magic numbers.

3. **Config-Driven Sizes** — All sizes from config: `default_column_width_eighths`, `gaps.inner`, `gaps.outer`. Layout engine receives via `MutationConfig` struct.

4. **Rect Frozen Contract** — `Rect { x: i32, y: i32, width: i32, height: i32 }` shape must not change — it's the cross-layer contract between layout and Win32.

5. **Pure Layout, Zero Win32** — `layout/` and `config/` have zero Win32 imports. All logic testable on any platform.

---

## Validation Results

| Check | Result |
|-------|--------|
| `cargo test` | 132/132 pass |
| `cargo clippy -- -D warnings` | Clean |
| `cargo fmt --check` | Clean |
| `cargo check --target x86_64-pc-windows-msvc` | Compiles |
| TestEngineer | ✅ Approved |
| CodeReviewer | ✅ Approved (after 3 fixes) |

Reviewer fixes applied:
1. `remove_window` had hardcoded config values → passed `MutationConfig`
2. Dead `ScrollExit` branch in `classify_hint` fixed
3. `.expect()` calls replaced with `StmResult` propagation

---

## What's Next (Phase 2)

| Module | Scope |
|--------|-------|
| `src/ipc/` | `SocketMessage`, `SocketResponse`, `SocketEvent` enums (OS-independent types) |
| `src/registry/` | `Window` struct, `WindowState` enum (needs Win32 hooks) |
| `src/main.rs` | Event loop connecting registry → layout → animation |
| Config loading | Actual YAML file loading (currently only in-memory for tests) |
