//! Pure-logic window classification — no Win32 dependencies.
//!
//! Evaluates config [`WindowRule`]s against a [`WindowCandidate`] to determine
//! the window's [`WindowAction`] (Tile / Float / Ignore). Rules are processed
//! top-to-bottom with first-match-wins semantics. Maximized and fullscreen
//! overrides always take precedence over rules.
//!
//! # Design: Platform Independence
//!
//! This module is intentionally **platform-independent**. It accepts a
//! [`WindowCandidate`] (a plain Rust struct with no HWND) and produces a
//! [`WindowAction`] or [`WindowState`]. This means:
//!
//! - All classification logic can be **unit-tested without any Win32 mocking**.
//! - The classification rules are pure functions — same input, same output.
//! - The Win32 layer (which gathers window metadata) is cleanly separated
//!   from the decision logic.
//!
//! # Classification Pipeline
//!
//! ```text
//! WindowCandidate { exe, title, class, process_path }
//!          │
//!          ▼
//! ┌─ Maximized? ──► Ignored(IgnoredReason::Maximized)    ← always wins
//! │
//! ├─ Fullscreen? ─► Ignored(IgnoredReason::Fullscreen)   ← always wins
//! │
//! └─ ClassificationPipeline
//!     ├─ User rules (first match wins)
//!     ├─ Learned rules (future, first match wins)
//!     ├─ Default rules (first match wins)
//!     └─ Default action (fallback)
//! ```
//!
//! The [`ClassificationPipeline`] provides multi-layer classification with a
//! single entry point. The legacy [`classify_window`] function is kept for
//! backward compatibility with existing tests.

use crate::common::Rect;
use crate::config::types::{MatchRule, WindowAction, WindowRule, WindowRulesConfig};
use crate::registry::types::{FloatingState, IgnoredReason, TilingState, WindowState};

// ── WindowCandidate ────────────────────────────────────────────────

/// Window metadata used for rule classification.
///
/// This is a platform-independent snapshot of window properties used by
/// [`classify_window`] to determine the window's [`WindowAction`].
/// The Win32 layer fills this struct; the classifier never touches HWND.
///
/// # Design: Decoupling from Win32
///
/// By using a plain data struct instead of querying Win32 directly, we:
/// - Make classification testable without any OS dependencies.
/// - Keep the classifier pure (no side effects, no I/O).
/// - Allow future support for other windowing systems (X11, Wayland) by
///   just providing a different `WindowCandidate` source.
#[derive(Debug, Clone)]
pub struct WindowCandidate {
    /// Executable name (e.g. `"code.exe"`).
    pub exe: String,
    /// Window title bar text.
    pub title: String,
    /// Win32 window class name.
    pub class: String,
    /// Full path to the executable.
    pub process_path: String,
}

// ── classify_window ──────────────────────────────────────────────────

/// Classify a window candidate against an ordered rule list.
///
/// Evaluates `rules` top-to-bottom; the **first matching rule wins**.
/// If no rule matches, returns `default`.
///
/// # Example
///
/// ```
/// # use scrolling_tiling_manager::config::types::{MatchRule, WindowAction, WindowRule};
/// # use scrolling_tiling_manager::registry::classification::WindowCandidate;
/// # use scrolling_tiling_manager::registry::classification::classify_window;
/// let rules = vec![
///     WindowRule {
///         match_: MatchRule { exe: Some("explorer.exe".into()), ..Default::default() },
///         action: WindowAction::Ignore,
///         initial_width_eighths: None,
///         override_persist: false,
///     },
/// ];
/// let candidate = WindowCandidate {
///     exe: "explorer.exe".into(),
///     title: String::new(),
///     class: String::new(),
///     process_path: String::new(),
/// };
/// assert_eq!(classify_window(&candidate, &rules, WindowAction::Tile), WindowAction::Ignore);
/// ```
#[must_use]
pub fn classify_window(
    candidate: &WindowCandidate,
    rules: &[WindowRule],
    default: WindowAction,
) -> WindowAction {
    for rule in rules {
        if matches_rule(candidate, &rule.match_) {
            return rule.action;
        }
    }
    default
}

// ── matches_rule ────────────────────────────────────────────────────

