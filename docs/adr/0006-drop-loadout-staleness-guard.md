# Drop the loadout staleness guard (no `saved_at` / `max_age_secs` / `force`)

**Status:** Accepted

The loadout file no longer carries a `saved_at` timestamp, and auto-restore no
longer skips snapshots based on age. The `max_age_secs` config knob, the
`force` IPC flag, and the `is_stale` helper are removed. The loadout file
format bumps `version` 2 → 3 (the existing version guard rejects v2 files on
load, logged and harmless). Restore now succeeds whenever every saved `HWND` is
live and aborts (no-partial) otherwise — the timestamp plays no role.

## Context

Until this change, `FlowWM::apply_loadout` ran a staleness guard before the
no-partial resolve: a snapshot whose `saved_at` was older than
`config.loadout.max_age_secs` (default 60s) was a silent skip, unless the
caller passed `force: true` (manual `flow loadout load`). The intent was a
safety net for the crash / hard-kill case, where save-on-stop never ran and
`loadout.json` could be arbitrarily old.

The matcher keys **only** on `HWND` (`WindowRef.hwnd`); `exe`/`title` are
diagnostic-only. A Win32 `HWND` *can* be recycled after its window is
destroyed (an index + 16-bit uniquifier; reuse is possible but rare), so a
stale loadout could, in principle, match a *different* live window behind a
recycled handle — silent layout corruption the no-partial guarantee cannot
see (the recycled handle *is* live).

## Decision

Remove the staleness guard entirely. Rely on the no-partial guarantee alone.

### Why it is safe: staleness and no-partial are anti-correlated

The two guards disagree only across the **gap length** axis, and on that axis
they are *anti-correlated* — there is no regime where staleness catches a
correctness bug that no-partial misses:

- **Short gap (< `max_age_secs`):** staleness does not trip, so it contributes
  nothing. `HWND` recycling within seconds is vanishingly unlikely, so
  collision is not a concern here either.
- **Long gap (> `max_age_secs`):** collision becomes *possible* — but in that
  same long gap, other saved windows have almost certainly closed too, so
  their `HWND`s are simply absent from the live set and `resolve_hwnd`
  aborts the whole load on the *first* missing one, before any recycled
  handle gets a chance to match. The collision is shadowed by an ordinary
  no-partial abort.

For a collision to slip through no-partial, *every* saved `HWND` would have
to still be live **and** at least one recycled to a different window — the
entire saved window-set destroyed *and* reconstituted as a live set that
happens to collide. That requires a gap so long that a 60s threshold is a
laughable proxy for it. The timestamp was guarding the wrong thing.

### Bonus: staleness had a false-negative cost

`max_age_secs = 60` also skipped *valid* restores: `flow stop`, walk away for
three minutes with every window still open and every `HWND` stable, then
`flow start` → silently no restore. Removing staleness *fixes* that case — the
user gets their layout back.

## Considered Options

- **Keep staleness as-is.** Rejected: it adds ~zero correctness value over
  no-partial (anti-correlation) and pays a false-negative cost on valid
  >60s restores. It also carried real surface area — `saved_at`, `is_stale`,
  `max_age_secs` config, the `force` flag plumbed through IPC +
  `apply_loadout` + the CLI, and ~7 tests.
- **Repurpose the stored `exe` as a resolve-time sanity check** (treat a slot
  as a miss when `hwnd` matches but `exe` differs), closing the collision gap
  *directly* and then dropping the timestamp. Rejected for now: it would
  reintroduce a form of the fuzzy matching the HWND-exact decision
  (see `docs/src/dev-guide/design-decisions.md`, "Loadout Window Identity:
  HWND-Exact (Not Fuzzy Matching)") deliberately rejected, and the collision
  case it would catch is already shadowed by no-partial. Noted as a future
  option if a real collision is ever observed.
- **Drop the check but keep `saved_at` in the file** (`#[serde(default)]`,
  stop writing it). Rejected: leaves dead weight in the schema, contradicting
  the goal of simplifying. The version bump is the honest signal.

## Consequences

- **Crash-case logging is noisier but more informative.** Previously: one
  clean `"skipping stale loadout from <ts>"`. Now: auto-restore attempts the
  (old) loadout and no-partial aborts, logging each missing window
  (`"window not currently open: firefox.exe … aborting (no-partial)"`). Same
  outcome (no restore); the new log names *exactly which* windows are gone.
- **One-time rejection of existing `loadout.json`.** Users with a v2 file on
  disk get it rejected once on next start (logged skip, never blocks
  startup) — the same graceful path legacy files already take.
- **`--no-restore` is unchanged.** It was always the orthogonal "don't even
  try" escape hatch, not a staleness knob.
- **The `chrono` crate stays.** `src/logging.rs` uses `chrono::Local` for
  daily log-file naming; dropping the loadout timestamp does not remove the
  dependency.
