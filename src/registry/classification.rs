//! Pure-logic window classification — no Win32 dependencies.
//!
//! Evaluates config [`WindowRule`]s against a [`WindowCandidate`] to determine
//! the window's [`WindowAction`] (Tile / Float / Ignore). Rules are processed
//! top-to-bottom with first-match-wins semantics. Maximized and fullscreen
//! overrides always take precedence over rules.
//!
//! # Design: platform independence
//!
//! This module is intentionally **platform-independent**. It accepts a
//! [`WindowCandidate`] (a plain Rust struct with no HWND) and produces a
//! [`WindowAction`] or [`WindowState`]. All classification logic can therefore
//! be unit-tested without any Win32 mocking, and the Win32 layer that gathers
//! window metadata is cleanly separated from the decision logic.
//!
//! # Pipeline (overview)
//!
//! Classification runs after cheap Win32 pre-filters (visible, titled, Alt+Tab
//! visible, no owner) have already eliminated obvious non-candidates in the
//! registry layer. Then, in order: maximized → `Ignored(Maximized)`; fullscreen
//! → `Ignored(Fullscreen)`; otherwise the [`ClassificationPipeline`] runs user
//! rules, then learned rules, then default rules — first match wins,
//! falling back to the default action. All regex patterns are pre-compiled at
//! construction time. The [`matches_rule`] function is kept public for testing
//! individual rule-matching logic.
//!
//! See the developer guide's *Window Registry* chapter
//! (`docs/src/dev-guide/window-registry.md`) for the full classification
//! flowchart and the default-rule catalogue.

use crate::common::Rect;
use crate::config::types::{MatchRule, WindowAction, WindowRule, WindowRulesConfig};
use crate::registry::types::{FloatingState, IgnoredReason, TilingState, WindowState};

// ── WindowCandidate ────────────────────────────────────────────────

/// Platform-independent snapshot of window metadata used for rule classification.
///
/// The Win32 layer fills this struct; the classifier never touches HWND. Consumed
/// by [`ClassificationPipeline`] and [`matches_rule`] to produce a [`WindowAction`].
///
/// See the developer guide's *Window Registry* chapter
/// (`docs/src/dev-guide/window-registry.md`) for the decoupling rationale and
/// the full classification algorithm.
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

// ── CompiledRegex ─────────────────────────────────────────────────────

/// Pre-compiled regex with three possible states.
///
/// Used by [`CompiledRule`] to avoid recompiling regex patterns on every
/// classification call. Each regex field from a [`MatchRule`] is compiled
/// once at pipeline construction time.
///
/// # Variants
///
/// - `Unspecified` — The original pattern was `None`; skip this field entirely.
/// - `Valid(Regex)` — Pattern compiled successfully; use it for matching.
/// - `Invalid` — Pattern failed to compile; treat as non-match (same as
///   the runtime behaviour in [`matches_rule`], but logged once at startup).
enum CompiledRegex {
    /// Pattern was `None` — field not specified, skip check.
    Unspecified,
    /// Pattern compiled successfully.
    Valid(regex::Regex),
    /// Pattern failed to compile — treat as non-match.
    Invalid,
}

// ── CompiledRule ──────────────────────────────────────────────────────

/// A [`WindowRule`] with all regex patterns pre-compiled.
///
/// Created at pipeline construction time so that repeated calls to
/// [`ClassificationPipeline::classify`] avoid the cost of compiling regex
/// patterns on every match attempt.
///
/// # Performance
///
/// Without caching, each call to [`matches_rule`] rebuilds up to 4 regex
/// objects (`exe_regex`, `title_regex`, `class_regex`, `process_path_regex`).
/// For a daemon that classifies hundreds of windows and re-classifies on
/// config reload, this is measurable overhead. Pre-compiling once at
/// construction time makes every subsequent `classify()` call pure matching
/// with zero allocations.
///
/// # Fallback for Invalid Patterns
///
/// If a regex pattern fails to compile, the corresponding [`CompiledRegex`]
/// is set to `Invalid`. At match time, this causes the field to return
/// `false` (non-match), exactly matching the runtime behaviour of
/// [`matches_rule`].
struct CompiledRule {
    /// The original rule (holds action, non-regex fields, etc.).
    rule: WindowRule,
    /// Pre-compiled `exe_regex` (case-insensitive).
    exe_regex: CompiledRegex,
    /// Pre-compiled `title_regex` (case-sensitive).
    title_regex: CompiledRegex,
    /// Pre-compiled `class_regex` (case-sensitive).
    class_regex: CompiledRegex,
    /// Pre-compiled `process_path_regex` (case-insensitive).
    process_path_regex: CompiledRegex,
}

/// Compile a single regex pattern into a [`CompiledRegex`].
///
/// - `None` → `Unspecified`
/// - Valid pattern → `Valid(Regex)` with the given case sensitivity
/// - Invalid pattern → logs a warning → `Invalid`
fn compile_regex(pattern: Option<&str>, case_insensitive: bool, field_name: &str) -> CompiledRegex {
    match pattern {
        None => CompiledRegex::Unspecified,
        Some(p) => {
            let mut builder = regex::RegexBuilder::new(p);
            builder.case_insensitive(case_insensitive);
            match builder.build() {
                Ok(re) => CompiledRegex::Valid(re),
                Err(e) => {
                    log::warn!(
                        "{field_name} pattern '{p}' failed to compile: {e}; treating as non-match"
                    );
                    CompiledRegex::Invalid
                }
            }
        }
    }
}