/// Test whether a window candidate matches all specified fields in a rule.
///
/// Uses AND logic: **every specified (non-`None`) field must match**.
/// Unspecified fields are ignored entirely.
///
/// # Match Semantics
///
/// | Field               | Match mode                     | Case sensitivity |
/// |----------------------|--------------------------------|------------------|
/// | `exe`               | Exact match                    | Case-insensitive |
/// | `exe_regex`         | Regex (full string)            | Case-insensitive |
/// | `title`             | Exact match                    | Case-sensitive   |
/// | `title_contains`    | Substring match                | Case-sensitive   |
/// | `title_regex`       | Regex (full string)            | Case-sensitive   |
/// | `class`             | Exact match                    | Case-sensitive   |
/// | `class_regex`       | Regex (full string)            | Case-sensitive   |
/// | `process_path`      | Exact match                    | Case-insensitive |
/// | `process_path_regex` | Regex (full string)           | Case-insensitive |
///
/// If a regex pattern fails to compile, logs a warning and treats the field
/// as non-matching (returns `false`). This prevents a bad regex from crashing
/// the daemon — the window simply falls through to the next rule or default.
#[must_use]
pub fn matches_rule(candidate: &WindowCandidate, rule: &MatchRule) -> bool {
    // exe — exact, case-insensitive (Windows paths are case-insensitive)
    if let Some(ref exe) = rule.exe
        && !candidate.exe.eq_ignore_ascii_case(exe)
    {
        return false;
    }

    // exe_regex — case-insensitive regex
    if let Some(ref pattern) = rule.exe_regex {
        match regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(re) => {
                if !re.is_match(&candidate.exe) {
                    return false;
                }
            }
            Err(e) => {
                log::warn!(
                    "exe_regex pattern '{pattern}' failed to compile: {e}; treating as non-match"
                );
                return false;
            }
        }
    }

    // title — exact, case-sensitive
    if let Some(ref title) = rule.title
        && candidate.title != *title
    {
        return false;
    }

    // title_contains — substring, case-sensitive
    if let Some(ref substr) = rule.title_contains
        && !candidate.title.contains(substr)
    {
        return false;
    }

    // title_regex — case-sensitive regex
    if let Some(ref pattern) = rule.title_regex {
        match regex::Regex::new(pattern) {
            Ok(re) => {
                if !re.is_match(&candidate.title) {
                    return false;
                }
            }
            Err(e) => {
                log::warn!(
                    "title_regex pattern '{pattern}' failed to compile: {e}; treating as non-match"
                );
                return false;
            }
        }
    }

    // class — exact, case-sensitive
    if let Some(ref class) = rule.class
        && candidate.class != *class
    {
        return false;
    }

    // class_regex — case-sensitive regex
    if let Some(ref pattern) = rule.class_regex {
        match regex::Regex::new(pattern) {
            Ok(re) => {
                if !re.is_match(&candidate.class) {
                    return false;
                }
            }
            Err(e) => {
                log::warn!(
                    "class_regex pattern '{pattern}' failed to compile: {e}; treating as non-match"
                );
                return false;
            }
        }
    }

    // process_path — exact, case-insensitive (Windows paths)
    if let Some(ref path) = rule.process_path
        && !candidate.process_path.eq_ignore_ascii_case(path)
    {
        return false;
    }

    // process_path_regex — case-insensitive regex
    if let Some(ref pattern) = rule.process_path_regex {
        match regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(re) => {
                if !re.is_match(&candidate.process_path) {
                    return false;
                }
            }
            Err(e) => {
                log::warn!(
                    "process_path_regex pattern '{pattern}' failed to compile: {e}; treating as non-match"
                );
                return false;
            }
        }
    }

    true
}

// ── classify_with_state ─────────────────────────────────────────────

/// Classify a window and produce a full [`WindowState`].
///
/// This is the high-level classification entry point that combines rule-based
/// classification with OS-state overrides. The evaluation order is:
///
/// 1. **Maximized check** — If `is_maximized` is `true`, returns
///    `Ignored(Maximized)` immediately. Maximised windows have their own
///    layout and shouldn't be tiled.
///
/// 2. **Fullscreen check** — If `is_fullscreen` is `true`, returns
///    `Ignored(Fullscreen)`. Fullscreen apps (games, video players) must
///    not be moved or resized.
///
/// 3. **Rule evaluation** — Delegates to [`classify_window`] for config
///    rule matching. If no rule matches, uses `default`.
///
/// 4. **Action → State** — Converts the [`WindowAction`] to an initial
///    [`WindowState`] with placeholder positions (`col: 0, row: 0` for
///    tiling, zero-rect for floating). The layout engine will update these
///    when the window is actually placed.
///
/// # Why Maximized/Fullscreen Overrides?
///
/// These windows have their own management behavior that conflicts with tiling:
///
/// - Maximized windows fill the entire work area.
/// - Fullscreen windows cover even the taskbar.
///
/// Tiling either type would cause visual glitches and break the user's
/// expectation of how these windows behave.
#[must_use]
pub fn classify_with_state(
    candidate: &WindowCandidate,
    is_maximized: bool,
    is_fullscreen: bool,
    rules: &[WindowRule],
    default: WindowAction,
) -> WindowState {
    if is_maximized {
        return WindowState::Ignored(IgnoredReason::Maximized);
    }
    if is_fullscreen {
        return WindowState::Ignored(IgnoredReason::Fullscreen);
    }

    let action = classify_window(candidate, rules, default);
    action_to_state(action)
}

