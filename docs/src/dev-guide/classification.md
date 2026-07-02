# Classification & Learned Rules

flow classifies every new window as **tiling**, **floating**, or **ignored**. This
chapter covers *why* the default is float, *how* flow learns from the user's
explicit `set-window` decisions, and *where* that learned state lives on disk.
For the rule-matching algorithm itself — the Win32 pre-filters, the per-layer
first-match-wins evaluation, and the regex/field semantics — see
[Window Registry](./window-registry.md).

## The Whitelist Model: Float by Default

Most tiling window managers take a **blacklist** approach: every window tiles
unless the user adds a rule to float it. flow inverts this. **Every window
floats unless the user promotes it to tiling** — either with an explicit rule
in `flow-rules.toml` or by running `flow dispatch set-window tile` on it once.

This whitelist model is encoded in a single value: the `default_action` field
of `WindowRulesConfig` defaults to `Float` (see
[`src/config/types.rs`](../../src/config/types.rs)). When no rule at any layer
matches a window, the pipeline returns `Float` rather than `Tile`. The bundled
`default-flow-rules.toml` carries `default_action = "float"` to match.

The rationale: on a scrolling-canvas tiler like flow, most application windows
(notably transients, dialogs, and small utilities) look wrong in a tile column.
Tiling is most useful for a small set of "main" apps the user actively works
in — and the user is the best judge of which apps those are.

## The Four-Layer Priority Chain

Every classification consults four layers in priority order. The first layer to
match wins; layers below it are not consulted:

```mermaid
flowchart TB
    W["New window<br/>(exe, title, class)"] --> L1{"User rules<br/>(flow-rules.toml)?"}
    L1 -- match --> R1["use user rule"]
    L1 -- no match --> L2{"Learned rules<br/>(history-flow-rules.toml)?"}
    L2 -- match --> R2["use learned rule"]
    L2 -- no match --> L3{"Default rules<br/>(embedded at compile time)?"}
    L3 -- match --> R3["use default rule"]
    L3 -- no match --> L4["default_action<br/>= Float"]
    R1 --> OUT["WindowState"]
    R2 --> OUT
    R3 --> OUT
    L4 --> OUT
```

The four layers, from highest to lowest priority:

1. **User rules** — hand-written in `flow-rules.toml`. Always win. The escape
   hatch for "I want this app to behave this way regardless of what flow
   learned."
