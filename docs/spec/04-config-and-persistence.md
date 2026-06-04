# Config & Persistence (`stm-config`, `stm-persist`)

---

## stm-config

### Format Choice: YAML

Config is **YAML**. Rationale vs alternatives:

| Format | Pros | Cons | Decision |
|---|---|---|---|
| YAML | Human-readable, widely known, comment support | Indentation-sensitive | ✅ Chosen |
| JSON | Machine-friendly, JSON Schema native | No comments, verbose | ❌ Too unfriendly |
| TOML | Clean, comment support | Poor for nested structures | ❌ Awkward for rule lists |
| Lua | Fully programmable, extensible | Requires Lua runtime, steep learning curve for non-programmers | ❌ Overkill at this stage |

Lua is a future option if users need conditional logic in config (e.g. "if monitor count > 2, use layout X"). For now, YAML covers all use cases cleanly.

### Autocomplete

Config autocomplete is delivered via **JSON Schema** generated from the config structs using `schemars`.

The generated schema is embedded in the binary and written to disk at:
```
%APPDATA%\stm\stm-config-schema.json
```

Users add a single comment to their config file to activate editor autocomplete (works in VS Code, Neovim with yaml-language-server, etc.):

```yaml
# yaml-language-server: $schema=%APPDATA%/stm/stm-config-schema.json
```

`stm` automatically prints this line as a hint on first run and when `stm check-config` is called.

### Default Config Location

```
%APPDATA%\stm\stm.yaml
```

Override with `--config <path>` flag on `stmd` or `stm`.

### Config File Structure

```yaml
# yaml-language-server: $schema=%APPDATA%/stm/stm-config-schema.json

# The key stm treats as its modifier
super_key: VK_F24

# Default action for windows not matching any rule
default_window_action: tile   # tile | float | ignore

# Gap between windows in pixels
gaps:
  inner: 8
  outer: 16

# Hotkeys (all accept Super + optional Shift/Ctrl/Alt + key)
hotkeys:
  focus_left:         "Super+H"
  focus_right:        "Super+L"
  focus_up:           "Super+K"
  focus_down:         "Super+J"
  swap_left:          "Super+Shift+H"
  swap_right:         "Super+Shift+L"
  scroll_left:        "Super+Left"
  scroll_right:       "Super+Right"
  toggle_float:       "Super+Space"
  toggle_monocle:     "Super+M"
  close_window:       "Super+Q"
  reload_config:      "Super+Shift+R"
  place_above:        "Super+A"

# Window classification rules (first match wins)
window_rules:
  - match:
      exe: "explorer.exe"
      title_contains: "Open"
    action: ignore

  - match:
      exe: "steam.exe"
    action: float

  - match:
      class: "Chrome_WidgetWin_1"
    action: tile
    initial_width_eighths: 4

# Animation settings (passed to window-animation crate)
animation:
  enabled: true
  duration_ms: 180
  easing: "ease-out-expo"

# Behavior when a minimized tiling window is restored
minimize_restore:
  strategy: original_slot   # original_slot | right_of_focused | append_right
```

### CLI Config Mutations

`stm set` provides direct config mutation from the terminal without opening a text editor. It reads the YAML, updates the key, and writes it back preserving comments (using a comment-preserving YAML library).

```bash
stm set gaps.inner 12
stm set hotkeys.focus_left "Super+A"
stm set animation.duration_ms 200
stm set window_rules.0.action float     # mutate a rule by index
```

`stm check-config` validates the config against the JSON Schema and prints structured errors with line numbers.

---

## stm-persist

### Purpose

`stm-persist` stores **per-app learned state** that is separate from the config file. The config is what the user intentionally wrote; persist is what `stm` has learned from user behavior.

Currently persisted:

| Key | Type | Description |
|---|---|---|
| `preferred_state` | `tile \| float` | Set when user explicitly toggles via `toggle_float` |
| `preferred_width_eighths` | `u8` | Set when user explicitly resizes a column |
| `last_natural_size` | `Size` | Last size used when the window was unmanaged or floating |

### Precedence

```
stm-persist  >  window_rules config  >  default_window_action
```

If the user pressed `Super+Space` to float VS Code yesterday, the next time VS Code opens it will be floating — even if `window_rules` says `tile`. This can be overridden by adding an explicit `window_rules` entry with `override_persist: true`.

### Storage Format

```
%APPDATA%\stm\stm-persist.json
```

```json
{
  "schema_version": 1,
  "entries": [
    {
      "exe": "code.exe",
      "preferred_state": "float",
      "preferred_width_eighths": null,
      "last_natural_size": { "w": 1200, "h": 900 }
    },
    {
      "exe": "firefox.exe",
      "preferred_width_eighths": 5
    }
  ]
}
```

Entries are keyed by `exe` name (case-insensitive). Future versions may support keying by `class` or `process_path` for finer granularity.

### Clearing Persist State

```bash
stm forget <exe>         # Remove persist entry for a specific app
stm forget --all         # Clear entire persist store
```

