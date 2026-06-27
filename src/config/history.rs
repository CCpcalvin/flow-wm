//! Machine-learned window classification rules, persisted to
//! `history-stm-rules.toml`.
//!
//! The [`HistoryStore`] records the user's explicit `set-window float|tile`
//! decisions keyed on `exe + class` (falling back to `exe`-only when `class` is
//! empty). On the next window open, the classification pipeline consults these
//! learned rules between user rules and default rules.

use std::path::Path;

use crate::config::types::{MatchRule, WindowAction, WindowRule, WindowRulesConfig};

/// Persistent store of per-app learned window modes.
///
/// Records the user's explicit `set-window float|tile` decisions keyed on
/// `exe + class` (falling back to `exe`-only when `class` is empty). On the
/// next window open, the classification pipeline consults these learned
/// rules between user rules and default rules. See
/// (`docs/src/dev-guide/classification.md`) for the priority chain.
#[derive(Debug, Clone, Default)]
pub struct HistoryStore {
    /// The learned rules. `default_action` on this config is unused by the
    /// pipeline (the user's `default_action` governs) but is serialized to
    /// keep the TOML shape consistent with `stm-rules.toml`.
    config: WindowRulesConfig,
}

/// Header comment prepended to `history-stm-rules.toml` on every save.
const HISTORY_FILE_HEADER: &str = "\
# history-stm-rules.toml — learned window classification rules.
#
# This file is maintained automatically by stmd based on your `set-window`
# float/tile decisions. You may edit or delete entries by hand; stmd will
# recreate the file as you continue using it. To clear ALL learned rules,
# delete this file (or empty the [[rules]] list).
#
# Priority chain: user rules (stm-rules.toml) > learned rules (this file)
# > default rules > default_action.
";

impl HistoryStore {
    /// Load from `history-stm-rules.toml`. Resilient: missing file, parse
    /// error, or IO error all return an empty store with a warning log.
    /// The daemon must never fail to start because of a bad history file.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str::<WindowRulesConfig>(&contents) {
                Ok(config) => {
                    log::info!(
                        "loaded {} learned rules from {:?}",
                        config.rules.len(),
                        path
                    );
                    Self { config }
                }
                Err(err) => {
                    log::warn!("history file {path:?} is corrupt, ignoring: {err}");
                    Self::default()
                }
            },
            Err(err) => {
                // NotFound is expected on first run — use debug level.
                if err.kind() == std::io::ErrorKind::NotFound {
                    log::debug!("no history file at {path:?}; starting with empty learned rules");
                } else {
                    log::warn!(
                        "failed to read history file {path:?}: {err}; starting with empty learned rules"
                    );
                }
                Self::default()
            }
        }
    }

    /// Record (or update) the learned mode for an app.
    ///
    /// Identity key = `exe + class` (or `exe`-only when `class` is empty).
    /// An existing rule with matching identity is updated in place; otherwise
    /// a new rule is pushed. Returns `true` if the store changed. See
    /// (`docs/src/dev-guide/classification.md`) for the dedup rationale.
    pub fn record(&mut self, action: WindowAction, exe: &str, class: Option<&str>) -> bool {
        debug_assert!(!exe.is_empty(), "record() requires a non-empty exe");
        let class = class.filter(|c| !c.is_empty());

        let match_rule = MatchRule {
            exe: Some(exe.to_owned()),
            class: class.map(|c| c.to_owned()),
            ..MatchRule::default()
        };

        // Search for an existing rule with the same identity.
        let existing =
            self.config.rules.iter_mut().find(|r| {
                r.match_.exe.as_deref() == Some(exe) && r.match_.class.as_deref() == class
            });

        if let Some(rule) = existing {
            if rule.action != action {
                rule.action = action;
                return true;
            }
            // Same action — no change.
            return false;
        }

        // No existing entry — push a new rule.
        self.config.rules.push(WindowRule {
            match_: match_rule,
            action,
            initial_width_px: None,
            override_persist: false,
        });
        true
    }

    /// Serialize to TOML and write atomically (write-to-temp + rename) to
    /// the given path. Best-effort: the caller logs and continues.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the temp-file write fails or the atomic rename fails.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let toml_str = toml::to_string_pretty(&self.config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        let content = format!("{HISTORY_FILE_HEADER}{toml_str}");

        let tmp_path = path.with_extension("toml.tmp");

        // Write to temp file first.
        std::fs::write(&tmp_path, &content)?;

        // Atomic rename.
        match std::fs::rename(&tmp_path, path) {
            Ok(()) => Ok(()),
            Err(rename_err) => {
                // Best-effort cleanup of the temp file on rename failure.
                let _ = std::fs::remove_file(&tmp_path);
                Err(rename_err)
            }
        }
    }

    /// Borrow the learned rules for pipeline compilation.
    #[must_use]
    pub fn rules(&self) -> &[WindowRule] {
        &self.config.rules
    }

    /// Number of learned rules (useful for logging / tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.config.rules.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.config.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── load tests ────────────────────────────────────────────────────────

    /// Missing file returns an empty store without error.
    #[test]
    fn load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent-history.toml");
        let store = HistoryStore::load(&path);
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    /// Corrupt TOML file returns an empty store without error.
    #[test]
    fn load_corrupt_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad-history.toml");
        std::fs::write(&path, "this is = not = valid = toml = [[[[").unwrap();
        let store = HistoryStore::load(&path);
        assert!(store.is_empty());
    }

    /// A well-formed history file loads correctly.
    #[test]
    fn load_valid_file_populates_store() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history.toml");
        let toml_content = r#"