/// Test whether a window candidate matches a compiled rule.
///
/// Uses AND logic identical to [`matches_rule`]: every specified (non-`None`)
/// field must match. The difference is that regex fields use pre-compiled
/// [`CompiledRegex`] values instead of building a new `Regex` per call.
#[must_use]
fn matches_compiled_rule(candidate: &WindowCandidate, compiled: &CompiledRule) -> bool {
    let rule = &compiled.rule.match_;

    // exe — exact, case-insensitive (Windows paths are case-insensitive)
    if let Some(ref exe) = rule.exe
        && !candidate.exe.eq_ignore_ascii_case(exe)
    {
        return false;
    }

    // exe_regex — pre-compiled, case-insensitive
    match &compiled.exe_regex {
        CompiledRegex::Valid(re) => {
            if !re.is_match(&candidate.exe) {
                return false;
            }
        }
        CompiledRegex::Invalid => return false,
        CompiledRegex::Unspecified => {}
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

    // title_regex — pre-compiled, case-sensitive
    match &compiled.title_regex {
        CompiledRegex::Valid(re) => {
            if !re.is_match(&candidate.title) {
                return false;
            }
        }
        CompiledRegex::Invalid => return false,
        CompiledRegex::Unspecified => {}
    }

    // class — exact, case-sensitive
    if let Some(ref class) = rule.class
        && candidate.class != *class
    {
        return false;
    }

    // class_regex — pre-compiled, case-sensitive
    match &compiled.class_regex {
        CompiledRegex::Valid(re) => {
            if !re.is_match(&candidate.class) {
                return false;
            }
        }
        CompiledRegex::Invalid => return false,
        CompiledRegex::Unspecified => {}
    }

    // process_path — exact, case-insensitive (Windows paths)
    if let Some(ref path) = rule.process_path
        && !candidate.process_path.eq_ignore_ascii_case(path)
    {
        return false;
    }

    // process_path_regex — pre-compiled, case-insensitive
    match &compiled.process_path_regex {
        CompiledRegex::Valid(re) => {
            if !re.is_match(&candidate.process_path) {
                return false;
            }
        }
        CompiledRegex::Invalid => return false,
        CompiledRegex::Unspecified => {}
    }

    true
}

/// Compile all regex patterns in a list of [`WindowRule`]s into [`CompiledRule`]s.
fn compile_rules(rules: Vec<WindowRule>) -> Vec<CompiledRule> {
    rules
        .into_iter()
        .map(|rule| {
            let m = &rule.match_;
            CompiledRule {
                exe_regex: compile_regex(m.exe_regex.as_deref(), true, "exe_regex"),
                title_regex: compile_regex(m.title_regex.as_deref(), false, "title_regex"),
                class_regex: compile_regex(m.class_regex.as_deref(), false, "class_regex"),
                process_path_regex: compile_regex(
                    m.process_path_regex.as_deref(),
                    true,
                    "process_path_regex",
                ),
                rule,
            }
        })
        .collect()
}

// ── ClassificationPipeline ───────────────────────────────────────────

/// Multi-layer classification pipeline with pre-compiled regex patterns.
///
/// Evaluates rule layers in priority order (first match wins):
///
/// 1. **User rules** — from `flow-rules.toml` (highest priority).
/// 2. **Learned rules** — persisted user decisions from `set-window` (`history-flow-rules.toml`).
/// 3. **Default rules** — bundled at compile time (lowest rule priority).
/// 4. **Default action** — fallback when no rule matches at any layer.
///
/// All regex patterns are pre-compiled at construction. Entry point:
/// [`classify`](Self::classify). OS-state overrides (maximized/fullscreen)
/// are handled before the pipeline runs — see [`classify_with_state_pipeline`].
pub struct ClassificationPipeline {
    /// User-defined rules with pre-compiled regexes (highest priority after OS overrides).
    user_rules: Vec<CompiledRule>,
    /// Default rules bundled with the application (lowest rule priority).
    default_rules: Vec<CompiledRule>,
    /// Learned rules — persisted user decisions, populated at runtime via
    /// [`set_learned_rules`](Self::set_learned_rules).
    learned_rules: Vec<CompiledRule>,
    /// Fallback action when no rule matches at any layer.
    default_action: WindowAction,
}

impl ClassificationPipeline {
    /// Creates a new classification pipeline from user and default rule configs.
    ///
    /// All regex patterns are pre-compiled at this point. Invalid patterns
    /// are logged as warnings and treated as non-matching at classification
    /// time (identical to the runtime fallback in [`matches_rule`]).
    ///
    /// The fallback action used when no rule at any layer matches is taken
    /// from `user_rules.default_action`. The `default_rules.default_action`
    /// field is intentionally ignored — the user's preference always governs
    /// the final fallback.
    ///
    /// # Arguments
    ///
    /// * `user_rules` - Rules from the user's `flow-rules.toml`.
    /// * `default_rules` - Bundled default rules (embedded at compile time from
    ///   `default-flow-rules.toml`).
    #[must_use]
    pub fn new(user_rules: WindowRulesConfig, default_rules: WindowRulesConfig) -> Self {
        let default_action = user_rules.default_action;
        Self {
            user_rules: compile_rules(user_rules.rules),
            default_rules: compile_rules(default_rules.rules),
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
    /// Returns a [`WindowAction`] (not [`WindowState`]) — OS overrides
    /// (maximized/fullscreen) are handled separately by
    /// [`classify_with_state_pipeline`].
    #[must_use]
    pub fn classify(&self, candidate: &WindowCandidate) -> WindowAction {
        // 1. User rules (first match wins)
        for compiled in &self.user_rules {
            if matches_compiled_rule(candidate, compiled) {
                return compiled.rule.action;
            }
        }

        // 2. Learned rules (first match wins)
        for compiled in &self.learned_rules {
            if matches_compiled_rule(candidate, compiled) {
                return compiled.rule.action;
            }
        }

        // 3. Default rules (first match wins)
        for compiled in &self.default_rules {
            if matches_compiled_rule(candidate, compiled) {
                return compiled.rule.action;
            }
        }

        // 4. Fallback
        self.default_action
    }

    /// Replace the learned-rules layer with recompiled versions of `rules`.
    ///
    /// Learned rules sit between user rules and default rules in the priority
    /// chain — see (`docs/src/dev-guide/classification.md`). Call this after
    /// the daemon records a new user decision (e.g. via `set-window tile`)
    /// so the next window of the same app is classified to the learned mode.
    ///
    /// Recompiles all regex patterns (cheap at human-frequency update rates).
    /// Invalid regex patterns are logged and treated as non-matching at
    /// classification time, identical to [`new`](Self::new).
    pub fn set_learned_rules(&mut self, rules: Vec<WindowRule>) {
        self.learned_rules = compile_rules(rules);
    }

    /// Replace the user-rules layer (and fallback action) from `flow-rules.toml`.
    ///
    /// Used by hot-reload so classification picks up edited rules without
    /// restarting the daemon. Recompiles all regex patterns; invalid patterns
    /// are logged and treated as non-matching, identical to
    /// [`new`](Self::new). The fallback `default_action` is also refreshed
    /// from `user_rules.default_action`. See
    /// (`docs/src/dev-guide/config-and-persistence.md`).
    pub fn set_user_rules(&mut self, user_rules: WindowRulesConfig) {
        self.default_action = user_rules.default_action;
        self.user_rules = compile_rules(user_rules.rules);
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
/// Visibility is `pub(super)` — this function is called by
/// [`core::WindowRegistry`](super::core::WindowRegistry) and is not part of
/// the public API of the registry module. External callers should interact
/// with the registry directly, not with the classification internals.
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
pub(super) fn classify_with_state_pipeline(
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
            initial_width_px: None,
            override_persist: false,
        }
    }

    /// Helper to build a [`ClassificationPipeline`] from user rules and a default action.
    ///
    /// Creates a pipeline with the given rules as user rules, no default rules,
    /// and the given `default_action`. Useful for testing single-layer classification
    /// without boilerplate.
    fn pipeline_from(
        rules: Vec<WindowRule>,
        default_action: WindowAction,
    ) -> ClassificationPipeline {
        ClassificationPipeline::new(
            WindowRulesConfig {
                default_action,
                rules,
            },
            WindowRulesConfig::default(),
        )
    }

    // --- Pipeline classification tests (single-layer) ---

    #[test]
    fn pipeline_exact_exe_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("notepad.exe".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("notepad.exe", "", "", "");
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    #[test]
    fn pipeline_case_insensitive_exe_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("Explorer.EXE".into()),
                    ..Default::default()
                },
                WindowAction::Ignore,
            )],
            WindowAction::Tile,
        );
        let c = candidate("explorer.exe", "", "", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    #[test]
    fn pipeline_title_contains_case_sensitive() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    title_contains: Some("Open File".into()),
                    ..Default::default()
                },
                WindowAction::Ignore,
            )],
            WindowAction::Tile,
        );
        let c = candidate("explorer.exe", "Open File - Explorer", "", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    #[test]
    fn pipeline_title_contains_case_sensitive_mismatch() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    title_contains: Some("SETTINGS".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
            WindowAction::Tile,
        );
        let c = candidate("settings.exe", "Windows Settings", "", "");
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    #[test]
    fn pipeline_all_fields_and_logic() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("chrome.exe".into()),
                    class: Some("Chrome_WidgetWin_1".into()),
                    title: Some("New Tab - Google Chrome".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate(
            "chrome.exe",
            "New Tab - Google Chrome",
            "Chrome_WidgetWin_1",
            "C:\\Program Files\\Google\\Chrome\\chrome.exe",
        );
        assert_eq!(p.classify(&c), WindowAction::Tile);

        let c2 = candidate(
            "chrome.exe",
            "New Tab - Google Chrome",
            "SomeOtherClass",
            "",
        );
        assert_eq!(p.classify(&c2), WindowAction::Ignore);
    }

    #[test]
    fn pipeline_first_match_wins() {
        let p = pipeline_from(
            vec![
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
            ],
            WindowAction::Float,
        );
        let c = candidate("chrome.exe", "", "", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    #[test]
    fn pipeline_default_action_when_no_rule_matches() {
        let p = pipeline_from(vec![], WindowAction::Tile);
        let c = candidate("unknown.exe", "Some Title", "", "");
        assert_eq!(p.classify(&c), WindowAction::Tile);

        let p2 = pipeline_from(vec![], WindowAction::Float);
        assert_eq!(p2.classify(&c), WindowAction::Float);
    }

    #[test]
    fn pipeline_window_candidate_with_empty_strings_no_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("notepad.exe".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("", "", "", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    #[test]
    fn pipeline_rule_with_all_fields_specified_matches() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("app.exe".into()),
                    title: Some("Main Window".into()),
                    class: Some("AppClass".into()),
                    process_path: Some("C:\\Apps\\app.exe".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
            WindowAction::Tile,
        );
        let c = candidate("app.exe", "Main Window", "AppClass", "C:\\Apps\\app.exe");
        assert_eq!(p.classify(&c), WindowAction::Float);
    }

    #[test]
    fn pipeline_rule_with_all_fields_specified_partial_mismatch() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("app.exe".into()),
                    title: Some("Main Window".into()),
                    class: Some("AppClass".into()),
                    process_path: Some("C:\\Apps\\app.exe".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
            WindowAction::Tile,
        );
        let c = candidate("app.exe", "Main Window", "AppClass", "D:\\Other\\app.exe");
        assert_eq!(p.classify(&c), WindowAction::Tile);
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

    // --- classify_with_state_pipeline tests ---

    #[test]
    fn classify_with_state_pipeline_maximized_override() {
        let pipeline = pipeline_from(vec![], WindowAction::Tile);
        let c = candidate("code.exe", "main.rs", "", "");
        let state = classify_with_state_pipeline(&c, true, false, &pipeline);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::Maximized)
        ));
    }

    #[test]
    fn classify_with_state_pipeline_fullscreen_override() {
        let pipeline = pipeline_from(vec![], WindowAction::Tile);
        let c = candidate("game.exe", "Game", "", "");
        let state = classify_with_state_pipeline(&c, false, true, &pipeline);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::Fullscreen)
        ));
    }

    #[test]
    fn classify_with_state_pipeline_maximize_takes_precedence_over_fullscreen() {
        let pipeline = pipeline_from(vec![], WindowAction::Tile);
        let c = candidate("code.exe", "", "", "");
        let state = classify_with_state_pipeline(&c, true, true, &pipeline);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::Maximized)
        ));
    }

    #[test]
    fn classify_with_state_pipeline_tile_action() {
        let pipeline = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("code.exe".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("code.exe", "", "", "");
        let state = classify_with_state_pipeline(&c, false, false, &pipeline);
        assert!(matches!(
            state,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
        ));
    }

    #[test]
    fn classify_with_state_pipeline_float_action() {
        let pipeline = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("steam.exe".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
            WindowAction::Tile,
        );
        let c = candidate("steam.exe", "", "", "");
        let state = classify_with_state_pipeline(&c, false, false, &pipeline);
        assert!(matches!(
            state,
            WindowState::Floating(FloatingState::Active { rect: _ })
        ));
    }

    #[test]
    fn classify_with_state_pipeline_ignore_action() {
        let pipeline = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("explorer.exe".into()),
                    ..Default::default()
                },
                WindowAction::Ignore,
            )],
            WindowAction::Tile,
        );
        let c = candidate("explorer.exe", "", "", "");
        let state = classify_with_state_pipeline(&c, false, false, &pipeline);
        assert!(matches!(
            state,
            WindowState::Ignored(IgnoredReason::ExplicitRule)
        ));
    }

    #[test]
    fn classify_with_state_pipeline_default_used() {
        let pipeline = pipeline_from(vec![], WindowAction::Tile);
        let c = candidate("unknown.exe", "", "", "");
        let state = classify_with_state_pipeline(&c, false, false, &pipeline);
        assert!(matches!(
            state,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
        ));
    }

    #[test]
    fn classify_with_state_pipeline_empty_candidate() {
        let pipeline = pipeline_from(vec![], WindowAction::Tile);
        let c = candidate("", "", "", "");
        let state = classify_with_state_pipeline(&c, false, false, &pipeline);
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

    // --- ClassificationPipeline multi-layer tests ---

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

    // --- Pipeline learned rules slot tests ---

    /// The pipeline's learned rules layer is initially empty — when user rules
    /// don't match and default rules don't match, the pipeline falls through to
    /// default_action even though a learned layer exists (it's empty).
    ///
    /// This test documents that the pipeline has 4 layers (user → learned →
    /// default → fallback) and that the learned layer is a no-op until
    /// populated via `set_learned_rules`.
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

    /// Positive: `set_learned_rules` compiles and installs rules so that
    /// a matching candidate is classified using the learned layer.
    #[test]
    fn set_learned_rules_classifies_with_learned_rule() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![],
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![],
        };

        let mut pipeline = ClassificationPipeline::new(user_rules, default_rules);
        pipeline.set_learned_rules(vec![rule(
            MatchRule {
                exe: Some("test.exe".into()),
                ..Default::default()
            },
            WindowAction::Float,
        )]);

        let c = candidate("test.exe", "", "", "");
        assert_eq!(
            pipeline.classify(&c),
            WindowAction::Float,
            "learned rule should classify test.exe as Float"
        );
    }

    /// Priority: user rules beat learned rules for the same candidate.
    #[test]
    fn set_learned_rules_user_rules_still_win() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![rule(
                MatchRule {
                    exe: Some("test.exe".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![],
        };

        let mut pipeline = ClassificationPipeline::new(user_rules, default_rules);
        pipeline.set_learned_rules(vec![rule(
            MatchRule {
                exe: Some("test.exe".into()),
                ..Default::default()
            },
            WindowAction::Float,
        )]);

        let c = candidate("test.exe", "", "", "");
        assert_eq!(
            pipeline.classify(&c),
            WindowAction::Tile,
            "user rules should take priority over learned rules"
        );
    }

    /// Priority: learned rules beat default rules for the same candidate.
    #[test]
    fn set_learned_rules_beats_default_rules() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![],
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![rule(
                MatchRule {
                    exe: Some("test.exe".into()),
                    ..Default::default()
                },
                WindowAction::Ignore,
            )],
        };

        let mut pipeline = ClassificationPipeline::new(user_rules, default_rules);
        pipeline.set_learned_rules(vec![rule(
            MatchRule {
                exe: Some("test.exe".into()),
                ..Default::default()
            },
            WindowAction::Float,
        )]);

        let c = candidate("test.exe", "", "", "");
        assert_eq!(
            pipeline.classify(&c),
            WindowAction::Float,
            "learned rules should take priority over default rules"
        );
    }

    /// Replacement: calling `set_learned_rules` twice fully replaces, not appends.
    #[test]
    fn set_learned_rules_replaces_previous() {
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![],
        };
        let default_rules = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![],
        };

        let mut pipeline = ClassificationPipeline::new(user_rules, default_rules);

        // First call: Float
        pipeline.set_learned_rules(vec![rule(
            MatchRule {
                exe: Some("test.exe".into()),
                ..Default::default()
            },
            WindowAction::Float,
        )]);
        let c = candidate("test.exe", "", "", "");
        assert_eq!(pipeline.classify(&c), WindowAction::Float);

        // Second call: Tile (same exe, different action)
        pipeline.set_learned_rules(vec![rule(
            MatchRule {
                exe: Some("test.exe".into()),
                ..Default::default()
            },
            WindowAction::Tile,
        )]);
        assert_eq!(
            pipeline.classify(&c),
            WindowAction::Tile,
            "second set_learned_rules should fully replace the first"
        );
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

    // --- Pipeline with real embedded default rules (phase 3 integration) ---

    /// End-to-end: the classification pipeline uses the real embedded default
    /// rules (from [`crate::config::lifecycle::load_default_rules`]) when user
    /// rules don't match.
    ///
    /// This is the critical regression test for the bug fix that embedded
    /// `default-flow-rules.toml` at compile time. Before the fix, the default
    /// rules layer was empty during development (file not found next to exe),
    /// so phase 3 of the pipeline matched nothing. This test:
    ///
    /// 1. Builds a pipeline with **empty user rules** and the **real embedded
    ///    defaults** from `load_default_rules()`.
    /// 2. Classifies a `Shell_TrayWnd` window (the Windows taskbar).
    /// 3. Verifies the pipeline reaches phase 3 (default rules) and returns
    ///    `Ignore` — proving the embedded defaults are actually consulted.
    ///
    /// Also verifies user rules still take priority: a user rule for
    /// `Shell_TrayWnd` with action `Tile` should override the default
    /// `Ignore`.
    #[test]
    fn pipeline_embedded_default_rules_classify_taskbar_as_ignore() {
        use crate::config::lifecycle::load_default_rules;

        // Arrange: pipeline with empty user rules + real embedded defaults.
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![],
        };
        let default_rules = load_default_rules();

        let pipeline = ClassificationPipeline::new(user_rules, default_rules);

        // Act: classify a Shell_TrayWnd window (Windows taskbar).
        let taskbar = candidate("explorer.exe", "", "Shell_TrayWnd", "");

        // Assert: phase 3 default rules should match → Ignore.
        assert_eq!(
            pipeline.classify(&taskbar),
            WindowAction::Ignore,
            "embedded default rules should classify Shell_TrayWnd as Ignore"
        );
    }

    /// End-to-end: user rules take priority over the real embedded default
    /// rules for the same window.
    ///
    /// This proves that phase 3 (default rules) is correctly bypassed when
    /// a user rule matches first. Uses the real embedded defaults loaded by
    /// [`crate::config::lifecycle::load_default_rules`].
    #[test]
    fn pipeline_user_rule_overrides_embedded_default_for_taskbar() {
        use crate::config::lifecycle::load_default_rules;

        // Arrange: user rule overrides the default Shell_TrayWnd → Ignore
        // classification with Tile (contrived but proves priority).
        let user_rules = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![rule(
                MatchRule {
                    class: Some("Shell_TrayWnd".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
        };
        let default_rules = load_default_rules();

        let pipeline = ClassificationPipeline::new(user_rules, default_rules);

        // Act: classify a Shell_TrayWnd window.
        let taskbar = candidate("explorer.exe", "", "Shell_TrayWnd", "");

        // Assert: user rule (Tile) should win over default rule (Ignore).
        assert_eq!(
            pipeline.classify(&taskbar),
            WindowAction::Tile,
            "user rule should override embedded default for Shell_TrayWnd"
        );
    }

    // --- Pipeline regex rule tests (pre-compiled via ClassificationPipeline) ---

    /// Positive: pipeline classifies correctly when `exe_regex` is used.
    ///
    /// Verifies that pre-compiled regex patterns work through the pipeline's
    /// [`CompiledRule`] path, not just the runtime [`matches_rule`] path.
    #[test]
    fn pipeline_exe_regex_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe_regex: Some("chrome\\.exe".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("chrome.exe", "", "", "");
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    /// Positive: `exe_regex` is case-insensitive by default through the pipeline.
    #[test]
    fn pipeline_exe_regex_case_insensitive() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe_regex: Some("CHROME\\.EXE".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("chrome.exe", "", "", "");
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    /// Negative: `exe_regex` that doesn't match falls through to default action.
    #[test]
    fn pipeline_exe_regex_mismatch_falls_through() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe_regex: Some("firefox\\.exe".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("chrome.exe", "", "", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    /// Positive: pipeline classifies correctly when `title_regex` is used.
    #[test]
    fn pipeline_title_regex_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    title_regex: Some("^Settings".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
            WindowAction::Tile,
        );
        let c = candidate("settings.exe", "Settings - Display", "", "");
        assert_eq!(p.classify(&c), WindowAction::Float);
    }

    /// Negative: `title_regex` is case-sensitive — lowercase won't match `^Settings`.
    #[test]
    fn pipeline_title_regex_case_sensitive_mismatch() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    title_regex: Some("^Settings".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
            WindowAction::Tile,
        );
        let c = candidate("settings.exe", "settings - display", "", "");
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    /// Positive: pipeline classifies correctly when `class_regex` is used.
    #[test]
    fn pipeline_class_regex_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    class_regex: Some("Chrome.*".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("chrome.exe", "", "Chrome_WidgetWin_1", "");
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    /// Negative: `class_regex` is case-sensitive — lowercase won't match `Chrome.*`.
    #[test]
    fn pipeline_class_regex_case_sensitive_mismatch() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    class_regex: Some("Chrome.*".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("chrome.exe", "", "chrome_widgetwin_1", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    /// Positive: pipeline classifies correctly when `process_path_regex` is used.
    #[test]
    fn pipeline_process_path_regex_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    process_path_regex: Some(".*\\\\Google\\\\Chrome\\\\.*".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate(
            "chrome.exe",
            "",
            "",
            "C:\\Program Files\\Google\\Chrome\\chrome.exe",
        );
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    /// Positive: `process_path_regex` is case-insensitive by default.
    #[test]
    fn pipeline_process_path_regex_case_insensitive() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    process_path_regex: Some(".*\\\\google\\\\chrome\\\\.*".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate(
            "chrome.exe",
            "",
            "",
            "C:\\Program Files\\Google\\Chrome\\chrome.exe",
        );
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    /// Negative: `process_path_regex` that doesn't match falls through.
    #[test]
    fn pipeline_process_path_regex_mismatch_falls_through() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    process_path_regex: Some(".*\\\\Firefox\\\\.*".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate(
            "chrome.exe",
            "",
            "",
            "C:\\Program Files\\Google\\Chrome\\chrome.exe",
        );
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    // --- Pipeline with invalid regex patterns (pre-compiled) ---

    /// Negative: invalid `exe_regex` pattern in pipeline is treated as non-match,
    /// not a panic. The regex is pre-compiled at pipeline construction time.
    #[test]
    fn pipeline_invalid_exe_regex_treated_as_non_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe_regex: Some("[invalid(".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("anything.exe", "", "", "");
        // Invalid regex → CompiledRegex::Invalid → non-match → falls through
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    /// Negative: invalid `title_regex` pattern in pipeline is treated as non-match.
    #[test]
    fn pipeline_invalid_title_regex_treated_as_non_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    title_regex: Some("[invalid(".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("anything.exe", "Some Title", "", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    /// Negative: invalid `class_regex` pattern in pipeline is treated as non-match.
    #[test]
    fn pipeline_invalid_class_regex_treated_as_non_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    class_regex: Some("[invalid(".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("anything.exe", "", "SomeClass", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    /// Negative: invalid `process_path_regex` pattern in pipeline is treated as non-match.
    #[test]
    fn pipeline_invalid_process_path_regex_treated_as_non_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    process_path_regex: Some("[invalid(".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("anything.exe", "", "", "C:\\path\\anything.exe");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    /// Negative: rule with ALL regex fields invalid still falls through gracefully.
    #[test]
    fn pipeline_all_invalid_regex_fields_treated_as_non_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe_regex: Some("[bad[".into()),
                    title_regex: Some("(?broken".into()),
                    class_regex: Some("[[[".into()),
                    process_path_regex: Some("*invalid".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("test.exe", "Title", "Class", "C:\\path\\test.exe");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    // --- Pipeline with mixed exact + regex fields in a single rule ---

    /// Positive: rule with both exact (`exe`) and regex (`title_regex`) fields
    /// matches when both conditions are satisfied.
    #[test]
    fn pipeline_mixed_exact_and_regex_both_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("code.exe".into()),
                    title_regex: Some(".*\\.rs - .+".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("code.exe", "main.rs - My Project", "", "");
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    /// Negative: rule with both exact and regex fields fails when exact matches
    /// but regex doesn't (AND logic).
    #[test]
    fn pipeline_mixed_exact_and_regex_regex_mismatch() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("code.exe".into()),
                    title_regex: Some(".*\\.rs - .+".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("code.exe", "settings.json - My Project", "", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    /// Negative: rule with both exact and regex fields fails when regex matches
    /// but exact doesn't (AND logic).
    #[test]
    fn pipeline_mixed_exact_and_regex_exact_mismatch() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("code.exe".into()),
                    title_regex: Some(".*\\.rs - .+".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate("other.exe", "main.rs - My Project", "", "");
        assert_eq!(p.classify(&c), WindowAction::Ignore);
    }

    /// Positive: rule combining all four regex fields with two exact fields
    /// matches when every condition is satisfied.
    #[test]
    fn pipeline_all_regex_fields_plus_exact_match() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("chrome.exe".into()),
                    title_regex: Some("New Tab.*".into()),
                    exe_regex: Some("chrome\\.exe".into()),
                    class_regex: Some("Chrome_WidgetWin_\\d+".into()),
                    process_path_regex: Some(".*\\\\Chrome\\\\.*".into()),
                    ..Default::default()
                },
                WindowAction::Tile,
            )],
            WindowAction::Ignore,
        );
        let c = candidate(
            "chrome.exe",
            "New Tab - Google Chrome",
            "Chrome_WidgetWin_1",
            "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
        );
        assert_eq!(p.classify(&c), WindowAction::Tile);
    }

    /// Positive: rule with `title_contains` (exact substring) AND `class_regex`
    /// matches when both are satisfied.
    #[test]
    fn pipeline_mixed_title_contains_and_class_regex() {
        let p = pipeline_from(
            vec![rule(
                MatchRule {
                    title_contains: Some("Visual Studio Code".into()),
                    class_regex: Some("Chrome_WidgetWin_1".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
            WindowAction::Tile,
        );
        let c = candidate(
            "code.exe",
            "main.rs - Visual Studio Code",
            "Chrome_WidgetWin_1",
            "",
        );
        assert_eq!(p.classify(&c), WindowAction::Float);
    }

    // --- Equivalence: matches_compiled_rule == matches_rule for regex fields ---

    /// Helper to compile a single rule and compare `matches_rule` vs
    /// `matches_compiled_rule`. Returns `true` if both produce the same result.
    fn check_equivalence(match_rule: &MatchRule, c: &WindowCandidate) -> bool {
        let r = rule(match_rule.clone(), WindowAction::Tile);
        let compiled_rules = compile_rules(vec![r]);
        assert_eq!(
            compiled_rules.len(),
            1,
            "compile_rules should return exactly 1 rule"
        );

        let runtime = matches_rule(c, match_rule);
        let compiled = matches_compiled_rule(c, &compiled_rules[0]);
        runtime == compiled
    }

    /// Equivalence: `exe_regex` produces identical results from both paths.
    #[test]
    fn equivalence_exe_regex_positive() {
        let mr = MatchRule {
            exe_regex: Some("chrome\\.exe".into()),
            ..Default::default()
        };
        let c = candidate("chrome.exe", "", "", "");
        assert!(check_equivalence(&mr, &c));
        assert!(matches_rule(&c, &mr), "guard: should match");
    }

    #[test]
    fn equivalence_exe_regex_negative() {
        let mr = MatchRule {
            exe_regex: Some("firefox\\.exe".into()),
            ..Default::default()
        };
        let c = candidate("chrome.exe", "", "", "");
        assert!(check_equivalence(&mr, &c));
        assert!(!matches_rule(&c, &mr), "guard: should not match");
    }

    #[test]
    fn equivalence_exe_regex_case_insensitive() {
        let mr = MatchRule {
            exe_regex: Some("CHROME\\.EXE".into()),
            ..Default::default()
        };
        let c = candidate("chrome.exe", "", "", "");
        assert!(check_equivalence(&mr, &c));
        assert!(
            matches_rule(&c, &mr),
            "guard: case-insensitive should match"
        );
    }

    /// Equivalence: `title_regex` produces identical results from both paths.
    #[test]
    fn equivalence_title_regex_positive() {
        let mr = MatchRule {
            title_regex: Some("^Settings.*".into()),
            ..Default::default()
        };
        let c = candidate("app.exe", "Settings - Display", "", "");
        assert!(check_equivalence(&mr, &c));
        assert!(matches_rule(&c, &mr), "guard: should match");
    }

    #[test]
    fn equivalence_title_regex_negative() {
        let mr = MatchRule {
            title_regex: Some("^Settings.*".into()),
            ..Default::default()
        };
        let c = candidate("app.exe", "Display - Settings", "", "");
        assert!(check_equivalence(&mr, &c));
        assert!(
            !matches_rule(&c, &mr),
            "guard: should not match (not at start)"
        );
    }

    /// Equivalence: `class_regex` produces identical results from both paths.
    #[test]
    fn equivalence_class_regex_positive() {
        let mr = MatchRule {
            class_regex: Some("Chrome_WidgetWin_\\d+".into()),
            ..Default::default()
        };
        let c = candidate("chrome.exe", "", "Chrome_WidgetWin_1", "");
        assert!(check_equivalence(&mr, &c));
        assert!(matches_rule(&c, &mr), "guard: should match");
    }

    #[test]
    fn equivalence_class_regex_negative() {
        let mr = MatchRule {
            class_regex: Some("Chrome_WidgetWin_\\d+".into()),
            ..Default::default()
        };
        let c = candidate("chrome.exe", "", "Chrome_WidgetWin_", "");
        assert!(check_equivalence(&mr, &c));
        assert!(!matches_rule(&c, &mr), "guard: should not match (no digit)");
    }

    /// Equivalence: `process_path_regex` produces identical results from both paths.
    #[test]
    fn equivalence_process_path_regex_positive() {
        let mr = MatchRule {
            process_path_regex: Some(".*\\\\Chrome\\\\.*".into()),
            ..Default::default()
        };
        let c = candidate(
            "chrome.exe",
            "",
            "",
            "C:\\Program Files\\Chrome\\chrome.exe",
        );
        assert!(check_equivalence(&mr, &c));
        assert!(matches_rule(&c, &mr), "guard: should match");
    }

    #[test]
    fn equivalence_process_path_regex_negative() {
        let mr = MatchRule {
            process_path_regex: Some(".*\\\\Chrome\\\\.*".into()),
            ..Default::default()
        };
        let c = candidate(
            "chrome.exe",
            "",
            "",
            "C:\\Program Files\\Firefox\\firefox.exe",
        );
        assert!(check_equivalence(&mr, &c));
        assert!(!matches_rule(&c, &mr), "guard: should not match");
    }

    #[test]
    fn equivalence_process_path_regex_case_insensitive() {
        let mr = MatchRule {
            process_path_regex: Some(".*\\\\chrome\\\\.*".into()),
            ..Default::default()
        };
        let c = candidate(
            "chrome.exe",
            "",
            "",
            "C:\\Program Files\\Chrome\\chrome.exe",
        );
        assert!(check_equivalence(&mr, &c));
        assert!(
            matches_rule(&c, &mr),
            "guard: case-insensitive should match"
        );
    }

    /// Equivalence: invalid regex patterns produce `false` from both paths.
    #[test]
    fn equivalence_invalid_regex_both_return_false() {
        let mr = MatchRule {
            exe_regex: Some("[invalid(".into()),
            ..Default::default()
        };
        let c = candidate("test.exe", "", "", "");
        assert!(check_equivalence(&mr, &c));
        assert!(
            !matches_rule(&c, &mr),
            "guard: invalid regex should return false"
        );
    }

    /// Equivalence: all regex fields invalid still produces same result from both paths.
    #[test]
    fn equivalence_all_invalid_regex_fields_both_return_false() {
        let mr = MatchRule {
            exe_regex: Some("[bad[".into()),
            title_regex: Some("(?broken".into()),
            class_regex: Some("[[[".into()),
            process_path_regex: Some("*invalid".into()),
            ..Default::default()
        };
        let c = candidate("test.exe", "Title", "Class", "C:\\path\\test.exe");
        assert!(check_equivalence(&mr, &c));
        assert!(!matches_rule(&c, &mr), "guard: all invalid → false");
    }

    /// Equivalence: mixed exact + regex fields produce identical results.
    #[test]
    fn equivalence_mixed_exact_and_regex_both_match() {
        let mr = MatchRule {
            exe: Some("code.exe".into()),
            title_regex: Some(".*\\.rs - .+".into()),
            ..Default::default()
        };
        let c = candidate("code.exe", "main.rs - My Project", "", "");
        assert!(check_equivalence(&mr, &c));
        assert!(matches_rule(&c, &mr), "guard: both fields match");
    }

    #[test]
    fn equivalence_mixed_exact_and_regex_partial_mismatch() {
        let mr = MatchRule {
            exe: Some("code.exe".into()),
            title_regex: Some(".*\\.rs - .+".into()),
            ..Default::default()
        };
        // exe matches but title_regex doesn't
        let c = candidate("code.exe", "settings.json - My Project", "", "");
        assert!(check_equivalence(&mr, &c));
        assert!(!matches_rule(&c, &mr), "guard: AND logic → false");
    }

    /// Equivalence: comprehensive rule with all fields specified produces
    /// identical results from both `matches_rule` and `matches_compiled_rule`.
    #[test]
    fn equivalence_comprehensive_all_fields() {
        let mr = MatchRule {
            exe: Some("chrome.exe".into()),
            exe_regex: Some("chrome\\.exe".into()),
            title: Some("New Tab - Google Chrome".into()),
            title_contains: Some("New Tab".into()),
            title_regex: Some("New Tab.*Chrome".into()),
            class: Some("Chrome_WidgetWin_1".into()),
            class_regex: Some("Chrome_WidgetWin_\\d+".into()),
            process_path: Some("C:\\Program Files\\Google\\Chrome\\chrome.exe".into()),
            process_path_regex: Some(".*\\\\Chrome\\\\.*".into()),
        };
        let c = candidate(
            "chrome.exe",
            "New Tab - Google Chrome",
            "Chrome_WidgetWin_1",
            "C:\\Program Files\\Google\\Chrome\\chrome.exe",
        );
        assert!(check_equivalence(&mr, &c));
        assert!(matches_rule(&c, &mr), "guard: all fields should match");
    }

    // --- set_user_rules (hot-reload) tests ---

    /// Positive: `set_user_rules` MUST refresh the fallback `default_action`.
    /// After reload, an unmatched candidate classifies via the NEW
    /// `default_action`, proving the field was replaced (not just the rule list).
    #[test]
    fn set_user_rules_replaces_default_action() {
        // Arrange: pipeline starts with default_action = Tile, no rules.
        let mut pipeline = pipeline_from(vec![], WindowAction::Tile);
        let unknown = candidate("unknown.exe", "", "", "");
        // Guard: initial fallback is Tile.
        assert_eq!(pipeline.classify(&unknown), WindowAction::Tile);

        // Act: hot-reload user rules with a different default_action and no rules.
        pipeline.set_user_rules(WindowRulesConfig {
            default_action: WindowAction::Float,
            rules: vec![],
        });

        // Assert: fallback now reflects the reloaded default_action.
        assert_eq!(
            pipeline.classify(&unknown),
            WindowAction::Float,
            "set_user_rules must refresh default_action"
        );
    }

    /// Positive + negative: `set_user_rules` REPLACES (not appends to) the
    /// user-rule list. After reload, a candidate matching a NEW rule classifies
    /// via it, and a candidate that matched only an OLD rule no longer matches
    /// (it falls through to `default_action`).
    #[test]
    fn set_user_rules_replaces_the_user_rule_list() {
        // Arrange: pipeline starts with one rule: "old.exe" → Ignore.
        let mut pipeline = pipeline_from(
            vec![rule(
                MatchRule {
                    exe: Some("old.exe".into()),
                    ..Default::default()
                },
                WindowAction::Ignore,
            )],
            WindowAction::Tile,
        );
        // Guard: the old rule is active before reload.
        assert_eq!(
            pipeline.classify(&candidate("old.exe", "", "", "")),
            WindowAction::Ignore
        );

        // Act: hot-reload user rules with a DIFFERENT rule: "new.exe" → Float.
        pipeline.set_user_rules(WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![rule(
                MatchRule {
                    exe: Some("new.exe".into()),
                    ..Default::default()
                },
                WindowAction::Float,
            )],
        });

        // Assert (positive): the new rule classifies "new.exe" as Float.
        assert_eq!(
            pipeline.classify(&candidate("new.exe", "", "", "")),
            WindowAction::Float,
            "set_user_rules must install the reloaded rule list"
        );
        // Assert (negative): the old rule is GONE — "old.exe" falls through.
        assert_eq!(
            pipeline.classify(&candidate("old.exe", "", "", "")),
            WindowAction::Tile,
            "set_user_rules must drop rules absent from the reload"
        );
    }
}