2. **Learned rules** — machine-written to `history-flow-rules.toml`. See
   [Learned Rules](#learned-rules) below.
3. **Default rules** — embedded into the binary at compile time from
   [`default-flow-rules.toml`](../../default-flow-rules.toml). Catch well-known
   system windows (taskbar, Chromium helper windows, dialogs).
4. **`default_action`** — the unconditional fallback, now `Float`.

User rules outrank learned rules deliberately: if flow's learned behavior
conflicts with the user's intent, the user's `flow-rules.toml` entry wins and
the learned rule is never consulted for that app.

## Learned Rules

Learned rules are flow's memory of the user's explicit float/tile decisions.
When the user runs `flow dispatch set-window float` or `flow dispatch set-window
tile` on a window, flow records the decision keyed on that app's identity. The
next time a window of the same app appears, it is classified automatically —
no rule writing required.

### What Triggers a Recording

Only **actual transitions** are recorded. A `set-window` call that resolves to a
no-op (the window was already in the requested mode) records nothing. This
keeps the history file free of redundant entries and makes repeated commands
idempotent.

The recording happens inside `dispatch_set_window`, after the transition
succeeds:

```mermaid
sequenceDiagram
    participant CLI as flow CLI
    participant D as dispatch_set_window
    participant H as HistoryStore
    participant R as WindowRegistry
    participant FS as history-flow-rules.toml

    CLI->>D: set-window float
    D->>D: resolve action (MakeFloating / MakeTiling / NoOp)
    D->>R: execute transition
    R-->>D: Ok
    alt action != NoOp
        D->>H: record(action, exe, class)
        H-->>D: changed? (dedup-and-update)
        alt changed
            D->>FS: save (atomic: temp + rename)
            D->>R: set_learned_rules(refresh pipeline)
        end
    end
    D-->>CLI: Ok
```

The pipeline refresh (`set_learned_rules`) is what makes the new decision take
effect immediately — the very next window of that app is classified using the
updated learned layer, with no daemon restart.

### App Identity: `exe` + `class`

The identity key for a learned rule is the app's **executable name** plus its
**Win32 window class name** (e.g. `chrome.exe` + `Chrome_WidgetWin_1`). Both
fields are used when the class is non-empty; when the class is empty, the key
falls back to `exe` alone.

Title is deliberately **not** part of the key. A window's title changes
constantly (the document name, the current tab, the cursor position), so keying
on it would fragment one app into dozens of "different" apps. The exe + class
pair is stable for the lifetime of an application version.

### Dedup-and-Update, Not Append

When the user toggles an app's mode repeatedly — float it, then tile it, then
float it again — flow does **not** append a new rule each time. Instead, the
existing learned rule for that `exe + class` is updated in place. This keeps
the history file bounded and makes the most recent decision authoritative.

If append were used instead, first-match-wins evaluation would keep returning
the *first* recorded (stale) decision forever, and the file would grow without
bound. Dedup-and-update avoids both problems.

### The History File

Learned rules persist to `history-flow-rules.toml` in the config directory
(default `%USERPROFILE%\.config\flow\`, overridable via `FLOW_CONFIG_DIR`). The
file uses the same schema as `flow-rules.toml` — a `default_action` field and a
`[[rules]]` array — so it is human-readable and human-editable. flow writes it
atomically (write to `.toml.tmp`, then rename) so a crash mid-write cannot
corrupt the existing file.

The `default_action` field in the history file is ignored at load time; only
the `rules` array is read. (The field exists only because the schema is shared
with `flow-rules.toml`.)

### Clearing or Overriding Learned State

To forget everything flow has learned:

- **Delete `history-flow-rules.toml`** — flow recreates it (empty) on the next
  recorded transition. Until then, classification falls through to the default
  rules and `default_action = Float`.

- **Delete a single app's entry** — open the file in a text editor and remove
  the matching `[[rules]]` block. flow will not re-add it until the user runs
  `set-window` on that app again.

- **Override a learned rule permanently** — add an explicit rule for the app in
  `flow-rules.toml`. User rules outrank learned rules, so the user's choice
  always wins regardless of what the history file says.

> The `ForgetApp` / `ForgetAllApps` IPC commands (declared in
> [`src/ipc/message.rs`](../../src/ipc/message.rs)) are intended as a
> programmatic way to clear learned state without touching the file by hand.
> They are not yet implemented.

## Where the Code Lives

| Concern | Location |
|---------|----------|
| History store (load, record, dedup, save) | [`src/config/history.rs`](../../src/config/history.rs) |
| History file path resolution | `history_rules_path[_in]` in [`src/config/dirs.rs`](../../src/config/dirs.rs) |
| `default_action = Float` default | `Default` impl for `WindowRulesConfig` in [`src/config/types.rs`](../../src/config/types.rs) |
| Four-layer pipeline evaluation | `ClassificationPipeline::classify` in [`src/registry/classification.rs`](../../src/registry/classification.rs) |
| Runtime pipeline refresh | `set_learned_rules` on `ClassificationPipeline` and `WindowRegistry` |
| Capture on `set-window` transition | `record_learned_transition` in [`src/daemon/dispatch.rs`](../../src/daemon/dispatch.rs) |
| Daemon-owned history store | `history` field on `FlowWM`, loaded in `new()` |

## Cross-References

- [Window Registry](./window-registry.md) — the full classification algorithm:
  Win32 pre-filters, the rule-pipeline flow diagram, and per-field match
  semantics.
- [Config & Persistence](./config-and-persistence.md) — the `flow-rules.toml`
  format for user-defined rules and the "code is the source of truth" config
  philosophy.
- [Floating Space](./floating-space.md) — where float-classified windows live
  and how tile↔float transitions are animated.