/// Convert a [`WindowAction`] to its corresponding initial [`WindowState`].
///
/// This produces placeholder positions (`col: 0, row: 0` / zero-rect) that
/// the layout engine will update once the window is placed.
#[must_use]
fn action_to_state(action: WindowAction) -> WindowState {
    match action {
        WindowAction::Tile => WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        WindowAction::Float => WindowState::Floating(FloatingState::Active {
            rect: Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
        }),
        WindowAction::Ignore => WindowState::Ignored(IgnoredReason::ExplicitRule),
    }
}

// ── ClassificationPipeline ───────────────────────────────────────────

/// Multi-layer classification pipeline.
///
/// Evaluates window rules in priority order:
///
/// 1. **User rules** — User-defined rules from `stm-rules.yml` (highest priority).
/// 2. **Learned rules** — Machine-learned rules from user behavior (future; currently empty).
/// 3. **Default rules** — Bundled default rules (lowest rule priority).
/// 4. **Default action** — Fallback when no rule matches at any layer.
///
/// This struct owns all rule layers and provides the single entry point
/// [`classify`](ClassificationPipeline::classify) used by the registry.
///
/// # Why Multi-Layer?
///
/// The separation allows:
/// - Users to override built-in defaults without editing bundled files.
/// - Future ML-based auto-classification to sit between user and default rules.
/// - Hot-reload of user rules without restarting the daemon.
///
/// # Usage in the Registry
///
/// The [`WindowRegistry`](crate::registry::core::WindowRegistry) stores a single
/// `ClassificationPipeline` instance and delegates all classification to it.
/// Maximized/fullscreen checks happen before the pipeline is consulted.
pub struct ClassificationPipeline {
    /// User-defined rules (highest priority after OS overrides).
    user_rules: Vec<WindowRule>,
    /// Default rules bundled with the application (lowest rule priority).
    default_rules: Vec<WindowRule>,
    /// Machine-learned rules from user behavior (future; currently empty).
    learned_rules: Vec<WindowRule>,
    /// Fallback action when no rule matches at any layer.
    default_action: WindowAction,
}

impl ClassificationPipeline {
    /// Creates a new classification pipeline from user and default rule configs.
    ///
    /// User rules take priority over default rules. The `default_action` from
    /// the user config is used as the final fallback.
    ///
    /// # Arguments
    ///
    /// * `user_rules` - Rules from the user's `stm-rules.yml`.
    /// * `default_rules` - Bundled default rules from `default-stm-rules.yml`.
    #[must_use]
    pub fn new(user_rules: WindowRulesConfig, default_rules: WindowRulesConfig) -> Self {
        let default_action = user_rules.default_action;
        Self {
            user_rules: user_rules.rules,
            default_rules: default_rules.rules,
            learned_rules: Vec::new(),
            default_action,
        }
    }

    /// Classify a window candidate using the full pipeline.
    ///
    /// Evaluates rule layers in order (user → learned → default), returning
    /// the action from the first matching rule. If no rule matches at any
    /// layer, returns the `default_action`.
    ///
    /// This returns a [`WindowAction`] (not [`WindowState`]) — OS overrides
    /// (maximized/fullscreen) are handled separately by
    /// [`classify_with_state_pipeline`].
    #[must_use]
    pub fn classify(&self, candidate: &WindowCandidate) -> WindowAction {
        // 1. User rules (first match wins)
        for rule in &self.user_rules {
            if matches_rule(candidate, &rule.match_) {
                return rule.action;
            }
        }

        // 2. Learned rules (currently empty, first match wins)
        for rule in &self.learned_rules {
            if matches_rule(candidate, &rule.match_) {
                return rule.action;
            }
        }

        // 3. Default rules (first match wins)
        for rule in &self.default_rules {
            if matches_rule(candidate, &rule.match_) {
                return rule.action;
            }
        }

        // 4. Fallback
        self.default_action
    }
}

