# FFM targets only the active workspace, and hover is suspended during any animation

Focus-follows-mouse (FFM) never focuses a window that is not on the **active** workspace, and the entire hover subsystem (FFM + edge-hover-scroll) is suspended while the animator is tweening. The two are layered: eligibility is an always-on invariant that fixes the catastrophic case even with animation disabled; animation suppression is a smoothness layer over every IPC/hook-driven scroll. IPC `Busy` is **not** extended to animation — it stays keyed to `drag_state` alone.

## Context

The bug (see `debug.log` on this branch): switching workspace while the mouse is moving makes the viewport oscillate back to the old workspace. `SwitchWorkspace` flips the active workspace to the destination synchronously at dispatch (`switch_workspace_layout`), then animates the slide for ~240ms. During that slide the old-workspace windows are still on screen and the moving cursor is sitting over one. FFM has no concept of workspace membership, so that foreign-workspace window is an eligible target: the movement-gate re-arms the dwell (~25ms), it fires `SetForegroundWindow` on the old window, `on_focus_changed` sees a focus change to a different workspace and re-runs `switch_workspace_layout` scrolling back — and the two paths ping-pong until `reconcile_foreground` forces a landing.

The same root cause produces a milder symptom on same-workspace scrolls (focus-left/right): FTM racing the camera feels laggy, just not catastrophic, because a same-workspace refocus does not re-trigger a workspace switch.

The movement-gate's "alt-tab respect" (`HoverController::on_foreground_change` cancels the dwell) was documented as defeating steal-back "with no keyboard detection or cooldown." That claim is **falsified**: the cancel only holds while the cursor is *still*. A cursor that keeps moving re-arms immediately, which is exactly the bug. The docs are revised to point here.

## Decision

Two layers, serving different purposes (not redundant):

1. **Eligibility invariant** — `hover_ffm_target` rejects any window not on the active workspace. This is an invariant, not a timing trick: a foreign-workspace window is never an FFM target, period. It holds even when animation is disabled in config (instant switch → old window gone immediately → cursor over a destination window → no fight), and it covers any future case where a foreign-workspace window is briefly on screen.

2. **Animation suppression** — the existing `interaction_suppresses_hover` seam is extended so hover is suspended while `Animator::is_animating()`. Implemented as the drag pattern copied exactly:
   - **Clean-on-engage:** when an animated batch is submitted (a tween-building call to `animate_layout` / `animate_workspaces`), reset the armed hover state — clear `focus_dwell_deadline` / `edge_dwell_deadline` and `HoverController::reset()` — mirroring `on_drag_start`.
   - **Skip-on-engage:** the three hover entry points (`poll_hover`, `maybe_fire_focus_dwell`, `maybe_fire_edge_dwell`) OR `is_animating()` into the suppression predicate.

   Clean-on-engage is the load-bearing half. Skip-on-engage alone (an early `return`) only stops *new* arms and firing *while* animating; it never clears a dwell armed before the keypress, so that orphaned timer fires the instant the animation ends. The clean must be an action taken when suppression engages, not merely a guard.

## Considered Options

- **Temporal-only (suspend FFM during animation, no eligibility).** Rejected as the primary fix. It attacks a symptom (FFM fired during an animation) rather than the category error (FFM targeting a foreign-workspace window). It leaves no floor when animation is disabled, and no protection when a foreign-workspace window is briefly on screen for a non-animation reason. Kept as the *second* layer for smoothness, not the first.
- **Single unified `InteractionState` enum (`Idle | Dragging(DragMode) | Animating`).** Rejected. `drag_state` is a rich, main-thread gesture FSM (hwnds, classification, edge-scroll state); animation is a binary flag owned by a *worker thread*. Mashing them into one type fights both designs. The single-source-of-truth goal is met by a **derived predicate** — `hover_suppressed() = interaction_suppresses_hover(drag_state) || animator.is_animating()` — consulted by every guard site, without welding two unrelated state machines together.
- **Extend IPC `Busy` to animation** ("only accept IPC when idle"). Rejected. The bug is FFM-vs-switch, not IPC-vs-anything. Two IPC focus commands are *cooperative* — they share intent — and the animator is deliberately built to retarget mid-flight from the current interpolated position so rapid `focus-left, focus-left` chains smoothly. Gating IPC on `is_animating()` would return `Busy` on the second keypress and break that chaining, trading a real feature for protection against a fight that does not occur. `Busy` stays keyed to `drag_state` (a destructive gesture) as today.
- **Animation-end callback from the animator worker.** Deferred. Cleanest structurally, but poll-reading `is_animating()` (an existing, accurate, currently test-only `AtomicBool`) is good enough; no new cross-thread callback channel is warranted yet.

## Consequences

- **FFM is unresponsive for ~`animation.duration_ms` after every focus/center/switch.** Accepted tradeoff — the user reported same-workspace scroll lag and explicitly chose to suppress FFM across all animations rather than only cross-workspace switches.
- **Edge-hover-scroll is suspended alongside FFM** (they share the `interaction_suppresses_hover` gate). Accepted; an edge-scroll compounding an in-flight camera scroll is undesirable anyway.
- **Clean-on-engage belongs at the animate-submit site**, not scattered across dispatch handlers, so every animation (IPC or hook-driven) is uniform. For commands that already call `SetForegroundWindow`, `on_hover_foreground_change` cancels the dwell today, so the clean is partly defense-in-depth there; it is *load-bearing* for viewport-only commands (ScrollLeft/Right, Center) that move windows without an OS foreground change.
- **The movement-gate "no steal-back" doc claim is falsified** and revised in `hover/controller.rs` and `daemon/hover.rs` to point here.
- **Eligibility needs window→workspace resolution in the poll path** (`workspace_containing_window` already exists). Implementer to confirm the lookup cost is acceptable per poll.