default_action = "float"

[[rules]]
match = { exe = "chrome.exe", class = "Chrome_WidgetWin_1" }
action = "tile"
"#;
        std::fs::write(&path, toml_content).unwrap();
        let store = HistoryStore::load(&path);
        assert_eq!(store.len(), 1);
        assert_eq!(store.rules()[0].action, WindowAction::Tile);
    }

    // ── record tests ───────────────────────────────────────────────────────

    /// Recording a new app with exe + class pushes a rule.
    #[test]
    fn record_new_app_pushes_rule() {
        let mut store = HistoryStore::default();
        let changed = store.record(WindowAction::Tile, "chrome.exe", Some("Chrome_WidgetWin_1"));
        assert!(changed);
        assert_eq!(store.len(), 1);
        assert_eq!(store.rules()[0].match_.exe.as_deref(), Some("chrome.exe"));
        assert_eq!(
            store.rules()[0].match_.class.as_deref(),
            Some("Chrome_WidgetWin_1")
        );
        assert_eq!(store.rules()[0].action, WindowAction::Tile);
    }

    /// Recording an existing app with the same exe + class updates the action.
    #[test]
    fn record_existing_app_updates_action() {
        let mut store = HistoryStore::default();
        store.record(WindowAction::Tile, "chrome.exe", Some("Chrome_WidgetWin_1"));
        let changed = store.record(
            WindowAction::Float,
            "chrome.exe",
            Some("Chrome_WidgetWin_1"),
        );
        assert!(changed);
        assert_eq!(store.len(), 1, "should still be exactly one rule (dedup)");
        assert_eq!(store.rules()[0].action, WindowAction::Float);
    }

    /// When class is None, identity falls back to exe-only.
    #[test]
    fn record_exe_only_when_class_none() {
        let mut store = HistoryStore::default();
        store.record(WindowAction::Float, "notepad.exe", None);
        assert_eq!(store.len(), 1);
        assert!(store.rules()[0].match_.class.is_none());
        assert_eq!(store.rules()[0].match_.exe.as_deref(), Some("notepad.exe"));
    }

    /// Recording the same action twice returns false (no change).
    #[test]
    fn record_no_change_returns_false() {
        let mut store = HistoryStore::default();
        let first = store.record(WindowAction::Tile, "code.exe", Some("Electron"));
        assert!(first);
        let second = store.record(WindowAction::Tile, "code.exe", Some("Electron"));
        assert!(!second, "same action should not report a change");
        assert_eq!(store.len(), 1);
    }

    /// When class is Some but empty string, identity falls back to exe-only.
    #[test]
    fn record_empty_class_treated_as_none() {
        let mut store = HistoryStore::default();
        store.record(WindowAction::Float, "app.exe", Some(""));
        // The rule should have class = None (empty string filtered out).
        assert!(store.rules()[0].match_.class.is_none());
        // Recording with explicit None should match (dedup).
        let changed = store.record(WindowAction::Tile, "app.exe", None);
        assert!(changed);
        assert_eq!(store.len(), 1);
    }

    /// Different classes for the same exe produce separate rules.
    #[test]
    fn identity_distinguishes_different_classes_same_exe() {
        let mut store = HistoryStore::default();
        store.record(WindowAction::Tile, "code.exe", Some("Electron"));
        store.record(WindowAction::Float, "code.exe", Some("Chrome_WidgetWin_1"));
        assert_eq!(store.len(), 2, "different classes should be separate rules");
    }

    // ── save tests ────────────────────────────────────────────────────────

    /// Save then load round-trips correctly.
    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history-stm-rules.toml");

        let mut store = HistoryStore::default();
        store.record(WindowAction::Tile, "chrome.exe", Some("Chrome_WidgetWin_1"));
        store.record(WindowAction::Float, "notepad.exe", None);
        store.save(&path).expect("save should succeed");

        let loaded = HistoryStore::load(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.rules()[0].action, WindowAction::Tile);
        assert_eq!(loaded.rules()[0].match_.exe.as_deref(), Some("chrome.exe"));
        assert_eq!(loaded.rules()[1].action, WindowAction::Float);
        assert_eq!(loaded.rules()[1].match_.exe.as_deref(), Some("notepad.exe"));
    }

    /// Saved file includes the human-readable header comment.
    #[test]
    fn save_includes_human_readable_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history-stm-rules.toml");

        let mut store = HistoryStore::default();
        store.record(WindowAction::Tile, "app.exe", None);
        store.save(&path).expect("save should succeed");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("history-stm-rules.toml"),
            "header should contain the file name"
        );
        assert!(
            contents.contains("Priority chain"),
            "header should explain the priority chain"
        );
    }

    /// Save produces valid TOML that `load` can parse.
    #[test]
    fn save_output_is_valid_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history-stm-rules.toml");

        let mut store = HistoryStore::default();
        store.record(WindowAction::Tile, "app.exe", Some("SomeClass"));
        store.save(&path).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        // Parsing must succeed.
        let _: WindowRulesConfig =
            toml::from_str(&contents).expect("saved content must be valid TOML");
    }
}