// ── classify_with_state_pipeline ──────────────────────────────────────

/// Classify a window using the full pipeline (OS overrides + multi-layer rules).
///
/// This is the high-level entry point that combines:
/// 1. Maximized/fullscreen OS-state overrides (always win).
/// 2. Multi-layer rule evaluation via [`ClassificationPipeline`].
/// 3. Action → [`WindowState`] conversion.
///
/// # Arguments
///
/// * `candidate` - Window metadata for classification.
/// * `is_maximized` - Whether the window is currently maximized.
/// * `is_fullscreen` - Whether the window is in fullscreen mode.
/// * `pipeline` - The multi-layer classification pipeline.
///
/// # Returns
///
/// A [`WindowState`] — `Ignored(Maximized)` or `Ignored(Fullscreen)` for OS
/// overrides, or the pipeline's result converted to a state.
#[must_use]
pub fn classify_with_state_pipeline(
    candidate: &WindowCandidate,
    is_maximized: bool,
    is_fullscreen: bool,
    pipeline: &ClassificationPipeline,
) -> WindowState {
    if is_maximized {
        return WindowState::Ignored(IgnoredReason::Maximized);
    }
    if is_fullscreen {
        return WindowState::Ignored(IgnoredReason::Fullscreen);
    }

    let action = pipeline.classify(candidate);
    action_to_state(action)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::types::IgnoredReason;

    /// Helper to build a [`WindowCandidate`] with minimal boilerplate.
    fn candidate(exe: &str, title: &str, class: &str, process_path: &str) -> WindowCandidate {
        WindowCandidate {
            exe: exe.to_owned(),
            title: title.to_owned(),
            class: class.to_owned(),
            process_path: process_path.to_owned(),
        }
    }

    /// Helper to build a [`WindowRule`] from a [`MatchRule`] and [`WindowAction`].
    fn rule(match_rule: MatchRule, action: WindowAction) -> WindowRule {
        WindowRule {
            match_: match_rule,
            action,
            initial_width_eighths: None,
            override_persist: false,
        }
    }

    // --- classify_window tests ---

    #[test]
    fn exact_exe_match() {
        let r = rule(
            MatchRule {
                exe: Some("notepad.exe".into()),
                ..Default::default()
            },
            WindowAction::Tile,
        );
        let c = candidate("notepad.exe", "", "", "");
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Ignore),
            WindowAction::Tile
        );
    }

    #[test]
    fn case_insensitive_exe_match_windows_paths() {
        let r = rule(
            MatchRule {
                exe: Some("Explorer.EXE".into()),
                ..Default::default()
            },
            WindowAction::Ignore,
        );
        let c = candidate("explorer.exe", "", "", "");
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Tile),
            WindowAction::Ignore
        );
    }

    #[test]
    fn title_contains_case_sensitive() {
        let r = rule(
            MatchRule {
                title_contains: Some("Open File".into()),
                ..Default::default()
            },
            WindowAction::Ignore,
        );
        let c = candidate("explorer.exe", "Open File - Explorer", "", "");
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Tile),
            WindowAction::Ignore
        );
    }

    #[test]
    fn title_contains_case_sensitive_mismatch() {
        // "SETTINGS" should NOT match "Settings" (case-sensitive)
        let r = rule(
            MatchRule {
                title_contains: Some("SETTINGS".into()),
                ..Default::default()
            },
            WindowAction::Float,
        );
        let c = candidate("settings.exe", "Windows Settings", "", "");
        // Should NOT match because title_contains is now case-sensitive
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Tile),
            WindowAction::Tile
        );
    }

    #[test]
    fn all_fields_and_logic() {
        // All specified fields must match (AND)
        let r = rule(
            MatchRule {
                exe: Some("chrome.exe".into()),
                class: Some("Chrome_WidgetWin_1".into()),
                title: Some("New Tab - Google Chrome".into()),
                ..Default::default()
            },
            WindowAction::Tile,
        );
        // Fully matching candidate
        let c = candidate(
            "chrome.exe",
            "New Tab - Google Chrome",
            "Chrome_WidgetWin_1",
            "C:\\Program Files\\Google\\Chrome\\chrome.exe",
        );
        assert_eq!(
            classify_window(&c, &[r.clone()], WindowAction::Ignore),
            WindowAction::Tile
        );

        // Partially matching — wrong class → no match
        let c2 = candidate(
            "chrome.exe",
            "New Tab - Google Chrome",
            "SomeOtherClass",
            "",
        );
        assert_eq!(
            classify_window(&c2, &[r], WindowAction::Ignore),
            WindowAction::Ignore
        );
    }

    #[test]
    fn first_match_wins() {
        let rules = vec![
            rule(
                MatchRule {
                    exe: Some("chrome.exe".into()),
                    ..Default::default()
                },
                WindowAction::Ignore,
            ),
            rule(
                MatchRule {
                    exe: Some("chrome.exe".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            ),
        ];
        let c = candidate("chrome.exe", "", "", "");
        // First rule wins — should be Ignore, not Tile
        assert_eq!(
            classify_window(&c, &rules, WindowAction::Float),
            WindowAction::Ignore
        );
    }

    #[test]
    fn default_action_when_no_rule_matches() {
        let rules: Vec<WindowRule> = vec![];
        let c = candidate("unknown.exe", "Some Title", "", "");
        assert_eq!(
            classify_window(&c, &rules, WindowAction::Tile),
            WindowAction::Tile
        );
        assert_eq!(
            classify_window(&c, &rules, WindowAction::Float),
            WindowAction::Float
        );
    }

    #[test]
    fn empty_rules_list_returns_default() {
        let rules: Vec<WindowRule> = vec![];
        let c = candidate("code.exe", "main.rs - VS Code", "", "");
        assert_eq!(
            classify_window(&c, &rules, WindowAction::Ignore),
            WindowAction::Ignore
        );
    }

    #[test]
    fn window_candidate_with_empty_strings_no_match() {
        let r = rule(
            MatchRule {
                exe: Some("notepad.exe".into()),
                ..Default::default()
            },
            WindowAction::Tile,
        );
        let c = candidate("", "", "", "");
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Ignore),
            WindowAction::Ignore
        );
    }

    // --- matches_rule field-specific tests ---

    #[test]
    fn title_exact_case_sensitive() {
        let rule = MatchRule {
            title: Some("Calculator".into()),
            ..Default::default()
        };
        let c = candidate("calc.exe", "Calculator", "", "");
        assert!(matches_rule(&c, &rule));

        let c2 = candidate("calc.exe", "calculator", "", "");
        assert!(!matches_rule(&c2, &rule));
    }

    #[test]
    fn class_exact_case_sensitive() {
        let rule = MatchRule {
            class: Some("Chrome_WidgetWin_1".into()),
            ..Default::default()
        };
        let c = candidate("chrome.exe", "", "Chrome_WidgetWin_1", "");
        assert!(matches_rule(&c, &rule));

        let c2 = candidate("chrome.exe", "", "chrome_widgetwin_1", "");
        assert!(!matches_rule(&c2, &rule));
    }

    #[test]
    fn process_path_case_insensitive() {
        let rule = MatchRule {
            process_path: Some("C:\\Windows\\System32\\calc.exe".into()),
            ..Default::default()
        };
        let c = candidate("calc.exe", "", "", "C:\\Windows\\System32\\calc.exe");
        assert!(matches_rule(&c, &rule));

        // Case-insensitive: Calc.exe should match calc.exe
        let c2 = candidate("calc.exe", "", "", "C:\\Windows\\System32\\Calc.exe");
        assert!(matches_rule(&c2, &rule));
    }

    #[test]
    fn title_regex_matches() {
        let rule = MatchRule {
            title_regex: Some("^Settings".into()),
            ..Default::default()
        };
        let c = candidate("settings.exe", "Settings - Display", "", "");
        assert!(matches_rule(&c, &rule));

        let c2 = candidate("explorer.exe", "Windows Settings", "", "");
        assert!(!matches_rule(&c2, &rule));
    }

    #[test]
    fn exe_regex_matches_case_insensitive() {
        let rule = MatchRule {
            exe_regex: Some("chrome\\.exe".into()),
            ..Default::default()
        };
        let c = candidate("chrome.exe", "", "", "");
        assert!(matches_rule(&c, &rule));

        let c2 = candidate("Chrome.EXE", "", "", "");
        assert!(matches_rule(&c2, &rule));
    }

    #[test]
    fn class_regex_matches() {
        let rule = MatchRule {
            class_regex: Some("Chrome.*".into()),
            ..Default::default()
        };
        let c = candidate("chrome.exe", "", "Chrome_WidgetWin_1", "");
        assert!(matches_rule(&c, &rule));

        let c2 = candidate("chrome.exe", "", "chrome_widgetwin_1", "");
        // Case-sensitive: lowercase shouldn't match "Chrome.*"
        assert!(!matches_rule(&c2, &rule));
    }

    #[test]
    fn process_path_regex_matches_case_insensitive() {
        let rule = MatchRule {
            process_path_regex: Some(".*\\\\Google\\\\Chrome\\\\.*".into()),
            ..Default::default()
        };
        let c = candidate(
            "chrome.exe",
            "",
            "",
            "C:\\Program Files\\Google\\Chrome\\chrome.exe",
        );
        assert!(matches_rule(&c, &rule));

        let c2 = candidate(
            "chrome.exe",
            "",
            "",
            "c:\\program files\\google\\chrome\\chrome.exe",
        );
        // Case-insensitive: lowercase path should still match
        assert!(matches_rule(&c2, &rule));
    }

    #[test]
    fn invalid_title_regex_returns_false() {
        let rule = MatchRule {
            title_regex: Some("[invalid(".into()),
            ..Default::default()
        };
        let c = candidate("test.exe", "anything", "", "");
        // Invalid regex should return false, not panic
        assert!(!matches_rule(&c, &rule));
    }

    /// Negative: invalid `exe_regex` pattern logs a warning and is treated as non-match.
    #[test]
    fn invalid_exe_regex_returns_false() {
        let rule = MatchRule {
            exe_regex: Some("[invalid(".into()),
            ..Default::default()
        };
        let c = candidate("test.exe", "", "", "");
        assert!(!matches_rule(&c, &rule));
    }

    /// Negative: invalid `class_regex` pattern logs a warning and is treated as non-match.
    #[test]
    fn invalid_class_regex_returns_false() {
        let rule = MatchRule {
            class_regex: Some("[invalid(".into()),
            ..Default::default()
        };
        let c = candidate("test.exe", "", "SomeClass", "");
        assert!(!matches_rule(&c, &rule));
    }

    /// Negative: invalid `process_path_regex` pattern logs a warning and is treated as non-match.
    #[test]
    fn invalid_process_path_regex_returns_false() {
        let rule = MatchRule {
            process_path_regex: Some("[invalid(".into()),
            ..Default::default()
        };
        let c = candidate("test.exe", "", "", "C:\\path\\test.exe");
        assert!(!matches_rule(&c, &rule));
    }

    /// Positive: `(?i)` inline flag in `class_regex` overrides default case-sensitive behavior.
    ///
    /// `class_regex` is case-sensitive by default. The `(?i)` inline flag allows
    /// users to opt into case-insensitive matching for specific patterns.
    #[test]
    fn class_regex_inline_flag_i_overrides_case_sensitivity() {
        let rule = MatchRule {
            class_regex: Some("(?i)chrome_widgetwin_1".into()),
            ..Default::default()
        };
        // Without (?i), this lowercase string would NOT match (class_regex is
        // case-sensitive). With (?i), it should match.
        let c = candidate("chrome.exe", "", "chrome_widgetwin_1", "");
        assert!(
            matches_rule(&c, &rule),
            "(?i) should make class_regex case-insensitive"
        );

        // Also verify uppercase input matches.
        let c2 = candidate("chrome.exe", "", "CHROME_WIDGETWIN_1", "");
        assert!(matches_rule(&c2, &rule), "(?i) should match uppercase too");
    }

    /// Positive: `(?-i)` inline flag in `exe_regex` opts into case-sensitive matching.
    ///
    /// `exe_regex` is case-insensitive by default. The `(?-i)` inline flag allows
    /// users to opt into case-sensitive matching for specific patterns.
    #[test]
    fn exe_regex_inline_flag_neg_i_opts_into_case_sensitive() {
        let rule = MatchRule {
            exe_regex: Some("(?-i)Chrome.exe".into()),
            ..Default::default()
        };
        // Exact case should match.
        let c = candidate("Chrome.exe", "", "", "");
        assert!(
            matches_rule(&c, &rule),
            "exact case should match with (?-i)"
        );

        // Different case should NOT match (case-sensitive now).
        let c2 = candidate("chrome.exe", "", "", "");
        assert!(
            !matches_rule(&c2, &rule),
            "different case should not match with (?-i)"
        );
    }

    #[test]
    fn unspecified_fields_ignored() {
        let rule = MatchRule {
            exe: Some("code.exe".into()),
            ..Default::default()
        };
        let c = candidate("code.exe", "", "", "");
        assert!(matches_rule(&c, &rule));
    }

    // --- classify_with_state tests ---

    #[test]
    fn maximized_override_forces_ignored_maximized() {
        let rules: Vec<WindowRule> = vec![];
        let c = candidate("code.exe", "main.rs", "", "");
        let state = classify_with_state(&c, true, false, &rules, WindowAction::Tile);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::Maximized)
        ));
    }

    #[test]
    fn fullscreen_override_forces_ignored_fullscreen() {
        let rules: Vec<WindowRule> = vec![];
        let c = candidate("game.exe", "Game", "", "");
        let state = classify_with_state(&c, false, true, &rules, WindowAction::Tile);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::Fullscreen)
        ));
    }

    #[test]
    fn maximize_takes_precedence_over_fullscreen_check() {
        let rules: Vec<WindowRule> = vec![];
        let c = candidate("code.exe", "", "", "");
        let state = classify_with_state(&c, true, true, &rules, WindowAction::Tile);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::Maximized)
        ));
    }

    #[test]
    fn classify_with_state_tile_action() {
        let r = rule(
            MatchRule {
                exe: Some("code.exe".into()),
                ..Default::default()
            },
            WindowAction::Tile,
        );
        let c = candidate("code.exe", "", "", "");
        let state = classify_with_state(&c, false, false, &[r], WindowAction::Ignore);
        assert!(matches!(
            state,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
        ));
    }

    #[test]
    fn classify_with_state_float_action() {
        let r = rule(
            MatchRule {
                exe: Some("steam.exe".into()),
                ..Default::default()
            },
            WindowAction::Float,
        );
        let c = candidate("steam.exe", "", "", "");
        let state = classify_with_state(&c, false, false, &[r], WindowAction::Tile);
        assert!(matches!(
            state,
            WindowState::Floating(FloatingState::Active { rect: _ })
        ));
    }

    #[test]
    fn classify_with_state_ignore_action() {
        let r = rule(
            MatchRule {
                exe: Some("explorer.exe".into()),
                ..Default::default()
            },
            WindowAction::Ignore,
        );
        let c = candidate("explorer.exe", "", "", "");
        let state = classify_with_state(&c, false, false, &[r], WindowAction::Tile);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::ExplicitRule)
        ));
    }

    #[test]
    fn classify_with_state_default_used() {
        let rules: Vec<WindowRule> = vec![];
        let c = candidate("unknown.exe", "", "", "");
        let state = classify_with_state(&c, false, false, &rules, WindowAction::Tile);
        assert!(matches!(
            state,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
        ));
    }

    // --- action_to_state tests ---

    #[test]
    fn action_to_state_tile() {
        let state = action_to_state(WindowAction::Tile);
        assert!(matches!(
            state,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
        ));
    }

    #[test]
    fn action_to_state_float() {
        let state = action_to_state(WindowAction::Float);
        assert!(matches!(
            state,
            WindowState::Floating(FloatingState::Active {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0
                }
            })
        ));
    }

    #[test]
    fn action_to_state_ignore() {
        let state = action_to_state(WindowAction::Ignore);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::ExplicitRule)
        ));
    }

    // --- Edge cases ---

    #[test]
    fn empty_candidate_all_empty_strings() {
        let rules: Vec<WindowRule> = vec![];
        let c = candidate("", "", "", "");
        assert_eq!(
            classify_window(&c, &rules, WindowAction::Tile),
            WindowAction::Tile
        );
        let state = classify_with_state(&c, false, false, &rules, WindowAction::Tile);
        assert!(matches!(
            state,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
        ));
    }

    #[test]
    fn rule_with_all_fields_specified_matches() {
        let r = rule(
            MatchRule {
                exe: Some("app.exe".into()),
                title: Some("Main Window".into()),
                title_contains: None,
                title_regex: None,
                class: Some("AppClass".into()),
                process_path: Some("C:\\Apps\\app.exe".into()),
                exe_regex: None,
                class_regex: None,
                process_path_regex: None,
            },
            WindowAction::Float,
        );
        let c = candidate("app.exe", "Main Window", "AppClass", "C:\\Apps\\app.exe");
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Tile),
            WindowAction::Float
        );
    }

    #[test]
    fn rule_with_all_fields_specified_partial_mismatch() {
        let r = rule(
            MatchRule {
                exe: Some("app.exe".into()),
                title: Some("Main Window".into()),
                title_contains: None,
                title_regex: None,
                class: Some("AppClass".into()),
                process_path: Some("C:\\Apps\\app.exe".into()),
                exe_regex: None,
                class_regex: None,
                process_path_regex: None,
            },
            WindowAction::Float,
        );
        // Wrong process_path (case-insensitive won't save us — path is different)
        let c = candidate("app.exe", "Main Window", "AppClass", "D:\\Other\\app.exe");
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Tile),
            WindowAction::Tile
        );
    }

    // --- ClassificationPipeline tests ---

    #[test]
    fn pipeline_user_rule_takes_priority_over_default() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![rule(
                MatchRule {
                    exe: Some("chrome.exe".into()),
                    ..Default::default()
                },
                WindowAction::Ignore,
            )],
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![rule(
                MatchRule {
                    exe: Some("chrome.exe".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
        };

        let pipeline = ClassificationPipeline::new(user_rules, default_rules);
        let c = candidate("chrome.exe", "", "", "");
        // User rule should win over default rule
        assert_eq!(pipeline.classify(&c), WindowAction::Ignore);
    }

    #[test]
    fn pipeline_falls_through_to_default_rules() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![], // No user rules match
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![rule(
                MatchRule {
                    exe: Some("firefox.exe".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
        };

        let pipeline = ClassificationPipeline::new(user_rules, default_rules);
        let c = candidate("firefox.exe", "", "", "");
        assert_eq!(pipeline.classify(&c), WindowAction::Float);
    }

    #[test]
    fn pipeline_falls_through_to_default_action() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Float,
            rules: vec![],
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![],
        };

        let pipeline = ClassificationPipeline::new(user_rules, default_rules);
        let c = candidate("unknown.exe", "", "", "");
        // Should use user's default_action (Float)
        assert_eq!(pipeline.classify(&c), WindowAction::Float);
    }

    #[test]
    fn classify_with_state_pipeline_maximized() {
        let user_rules = WindowRulesConfig::default();
        let default_rules = WindowRulesConfig::default();
        let pipeline = ClassificationPipeline::new(user_rules, default_rules);

        let c = candidate("code.exe", "", "", "");
        let state = classify_with_state_pipeline(&c, true, false, &pipeline);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::Maximized)
        ));
    }

    #[test]
    fn classify_with_state_pipeline_fullscreen() {
        let user_rules = WindowRulesConfig::default();
        let default_rules = WindowRulesConfig::default();
        let pipeline = ClassificationPipeline::new(user_rules, default_rules);

        let c = candidate("game.exe", "", "", "");
        let state = classify_with_state_pipeline(&c, false, true, &pipeline);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::Fullscreen)
        ));
    }

    #[test]
    fn classify_with_state_pipeline_normal_classification() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![rule(
                MatchRule {
                    exe: Some("explorer.exe".into()),
                    ..Default::default()
                },
                WindowAction::Ignore,
            )],
        };
        let default_rules = WindowRulesConfig::default();
        let pipeline = ClassificationPipeline::new(user_rules, default_rules);

        let c = candidate("explorer.exe", "", "", "");
        let state = classify_with_state_pipeline(&c, false, false, &pipeline);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::ExplicitRule)
        ));
    }

    // --- Pipeline learned rules slot tests ---

    /// The pipeline's learned rules layer is currently always empty, but
    /// we verify the slot works: when user rules don't match and default
    /// rules don't match, the pipeline falls through to default_action
    /// even though a "learned" layer exists (it's empty).
    ///
    /// This test documents that the pipeline has 4 layers (user → learned →
    /// default → fallback) and that the learned layer is a no-op today.
    #[test]
    fn pipeline_learned_rules_slot_is_noop_when_empty() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Float,
            rules: vec![],
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![],
        };

        let pipeline = ClassificationPipeline::new(user_rules, default_rules);
        let c = candidate("anything.exe", "", "", "");
        // No rules at any layer → should fall through to user's default_action.
        assert_eq!(pipeline.classify(&c), WindowAction::Float);
    }

    /// Regression: verify that the pipeline with both user and default rules
    /// that are BOTH empty still produces the correct default_action.
    #[test]
    fn pipeline_empty_user_and_default_returns_user_default_action() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![],
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![],
        };

        let pipeline = ClassificationPipeline::new(user_rules, default_rules);
        let c = candidate("unknown.exe", "Some Title", "SomeClass", "");
        assert_eq!(pipeline.classify(&c), WindowAction::Ignore);
    }
}
