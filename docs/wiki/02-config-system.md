# Config System (`src/config/`) — Reference

Developer reference for `src/config/`. Handles YAML parsing, serde (de)serialization, and JSON Schema generation for editor autocomplete.

---

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `types.rs` | 460 | StmConfig and all sub-types |
| `schema.rs` | 183 | JSON Schema generation via schemars |
| `mod.rs` | — | Re-exports |

Total: **643 lines**

---

## Top-Level Config

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StmConfig {
    pub super_key: String,
    pub default_window_action: WindowAction,
    pub default_column_width_eighths: u8,
    pub gaps: Gaps,
    pub hotkeys: Hotkeys,
    pub window_rules: Vec<WindowRule>,
    pub animation: AnimationConfig,
    pub minimize_restore: MinimizeRestore,
}
```

Every field has `#[serde(default = "...")]` so partial YAML configs fill in missing fields with defaults.

---

## Default Values

| Field | Default | Notes |
|-------|---------|-------|
| `super_key` | `"VK_F24"` | Virtual key code for modifier |
| `default_window_action` | `Tile` | Tile / Float / Ignore |
| `default_column_width_eighths` | `4` | 4/8 = half screen |
| `gaps.inner` | `8` | Gap between windows |
| `gaps.outer` | `16` | Gap to monitor edge |
| `animation.enabled` | `true` | — |
| `animation.duration_ms` | `180` | — |
| `animation.easing` | `"ease-out-expo"` | — |
| `minimize_restore.strategy` | `OriginalSlot` | OriginalSlot / RightOfFocused / AppendRight |

---

## Sub-Types

### Gaps

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Gaps {
    #[serde(default = "default_inner_gap")]
    pub inner: i32,   // default: 8
    #[serde(default = "default_outer_gap")]
    pub outer: i32,   // default: 16
}
```

### Hotkeys

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Hotkeys {
    pub focus_left: String,        // "Super+H"
    pub focus_right: String,       // "Super+L"
    pub focus_up: String,          // "Super+K"
    pub focus_down: String,        // "Super+J"
    pub swap_left: String,         // "Super+Shift+H"
    pub swap_right: String,        // "Super+Shift+L"
    pub scroll_left: String,       // "Super+Left"
    pub scroll_right: String,      // "Super+Right"
    pub toggle_float: String,      // "Super+Space"
    pub toggle_monocle: String,    // "Super+M"
    pub close_window: String,      // "Super+Q"
    pub reload_config: String,     // "Super+Shift+R"
    pub place_above: String,       // "Super+A"
}
```

13 bindings total. All defaults generated via a `default_hotkey!` macro.

### WindowRule

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WindowRule {
    #[serde(rename = "match")]
    pub match_: MatchRule,
    pub action: WindowAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_width_eighths: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub override_persist: bool,
}
```

The `match` field uses `#[serde(rename = "match")]` to avoid the Rust reserved word.

### MatchRule

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct MatchRule {
    pub exe: Option<String>,
    pub title: Option<String>,
    pub title_contains: Option<String>,
    pub title_regex: Option<String>,
    pub class: Option<String>,
    pub process_path: Option<String>,
}
```

All fields optional — first match wins in `window_rules` evaluation order.

### WindowAction

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
    Tile,
    Float,
    Ignore,
}
```

### AnimationConfig

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnimationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_duration_ms")]
    pub duration_ms: u32,     // default: 180
    #[serde(default = "default_easing")]
    pub easing: String,       // default: "ease-out-expo"
}
```

### MinimizeRestore

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MinimizeRestore {
    pub strategy: MinimizeRestoreStrategy,  // default: OriginalSlot
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MinimizeRestoreStrategy {
    OriginalSlot,
    RightOfFocused,
    AppendRight,
}
```

---

## YAML Examples

### Minimal (all defaults)

```yaml
{}
```

Produces: `super_key = "VK_F24"`, `gaps = { inner: 8, outer: 16 }`, etc.

### Full example

```yaml
super_key: VK_F24
default_window_action: tile
default_column_width_eighths: 4
gaps:
  inner: 12
  outer: 20
hotkeys:
  focus_left: Alt+H
  focus_right: Alt+L
window_rules:
  - match:
      exe: "explorer.exe"
      title_contains: "Open"
    action: ignore
  - match:
      class: "Chrome_WidgetWin_1"
    action: tile
    initial_width_eighths: 4
    override_persist: true
animation:
  enabled: false
  duration_ms: 250
  easing: ease-in-out-cubic
minimize_restore:
  strategy: append_right
```

---

## Schema Generation

```rust
pub fn generate_config_schema() -> StmResult<String>;
```

Produces a JSON Schema (via `schemars`) suitable for VS Code / Neovim YAML autocomplete. Write to `%APPDATA%\stm\stm-config-schema.json`.

The generated schema includes all top-level properties: `super_key`, `default_window_action`, `default_column_width_eighths`, `gaps`, `hotkeys`, `window_rules`, `animation`, `minimize_restore`.

---

## YAML Round-Trip Pattern

Every config field survives `StmConfig → YAML → StmConfig`:

```rust
let config = StmConfig { /* all fields customized */ };
let yaml = serde_yaml::to_string(&config).expect("serialize");
let parsed: StmConfig = serde_yaml::from_str(&yaml).expect("deserialize");
assert_eq!(parsed.super_key, config.super_key);
// ... every field verified
```

Invalid enum values are rejected by serde:

```rust
let yaml = "default_window_action: foobar";
assert!(serde_yaml::from_str::<StmConfig>(yaml).is_err());
```
