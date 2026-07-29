# Loadout Save/Restore

The loadout feature saves a user's tiling arrangement to a file and restores it
across a daemon restart — so a `flow stop` / `flow start` (to apply a config
change), a crash, or a self-update does not destroy their workspace.

This page covers the feature's architecture: how window identity survives a
restart, the strict no-partial restore guarantee, and the save/load lifecycle.
The *rationale* for matching on `HWND` (and the rejected fuzzy-matching
alternative) lives in the [Design Decisions](./design-decisions.md) record,
"Loadout Window Identity: HWND-Exact (Not Fuzzy Matching)".

## Window identity across a restart

A window's identity across a daemon restart is its **Win32 `HWND`**, stored
directly in the loadout file (`WindowRef.hwnd`). The target applications keep
running independently of the daemon, so their handles are stable and unique
across the restart — `HWND` is an exact, unambiguous key for the only window of
time the matcher must handle (the seconds between `stop` and `start`).

Stored alongside the `HWND` are `exe` and `title`, retained as
**diagnostic-only** fields: they are never read by the matcher, but make a
failed restore self-describing (a missing window's identity is only known at
save time, so it must be persisted to name the window at load time). The
window `class` is dropped as low diagnostic value.

This extends the `WindowId`-as-bridge decision: `WindowId` bridges the
registry and the layout engine *at runtime*; the stored `HWND` bridges the same
identity *across daemon restarts*.

## The no-partial guarantee

Restore either fully succeeds or cleanly falls back — never partially applies.
On load, each saved slot is matched to the live window with the identical
`HWND`. If **any** saved `HWND` is not currently live, the **entire** load
aborts and the just-built init layout (the daemon's normal tiling of whatever
windows are currently open) is left untouched. There is no per-slot skip, no
gap-collapsing, and no layout-simplification algorithm.

The abort is the single, explicit failure mode. It is logged with the missing
window's diagnostic `exe`/`title` so a failed restore is diagnosable without a
debugger, and surfaced as an error to a manual `flow loadout load`.

## File format

The loadout file (`loadout.json` in the config directory by default) is a
versioned JSON document:

```json
{
  "version": 2,
  "saved_at": "2026-07-29T12:00:00Z",
  "workspaces": [
    {
      "workspace_id": 0,
      "active": true,
      "scrolling": {
        "viewport_offset": 0,
        "focus": { "hwnd": 2885958, "exe": "code.exe", "title": "main.rs" },
        "columns": [
          { "width_px": 960, "rows": [
            { "window": { "hwnd": 2885958, "exe": "code.exe", "title": "main.rs" },
              "height_px": 600 }
          ] }
        ]
      },
      "floating": []
    }
  ]
}
```

- `version` — `LoadoutFile::CURRENT_VERSION` (`2`). The writer always emits
  this; the loader rejects any other value. A legacy pre-`HWND` file cannot be
  migrated (`HWND` cannot be synthesized), so it is skipped with a logged
  reason rather than silently misread.
- `saved_at` — RFC3339 timestamp used by the staleness guard.
- `workspaces` — one snapshot per workspace: its tiling columns/rows (each row
  a `WindowRef` + height), the viewport scroll offset, the focused window, and
  any floating windows with their screen rectangles.

`exe`/`title` appear in every `WindowRef` purely so the file is human-readable
when a restore fails.

## Lifecycle

```mermaid
sequenceDiagram
    participant U as User
    participant D as flowd
    participant F as loadout.json

    U->>D: flow stop
    D->>F: save current arrangement (save-on-stop)
    D->>D: shutdown

    Note over D: (windows keep running; HWNDs stable)

    U->>D: flow start
    D->>D: init — scan existing windows, fresh-tile them
    D->>F: read loadout (auto-restore, honors max_age_secs)
    alt every saved HWND is live
        D->>D: apply saved layout (set_layout + floats)
        D->>D: append leftover windows as columns
    else any saved HWND missing
        D->>D: abort — keep fresh init layout
    end
```

### Save

`flow loadout save` (manual) and the save-on-stop hook share one code path
(`FlowWM::dispatch_loadout_save` / `try_save_loadout_default`). The save walks
every monitor → workspace, joins the virtual layout (columns, rows, viewport
offset, focus) with per-window registry metadata, swaps `WindowId` for
`WindowRef`, and writes pretty-printed JSON. Ignored windows (maximized,
fullscreen, or rule-ignored) are excluded from both save and restore.

### Restore

Auto-restore runs **in-process at daemon boot** (`FlowWM::try_restore_loadout_default`),
right after init finishes tiling the currently-open windows and before the IPC
event loop starts. Performing restore here — rather than via an IPC round-trip
from the CLI — sidesteps the startup named-pipe race entirely.

The load path (`FlowWM::apply_loadout`) is shared by auto-restore and manual
`flow loadout load`, so both behave identically:

1. Parse; reject on a non-current `version`.
2. Staleness guard — a snapshot older than `max_age_secs` is a silent skip
   (unless `force`). A future timestamp (clock skew) is treated as fresh so a
   slightly-ahead clock does not silently drop the loadout.
3. Collect live managed windows' `HWND`s (skip `Ignored`).
4. **Resolve** every saved slot's `HWND` against that set — no daemon mutation.
   The first missing `HWND` aborts the whole load.
5. **Apply** per-workspace: `set_layout` replaces the tiling canvas, floating
   windows are placed at their saved rectangles, and registry state is synced.
   Focus is restored by its saved `HWND` directly.
6. **Leftover** windows — open now but not referenced by the loadout — are
   appended as new columns on the active workspace, so no open window
   disappears.

### Opt-outs and overrides

- `--no-restore` — skip auto-restore at startup, starting completely fresh.
- `flow loadout load` — manual restore; always forces past the staleness guard
  (`force: true`) because the user explicitly asked for the arrangement.

## What the loadout is (and is not)

The loadout's job is **resilience, not desired-state declaration**. It recovers
the exact arrangement that existed when the daemon was stopped — a window of
seconds during which the target applications keep running and their `HWND`s are
stable. The "save a canonical layout and restore it after a reboot" use case is
better served by classification config rules (declarative and durable) than by
a snapshot tied to specific window instances: across a reboot, `HWND` is
meaningless and identical windows (e.g. several Windows Terminals) are
information-theoretically indistinguishable, so the arrangement cannot be
recovered correctly. See the design-decision record for the full rationale.
