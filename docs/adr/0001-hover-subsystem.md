# Hover subsystem: polling, movement-gated dwell, and unified edge-scroll

The hover subsystem (focus-follows-mouse and edge-hover-scroll) obtains cursor position by **polling `GetCursorPos`** on the main loop at a configurable interval rather than installing a `WH_MOUSE_LL` low-level mouse hook, because a low-level hook fires at the mouse hardware reporting rate (up to 8000 Hz) and we want resource use independent of the user's hardware. Focus-follows-mouse uses a **movement-gated dwell** — the dwell arms only on an observed cursor-position change over an eligible window, and is cancelled by any foreground event — which defeats the classic alt-tab steal-back with no keyboard detection, no cooldown timer, and no forced cursor movement. Edge-scroll is **unified**: one auto-repeat scheduler instance held on the orchestrator, fed by the drag's reactive move handler during a drag and by the hover poll otherwise.

## Considered Options

- **Cursor source — `WH_MOUSE_LL` hook vs polling.** Rejected the hook: it fires per hardware mouse event (up to 8000 Hz) and needs its own thread, exactly the cost we want to avoid. Polling lets us choose the rate.
- **Alt-tab handling — keyboard detection / cooldown / forced cursor move vs movement-gate.** Rejected all three: keyboard detection is heuristic, a cooldown timer is imprecise, and forcing the cursor to move is hostile UX. The movement-gate (arm only on observed motion; cancel on foreground change) handles it structurally.
- **Edge-scroll — poll drives both vs unified scheduler with split triggers.** Rejected poll-drives-the-drag: the drag already reads the cursor reactively per move event (the drop-zone preview requires it), the band check rides that read for free, and the screen-edge band geometrically overlaps the first/last column's insert band — so scroll-vs-insert priority must be resolved atomically from one cursor read. The drag feed therefore stays reactive and co-located with drop-zone resolution; only the hover feed uses the poll.

## Consequences

- Enabling either behavior (both ship **on by default**) means the daemon polls forever at the configured interval, so the daemon's long-standing "zero CPU while idle" property no longer holds by default. This is a deliberate trade-off; flipping both flags to default-off is a one-line, low-risk reversal if it proves too costly.
- The poll interval, the focus dwell, and the edge dwell are the three knobs that govern the cost/responsiveness trade-off and are all user-configurable.
