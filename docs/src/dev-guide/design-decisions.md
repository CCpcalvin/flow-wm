# Design Decisions

This chapter collects the major "why not X" trade-offs in one place. Each decision
is presented with the chosen approach, the main alternative considered, and the
rationale for the choice.

## Single Cargo Package (Not a Workspace)

`flow` is a single Cargo package with two binaries (`flowd`, `flow`)
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

## Loadout Window Identity: HWND-Exact (Not Fuzzy Matching)

The loadout feature (saving and restoring workspace arrangements across
daemon restarts) identifies windows by their **Win32 `HWND`**, stored directly
in the loadout file. On load, each saved slot is matched to the live window with
the identical HWND. If any saved HWND is not currently live, the **entire** load
aborts and the daemon falls back to its fresh init layout — a strict no-partial
guarantee.

The alternative considered was **fuzzy similarity matching**: score each candidate
live window on a combination of exe, class, title, and HWND, then assign the
best-scoring window to each slot. This was rejected for three reasons.

First, the loadout's job is **resilience, not desired-state declaration**. Its
purpose is to recover the exact arrangement that existed when the daemon was
stopped, restarted, or crashed — a window of seconds during which the target
applications keep running and their HWNDs are stable. HWND is therefore a
unique, exact, unambiguous key for the only case that actually matters. The
"save a canonical layout and restore it after a reboot" use case is better
served by classification config rules (declarative and durable) than by a
snapshot tied to specific window instances.

Second, fuzzy matching is **information-theoretically unsolvable for the very
windows that motivated the feature**. Windows Terminal instances share one
executable and one window class, and their titles (which encode the active
tab or working directory) are volatile. After a reboot, several identical
Terminal windows are genuinely indistinguishable — their original slot
assignments are lost forever. HWND is the only signal that could disambiguate
them, and across a reboot HWND is meaningless. Fuzzy matching would therefore
offer to "restore" a layout it cannot restore correctly, failing silently.

Third, fuzzy matching **destroys the no-partial guarantee and drags in a
layout-simplification algorithm**. "Best match" always returns a candidate, so
a slot whose window is genuinely absent would be silently force-paired with the
least-bad survivor, misplacing windows. Applying a partial layout well (skip the
missing slot, collapse the gap) requires recomputing column and row geometry —
real layout work for a case that, in the resilience window, essentially never
occurs, because target applications are independent processes that do not close
during a daemon restart. Aborting the whole load on any missing HWND avoids both
the silent-misplacement danger and the simplification algorithm entirely.

Stored alongside the HWND are `exe` and `title`, retained as **diagnostic-only**
fields: they make the loadout file self-describing at the moment a restore fails
(a missing window's identity is only known at save time, so it must be persisted
to be useful at load time). They are never consulted by the matcher. The window
`class` is dropped as low-value (opaque Win32 identifiers of no use to a human
reader).

This extends the `WindowId`-as-bridge decision above: `WindowId` bridges the
registry and layout engine *at runtime*; the stored HWND bridges the same
identity *across daemon restarts*.

## Drop the loadout staleness guard (No `saved_at` / `max_age_secs`)

Restore runs **no staleness check**: a loadout is applied whenever every saved
HWND is live, and the no-partial abort handles everything else. Earlier the
file carried a `saved_at` timestamp and auto-restore skipped snapshots older
than `max_age_secs` (default 60s). That guard was removed as **redundant with
no-partial** — the two are anti-correlated across the save→restore gap:

- **Short gap (< `max_age_secs`):** the staleness guard does not trip, so it
  contributes nothing, and HWND recycling within seconds is vanishingly
  unlikely.
- **Long gap (> `max_age_secs`):** an HWND could be recycled to a different
  window — but in that same gap other saved windows have closed too, so their
  HWNDs are simply absent and `resolve_hwnd` aborts the whole load on the
  first missing one, before any recycled handle can match. The collision is
  shadowed by an ordinary no-partial abort.

