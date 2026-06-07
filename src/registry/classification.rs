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
//! └─ classify_window(candidate, rules, default)
//!     │
//!     ├─ Rule 1 matches? ──► rule.action    ← first match wins
//!     ├─ Rule 2 matches? ──► rule.action
//!     ├─ ...
//!     └─ No match ──► default action         ← from config
//! ```

use crate::common::Rect;
use crate::config::types::{MatchRule, WindowAction, WindowRule};
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
/// | Field            | Match semantics                        |
/// |------------------|----------------------------------------|
/// | `exe`            | Exact, case-insensitive                |
/// | `title`          | Exact, case-sensitive                  |
/// | `title_contains` | Substring, case-insensitive            |
/// | `title_regex`    | ⚠ Not yet implemented (falls back to `title_contains`, emits warning) |
/// | `class`          | Exact, case-sensitive                  |
/// | `process_path`   | Exact, case-sensitive (glob later)     |
#[must_use]
pub fn matches_rule(candidate: &WindowCandidate, rule: &MatchRule) -> bool {
    // exe — exact, case-insensitive
    if let Some(ref exe) = rule.exe {
        if candidate.exe.eq_ignore_ascii_case(exe) {
            // matched
        } else {
            return false;
        }
    }

    // title — exact, case-sensitive
    if let Some(ref title) = rule.title
        && candidate.title != *title
    {
        return false;
    }

    // title_contains — substring, case-insensitive
    if let Some(ref substr) = rule.title_contains
        && !candidate
            .title
            .to_ascii_lowercase()
            .contains(&substr.to_ascii_lowercase())
    {
        return false;
    }

    // title_regex — not yet implemented; falls back to contains + warning
    if let Some(ref _pattern) = rule.title_regex {
        log::warn!(
            "title_regex is not yet implemented; treating as title_contains. \
             Pattern: {_pattern}"
        );
        // For now, fall back to a case-insensitive contains check
        if !candidate
            .title
            .to_ascii_lowercase()
            .contains(&_pattern.to_ascii_lowercase())
        {
            return false;
        }
    }

    // class — exact, case-sensitive
    if let Some(ref class) = rule.class
        && candidate.class != *class
    {
        return false;
    }

    // process_path — exact, case-sensitive (glob support can be added later)
    if let Some(ref path) = rule.process_path
        && candidate.process_path != *path
    {
        return false;
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
    fn case_insensitive_exe_match() {
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
    fn title_contains_case_insensitive() {
        let r = rule(
            MatchRule {
                title_contains: Some("open file".into()),
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
    fn title_contains_case_insensitive_mixed() {
        let r = rule(
            MatchRule {
                title_contains: Some("SETTINGS".into()),
                ..Default::default()
            },
            WindowAction::Float,
        );
        let c = candidate("settings.exe", "Windows Settings", "", "");
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Tile),
            WindowAction::Float
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
    fn process_path_exact() {
        let rule = MatchRule {
            process_path: Some("C:\\Windows\\System32\\calc.exe".into()),
            ..Default::default()
        };
        let c = candidate("calc.exe", "", "", "C:\\Windows\\System32\\calc.exe");
        assert!(matches_rule(&c, &rule));

        let c2 = candidate("calc.exe", "", "", "C:\\Windows\\System32\\Calc.exe");
        assert!(!matches_rule(&c2, &rule));
    }

    #[test]
    fn title_regex_fallback_to_contains() {
        let rule = MatchRule {
            title_regex: Some("settings".into()),
            ..Default::default()
        };
        let c = candidate("settings.exe", "Windows Settings", "", "");
        // Falls back to case-insensitive contains — should match
        assert!(matches_rule(&c, &rule));

        let c2 = candidate("explorer.exe", "File Explorer", "", "");
        assert!(!matches_rule(&c2, &rule));
    }

    #[test]
    fn unspecified_fields_ignored() {
        // Rule only specifies exe — empty class/title should still match
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
        // Both maximized — maximized is checked first
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
            },
            WindowAction::Float,
        );
        // Wrong process_path
        let c = candidate("app.exe", "Main Window", "AppClass", "D:\\Other\\app.exe");
        assert_eq!(
            classify_window(&c, &[r], WindowAction::Tile),
            WindowAction::Tile
        );
    }
}
