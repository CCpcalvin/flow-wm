# Config & Persistence (`stm-config`, `stm-persist`)

---

## stm-config

### Format Choice: TOML

Config is **TOML**. Rationale vs alternatives:

| Format | Pros | Cons | Decision |
|---|---|---|---|
| TOML | Clean syntax, comment support, good for flat/nested config | Slightly verbose for rule lists | ✅ Chosen |
| YAML | Human-readable, widely known, comment support | Indentation-sensitive, complex spec | ❌ Indentation pitfalls |
| JSON | Machine-friendly, JSON Schema native | No comments, verbose | ❌ Too unfriendly |
| Lua | Fully programmable, extensible | Requires Lua runtime, steep learning curve for non-programmers | ❌ Overkill at this stage |

Lua is a future option if users need conditional logic in config (e.g. "if monitor count > 2, use layout X"). For now, TOML covers all use cases cleanly.

### Two-Layer Config Model

The daemon uses a two-layer config model where **shipped defaults** provide the base and the **user's config** overlays on top. Merging happens at the TOML level (before serde deserialization):

```text
default-config.toml ──► toml::Value ─┐
                                     ├─ deep merge ──► merged Value ──► StmConfig
stm.toml            ──► toml::Value ─┘
```

- **Layer 1 (base)**: `default-config.toml` shipped next to `stmd.exe`.
  This is the single source of truth for default values. Edit this file to
  change defaults without recompiling.
- **Layer 2 (overlay)**: User's `stm.toml` in the config directory.
  Keys present here always win; absent keys inherit from shipped defaults.

Users' `stm.toml` files can be **partial** — they only need to contain the fields they want to override. The merge fills in all missing fields from the shipped defaults before deserializing.

### No Serde Defaults

All fields on `StmConfig` and its nested types are **required** — there are no `#[serde(default)]` annotations. This creates a built-in safety net:

- `default-config.toml` **must** contain every field. If a developer adds a
  new field to a Rust struct but forgets to add it to `default-config.toml`,
  deserialization fails with a clear `"missing field 'xyz'"` error.
- The compiled-in Rust `Default` impl serves as an **emergency fallback only**
  (e.g., dev environments without the shipped file). It is NOT the canonical
  source of default values.

**Exceptions**: `Vec` fields (like `WindowRulesConfig::rules`) retain `#[serde(default)]` because an empty collection is unambiguous. Per-entry boolean flags (like `WindowRule::override_persist`) also keep defaults for convenience.

### Autocomplete

Config autocomplete is delivered via **JSON Schema** generated from the config structs using `schemars`.

The generated schemas are written to disk at:
```
%USERPROFILE%\.config\stm\schemas\stm-config.schema.json
%USERPROFILE%\.config\stm\schemas\stm-rules.schema.json
```

The default `stm.toml` file is written with a `#:schema` comment header that activates editor autocomplete (works in VS Code with taplo extension, Neovim with taplo LSP, etc.):

```toml
#:schema ./schemas/stm-config.schema.json
```

### Config File Locations

```
%USERPROFILE%\.config\stm\stm.toml          # User app config (overlay)
%USERPROFILE%\.config\stm\stm-rules.toml    # User window rules
<stmd.exe dir>\default-config.toml           # Shipped app defaults
<stmd.exe dir>\default-stm-rules.toml        # Shipped default rules
```

Override with `--config <path>` flag on `stmd` or `stm`, or `STM_CONFIG_DIR` env var.

### Config File Split

Configuration is split across **two TOML files**:

- **`stm.toml`** (`StmConfig`) — Application settings (hotkeys, padding,
  animation, etc.). This file can be partial — missing fields inherit from
  `default-config.toml`.

- **`stm-rules.toml`** (`WindowRulesConfig`) — Window classification rules
  and default action. Rules are evaluated top-to-bottom, first match wins.

This separation allows users to edit rules frequently (adding ignore patterns
for new apps) without risk of corrupting their app settings, and vice versa.

### App Config Structure (`stm.toml`)

```toml
#:schema ./schemas/stm-config.schema.json

# Override defaults from default-config.toml here.
# See default-config.toml for all available fields.

# The key stm treats as its modifier
super_key = "VK_F24"

# Default column width in pixels
column_width = 1280

# Minimum column width in pixels
min_column_width_px = 640

# Padding settings in pixels
[padding]
window = 2    # inset around each window (visual gap)
up = 2        # top screen margin
down = 2      # bottom screen margin

# Hotkey bindings (all accept Super + optional Shift/Ctrl/Alt + key)
[hotkeys]
focus_left = "Super+H"
focus_right = "Super+L"
focus_up = "Super+K"
focus_down = "Super+J"
swap_left = "Super+Shift+H"
swap_right = "Super+Shift+L"
scroll_left = "Super+Left"
scroll_right = "Super+Right"
toggle_float = "Super+Space"
toggle_monocle = "Super+M"
close_window = "Super+Q"
reload_config = "Super+Shift+R"
place_above = "Super+A"

# Animation settings for layout transitions
[animation]
enabled = true
duration_ms = 240
easing = "ease-out-expo"

# Behavior when a minimized tiling window is restored
[minimize_restore]
strategy = "original_slot"   # original_slot | right_of_focused | append_right
```

### Window Rules Structure (`stm-rules.toml`)

```toml
#:schema ./schemas/stm-rules.schema.json

# Default action for windows not matching any rule
default_action = "tile"   # tile | float | ignore

# Window classification rules (first match wins)
[[rules]]
match = { exe = "explorer.exe", title_contains = "Open" }
action = "ignore"

[[rules]]
match = { exe = "steam.exe" }
action = "float"

[[rules]]
match = { class = "Chrome_WidgetWin_1" }
action = "tile"
initial_width_eighths = 4
```

### CLI Config Validation

`stm config check` validates config files by merging them with shipped defaults (same behavior as the daemon) and checking for parse/validation errors:

```bash
stm config check               # Validate all config files
stm config check --verbose     # Show detailed validation info
```

Partial user files are valid — only semantically invalid values (e.g., negative padding, min exceeding max) are reported as errors.

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
stm-persist  >  window_rules config  >  default_action
```

If the user pressed `Super+Space` to float VS Code yesterday, the next time VS Code opens it will be floating — even if `window_rules` says `tile`. This can be overridden by adding an explicit `window_rules` entry with `override_persist: true`.

### Storage Format

```
%USERPROFILE%\.config\stm\state.json
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
