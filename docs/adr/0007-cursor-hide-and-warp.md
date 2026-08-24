# Cursor hide + cursor warp: one invariant, three knobs

**Status:** Accepted

*(The spec (#35) drafted this as `0002-cursor-hide-and-warp.md`; that number
was taken by the edge-scroll config block before this feature landed, so it
ships here as ADR-0007.)*

Two cursor behaviors share one `[cursor]` config section:

- **Cursor warp** (`warp_on_focus`, default **true**): whenever window focus
  changes by any path, if the pointer lies outside the newly focused window's
  rect, FlowWM teleports it (`SetCursorPos`) to that window's center (clamped
  to the rect on the monitor work area). If the pointer is already inside, it
  is left untouched — byte-identical.
- **Cursor hide** (`hide_timeout_ms`, default **0 = disabled**; `poll_interval_ms`,
  default 125): after the timeout with no mouse activity, the system cursor
  becomes invisible. Any real mouse activity — pointer motion or a button
  press — makes it visible again and restarts the timer.

They interact by exactly one rule: **a warp counts as activity** — it un-hides
the cursor and restarts the inactivity timer.

## Context

Hyprland's cursor knobs (`cursor.inactive_timeout`, `no_warps`,
`persistent_warps`, `warp_on_change_workspace`, `hide_on_key_press`) are
independent, and their interactions produced a long tail of bugs: #2570
(keyboard focus reappears the cursor and it never hides again), #4197, #6594,
#4156, #1430 (flash-on-keyboard-focus, hide-under-new-window). This design is
the deliberate counter-proposal: **one warp rule, one interaction rule, three
config knobs total.**

## Decision

1. **Warp lives at the single focus convergence point.** Every focus path —
   keyboard dispatch, alt-tab/taskbar foreground changes, workspace switches —
   converges on `FlowWM::on_focus_changed` in `src/daemon/hooks.rs`. The warp
   check runs there, guarded by `prev_focus != Some(hwnd)` so duplicate
   foreground events never re-warp. There are no per-path warp flags, and none
   may be added.
2. **The warp target is the focused window's own rect** — floats use their
   float rect, tiles their projected rect — center-clamped into
   `rect ∩ work_area`. No animation; teleports are instant.
3. **Hide is a clock-injectable pure state machine** (`CursorHideScheduler`,
   `src/daemon/cursor_hide.rs`): the daemon feeds it activity samples and an
   injected `Instant`; it emits `Hide`/`Unhide`/`None` actions. The impure
   half (`set_system_cursor_transparent` / `restore_system_cursors`) lives in
   a thin `apply_cursor_hide_action` shim.
4. **Activity detection is main-loop polling** of pointer position + all mouse
   buttons (`GetAsyncKeyState`), no low-level hook, no new thread — the same
   deadline-driven pattern as edge-scroll. Polling runs **only** while
   `hide_timeout_ms > 0`; the default config keeps the daemon's
   zero-CPU-while-idle property. An interval larger than the timeout is
   clamped to the timeout (`effective_poll_interval`) rather than rejected —
   the spec puts no lower bound on `hide_timeout_ms`, and `flow.toml` is
   authoritative: legal config must not refuse daemon startup.
5. **Keyboard input is not an activity source in v1** — the daemon is
   keyboard-blind (hotkeys live in AutoHotkey). The pointer may hide while
   the user types; accepted, and clicks still land (button-down is activity).
6. **Hiding is suspended during any move-size gesture** (tile translate,
   resize): the gesture owns the pointer, and a hidden grab point is worse
   than a visible one.
7. **Layered restore for the blank-cursor hazard.** `SetSystemCursor` swaps
   session-global state; a daemon hard-killed mid-hide would leave it blank.
   Restore therefore runs on unhide, on clean daemon exit, in a panic hook
   (installed before anything else in `main`), and via the **daemonless**
   `flow cursor restore` CLI escape hatch (executes `SPI_SETCURSORS` locally,
   never contacts the daemon — works when flowd is dead). The escape hatch
   (#36) shipped *before* the swapping mechanism (#37) for exactly this
   reason. Reboot or any app invoking the system restore-cursors call also
   recovers.

## Consequences

- Cursor-swap wrappers are deliberately **not** exercised by parallel
  automated tests (session-global state would blank a developer's cursor
  mid-run). Coverage is the pure state machine's unit tests, real-daemon
  integration tests that assert via the daemon's transition log, and the
  harmless local `flow cursor restore` smoke test.
- Cursor integration tests must promote the isolated test desktop to the
  session **input desktop** (`SwitchDesktop`, RAII-restored) because cursor
  APIs are gated on it — see `TestDesktop::make_input` and the
  `INPUT_DESKTOP_LOCK` serialization in `tests/cli/test_desktop.rs`. This
  briefly switches the session's visible desktop and moves the real pointer;
  it is inherent to testing cursor behavior against a real daemon.
- When a future focus-follows-mouse lands (ADR-0001's hover subsystem), warp's
  center-clamp and hover's dwell logic must compose; likewise keyboard
  activity can later join the same poll as an activity source without new
  plumbing.
- `persistent_warps` (per-window pointer offsets) and hide-on-keystroke are
  explicitly out of scope for v1.
