# FlowWM

A scrolling, infinite-horizontal-canvas tiling window manager for Windows. Windows live as columns on a canvas wider than the monitor; the viewport slides left/right instead of fitting a fixed grid. The daemon (`flowd`) owns all state; the CLI (`flow`) talks to it over named-pipe IPC. Window management sits *on top of* the Win32 window model — flow-wm does not own the OS's focus/raise primitives, it orchestrates them.

## Language

### Window focus & stacking

**Focus**:
The window that receives keyboard input and is shown with the active border. Moves freely, including by focus-follows-mouse. Distinct from *raise* — focusing a window must not change what is visually on top.
_Avoid_: active window, selected window, foreground (when meaning input target)

**Raise** (a.k.a. **Z-order**, **stacking position**):
Which window is visually on top. Must obey the *stacking invariant*. Changes rarely and deliberately.
_Avoid_: bring-to-front, foreground (when meaning visual position)

**Foreground**:
The Win32 OS concept: the window holding active state (`WM_ACTIVATE`), input-queue ownership, and taskbar highlight. Windows *bundles* foreground with raise in `SetForegroundWindow` — this conflation is the root cause flow-wm must work around. When we say "foreground" we mean the OS's notion specifically.
_Avoid_: using "foreground" loosely for either focus or raise.

### Stacking layers

**Tile layer**:
The bottom stacking layer. Holds tiled windows, laid out by the scrolling tiling engine. Tiles never overlap each other, so their mutual order is irrelevant.

**Float layer**:
The top stacking layer. Holds floating windows, ordered by `FloatingSpace` (later = on top). A float is always above every tile regardless of focus.

**Stacking invariant**:
The rule the daemon enforces on every foreground change: every float is always above every tile in Win32 Z-order, no matter which window holds focus or how focus moved there (FFM, keyboard nav, alt-tab, taskbar).

## Cursor behavior

**Focus-follows-mouse**:
Hovering the cursor over a window moves OS focus to it (`SetForegroundWindow`), no click required.
_Avoid_: hover-focus, sloppy-focus, follow_mouse (ambiguous — also covers edge-scroll-follows-mouse)

**Edge-scroll-follows-mouse**:
A cursor parked at the left/right screen edge scrolls the viewport — one immediate column-scroll, a longer dwell, then continuous auto-repeat — independent of any drag.
_Avoid_: mouse edge-scroll, follow_mouse (ambiguous)

## Tile resize

**Translate** (a.k.a. **tile drag**):
A move-size gesture that reorders a tile — grabbing the title bar to drop it in a new column/row slot. The committed layout is frozen for the duration; non-committing previews move the other windows; placement commits once, on release.
_Avoid_: move

**Resize**:
A move-size gesture that changes a tile's geometry — grabbing an edge or corner. Win32 fires the same `MoveSizeStart`/`End` events for translate and resize and carries no sizing edge, so the two are told apart at start by where the cursor sits on the window rect.
_Avoid_: size

**Anchor edge**:
In a resize, the edge opposite the grip. It stays fixed; all growth extends from the grip side.

**Boundary-move**:
The resize invariant — exactly one column/row boundary shifts. The growing side absorbs `+Δ`; the shrinking neighbor absorbs `−Δ` down to its minimum. The tmux model, not "translate the neighbors."

**Grow**:
Horizontal resize past the neighbor's minimum: the clamped neighbor rides at min width and the canvas extends, because the horizontal axis is unbounded. Not available vertically.

**Elastic pin**:
The behavior at a resize ceiling. Win32 owns the dragged window's geometry during a native move-size, so the edge cannot be restrained live — it overshoots during the drag and snaps back to the ceiling on release. Vertical reaches this ceiling as soon as the neighbors hit minimum; horizontal reaches it at the absolute max width. Never a hard stop.
_Avoid_: hard pin

**Teleport**:
Placing a window at its target rect instantly, bypassing the animator. Used wherever tweening would be wrong — bystander workspaces during a switch, the initial snap, and every window moved during a resize drag. Contrast **animate** (tweened), for autonomous motion.
_Avoid_: snap (ambiguous with snap-to-grid)

## Event publishing

**HookEvent**:
A raw OS signal captured by the hook thread (`SystemForeground`, `ObjectCreate`, `MoveSizeStart`, etc.). Internal noise; never leaves the daemon.
_Avoid_: event (when meaning the raw signal)

**Event**:
A published semantic state change the daemon announces to subscribers — the only thing that travels on the event stream. Derived from `HookEvent`s and command outcomes, never a raw hook signal itself.
_Avoid_: notification, message (ambiguous with IPC command/response messages)

**Subscriber**:
An external application (e.g. a status bar) that owns its own named pipe, registers it with the daemon, and receives `Event`s as newline-delimited JSON. The daemon writes; the subscriber reads.
_Avoid_: listener, client (ambiguous with the command-pipe `flow` client)

**Workspace switch**:
The active (visible) workspace on a monitor changes — what the user is looking at.
_Avoid_: workspace change (ambiguous with contents mutation)

**Workspace contents mutation**:
A workspace's window-set changes (a window moved in or out) while the active workspace stays put. `move-to-workspace` performs *both* a contents mutation and a switch, because the camera follows the moved window.
_Avoid_: workspace change (ambiguous with switch)
