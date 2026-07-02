# Design Decisions

This chapter collects the major "why not X" trade-offs in one place. Each decision
is presented with the chosen approach, the main alternative considered, and the
rationale for the choice.

## Single Cargo Package (Not a Workspace)

`flow` is a single Cargo package with three binaries (`flowd`, `flow`, `flow-watchdog`)
sharing one library crate (`src/lib.rs`). Rust supports this layout natively — one
package can contain a library crate, a default binary, and additional binaries.

The alternative was a multi-crate workspace from the start. That was rejected
because the internal subsystem boundaries are still evolving. Module boundaries
within a single package are sufficient for the current scale, and keeping
everything in one package avoids Cargo configuration overhead and makes refactoring
across module boundaries trivial.

Extraction to a separate crate will happen only when a concrete reason appears:
the code is reusable outside `flow`, compile times become a real bottleneck, or
the API has stabilized and deserves a stronger boundary. Likely future
extraction candidates are `src/animation` (if the animation system becomes
reusable elsewhere) and `src/ipc` (if external tools need a standalone client
crate). Internal modules like `registry`, `layout`, and `workspace` should stay
where they are until a strong reason to move them emerges.

## Pixel Widths (Not Proportional Eighths)

Column widths in the virtual layout are stored as absolute pixel values
(`width_px: i32`), not as proportional units (e.g. eighths of a base column
width).

An earlier revision used eighths to stay resolution-independent. This caused a
gap-loss bug: `expand_column` computed the correct pixel target
(`column_width + window_gap`), then `pixels_to_eighths` re-quantized onto the
base grid, discarding the gap because it was smaller than one eighth and rounded
away. Each expand step grew by exactly `column_width` instead of
`column_width + window_gap`.

Pixel widths make the gap observable at every step and fix the bug. The cost is
dependence on the configured column width and monitor resolution — accepted because
`window_gap` is already pixel-based everywhere else and resolution independence
was never a real requirement (the target is Windows desktop, not cross-platform).

## No `Arc<Mutex>` (Single-Thread Ownership)

The daemon uses single-thread ownership: `FlowWM` lives entirely on
the IPC thread, and all subsystem methods take `&mut self`. There is no
`Arc<Mutex<T>>` anywhere in the daemon.

The previous architecture used `Arc<Mutex<WindowRegistry>>` because hook events
and IPC commands were consumed by different parts of the code with no single
coordination point. The refactor to a single orchestrator eliminated that need.
The borrow checker now enforces exclusive access at compile time, which is
strictly safer than `Mutex` (runtime-only enforcement, potential deadlocks). The
hook thread communicates exclusively through an `mpsc` channel and never touches
daemon state.

See [Threading Model](./threading-model.md) for the full model.

## TOML Config (Not YAML, JSON, or Lua)

Configuration uses TOML. The alternatives were YAML (indentation-sensitive, complex
spec), JSON (no comments, verbose), and Lua (fully programmable but requires a
runtime and steep learning curve).

TOML's clean syntax, comment support, and good fit for flat/nested config made it
the clear choice. Lua remains a future option if users need conditional logic
(e.g. per-monitor layouts), but TOML covers all current use cases.

The config system uses a two-layer merge model: compiled-in `Default` impls
provide the base, and the user's `flow.toml` overlays on top. Serde's
`#[serde(default)]` at the container level means a user's config can be partial,
empty, or nested-partial — serde fills the gaps from the `Default` impl.

## Config Defaults: Code Is the Single Source of Truth

Default values for every config field live in the `Default` impls of each config
struct in [`src/config/types.rs`](../../src/config/types.rs). Each struct carries
`#[serde(default)]` at the container level, so a user's `flow.toml` may be partial
or empty.

`default-config.toml` in the repo root is a hand-written **example** file, copied
to users by `flow config init`. It is **not** read at runtime — the compiled
defaults are authoritative. When a developer adds or changes a config field,
they must update both the `Default` impl and `default-config.toml`. A test
(`default_config_toml_matches_compiled_defaults`) enforces they stay in sync.

This avoids the fragility of "file A is the source of truth except when it's
missing" and the complexity of a two-layer TOML merge with a shipped file at
runtime.

## Separate `WindowRegistry` and `ScrollingSpace`

The window registry (`src/registry`) and the tiling engine
(`ScrollingSpace` in `src/workspace`) are separate components with a clear
purity boundary.

The registry owns all window metadata: HWND-to-`WindowId` mapping, titles,
classes, classification state (tile/float/ignore), and invisible bounds. It
bridges the layout engine to Win32 — it is the only place that holds HWND
references.

`ScrollingSpace` owns the layout math: virtual layout, focus state, column
widths, viewport offset. It operates exclusively on `WindowId` values and never
sees HWNDs. The pure layout computation in `src/layout/` is even more isolated —
it has zero Win32 dependencies.

This separation means the layout math is fully unit-testable on any platform and
can be reasoned about without considering Win32 quirks. The daemon is the only
code that shuttles data between the two subsystems.

## `SetWindowPos` Over `DeferWindowPos`

Window positioning uses `SetWindowPos` rather than the batched `DeferWindowPos`
API.

`DeferWindowPos` batches multiple position changes and triggers a single repaint
at the end — useful for UI frameworks that own all the windows they move. In
`flow`'s case, not all windows are deferrable (some apps reject deferred
positioning), and each application owns its own rendering pipeline. `SetWindowPos`
applies the position change immediately, which is what the animation system
expects when tweening individual window rects frame by frame.

## `WindowId` as the Platform-Independent Bridge Type

`WindowId` (currently `pub struct WindowId(pub isize)`, wrapping the raw HWND
value) is the bridge type between the registry and the layout engine.

The layout engine only ever sees `WindowId` — it never knows about HWNDs. This
keeps the layout math platform-independent and unit-testable. The registry is the
only component that holds the HWND-to-`WindowId` mapping and performs Win32 calls.

The `isize` wrapping exists because `HWND` is `!Send` in windows-rs (it wraps a
raw pointer), but `WindowId` must be `Send` to cross thread boundaries (e.g. the
hook callback sends `HookEvent { hwnd: isize }` through the `mpsc` channel). The
raw integer value is just a kernel handle — it is safe to share across threads.

## Keybindings Removed, Delegated to External Tools

Keybindings were removed from `flow`. The daemon accepts IPC commands over a named
pipe, and users are expected to configure external tools (AutoHotkey, PowerToys,
etc.) to send those commands.

The original design had a built-in `InputInterceptor` with Super-key capture and
configurable hotkey bindings. This was removed because:

- It required intercepting global input, which conflicts with other tools the
  user may already have.
- A dedicated keybinding tool gives users more flexibility (per-application
  rules, macros, chords) than any tiling manager's built-in binding system.
- It keeps `flow` focused on what it does well: window management and layout.

The IPC protocol surface (focus, swap, scroll, resize, etc.) is fully defined and
stable — any external tool that can write JSON to a named pipe can drive `flow`.