For a collision to slip through, every saved HWND would have to be live *and*
one recycled to a different window — a gap so long a 60s threshold is the wrong
proxy anyway. Worse, the threshold paid a **false-negative cost**: a valid
restore after a >60s gap (every window still open, every HWND stable) was
silently skipped. Removing the guard fixes that. The file format bumped
`version` 2 → 3 (old files are rejected by the existing version guard, logged
and harmless); `--no-restore` (the "don't even try" escape hatch) is unchanged.
Full rationale in `docs/adr/0006-drop-loadout-staleness-guard.md`.

## Restore Animates All, Switch Teleports Bystanders (Intentionally Divergent)

Workspace switching and loadout restore both end with the same picture: one
workspace visible at vertical offset `0`, every other non-empty workspace
parked one monitor-height (plus a `window_gap`) above or below it. They reach
that picture through **different animation policies**, and the two paths
deliberately do not share a code path beyond the pure merge+offset math.

- **Switch** (`switch_workspace_layout`) animates only the two *participants*
  (the source and destination workspaces, whose offset genuinely changes) and
  **teleports** side-changed bystanders into place via `teleport_workspaces`
  (an instant `SetWindowPos`, no tween).
- **Restore** ([`apply_loadout`](./loadout.md) → `build_seating_batches` →
  `animate_workspaces`) animates **every** non-empty workspace to its parking
  offset and performs **no** teleport step.

### The precondition that drives the divergence: are the bystanders pre-parked?

The teleport is only safe when the bystander workspaces are **already parked
off-screen**. That is the invariant a running daemon maintains between switches:
every non-active workspace sits one monitor-height above or below the viewport,
off the visible area. A switch flips which workspace is at offset `0`, and a
bystander whose parking *side* changed (e.g. workspaces 3–7 when switching
2 → 8: below active 2 but above active 8) is already off-screen on both ends.
Snapping it instantly is **invisible** — it moves from one off-screen slot to
another, so the user never sees the jump. Teleporting it avoids a long,
distracting slide across the full visible area.

At restore time that precondition does **not** hold. Restore runs after the
layout has just been applied but before any seating animation, so **nothing is
pre-parked** — every window is still at its on-screen init position (the
freshly-tiled layout the apply step just computed). A bystander workspace is
therefore sitting *on-screen*, not one monitor-height away. Teleporting it to
its parking offset would `SetWindowPos` an **on-screen** window straight
off-screen — a visible snap/jump the user sees. Animating it instead slides it
out smoothly.

So the same teleport that is correct for a switch is **wrong** for a restore,
and vice versa: a switch cannot animate every bystander (visual noise, eight
workspaces sliding at once on a 10-workspace switch), and a restore cannot
teleport (on-screen windows would visibly jump off-screen).

### The accepted cost: a full-span slide on an explicit load

Because restore animates rather than teleports, a workspace whose parking side
**flips** relative to the previously-visible workspace on an explicit
`flow loadout load` will animate across the full vertical span — it starts
on-screen and slides all the way to its off-screen parking offset. This is the
visible "streak" the switch path's teleport exists to suppress. It is accepted
because restore is a rare, explicit, whole-layout operation where a short
slide is expected and acceptable, and the alternative (teleport) is strictly
worse here (the on-screen snap).

Startup auto-restore (`try_restore_loadout_default`) has **no such streaks**,
because at startup no window begins parked either — every window starts at its
fresh-init position and animates once to its restored target. There is no
prior visible workspace to flip *from*, so no bystander makes a full-span
crossing; every workspace simply settles into place.

### Do not unify these paths

A future maintainer may be tempted to collapse `switch_workspace_layout` and
`build_seating_batches`/`animate_workspaces` into one shared
"seat the stack" routine. **Do not.** The two paths share the pure
merge-scrolling-and-floating + `workspace_y_offset` math (that sharing is
correct and already factored into `build_seating_batches`), but they must
remain split at the animation step:

- Making **restore teleport** reintroduces the on-screen snap described above.
- Making **switch animate every bystander** reintroduces the multi-workspace
  visual noise the teleport was added to kill.

The split is load-bearing and the divergence is the point, not an accident to
clean up. The pre-parked-vs-not precondition is the single fact that decides
which policy is correct, and it differs between the two call sites.
