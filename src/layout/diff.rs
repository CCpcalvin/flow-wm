//! Layout diff computation.
//!
//! Compares two actual layouts and produces the minimal set of
//! `WindowMove` instructions with appropriate animation hints.

use crate::common::Rect;
use crate::common::WindowId;
use crate::layout::types::{ActualLayout, AnimationHint, WindowMove};

/// Threshold for classifying a move as "scroll enter/exit" vs "snap/displaced".
///
/// If a window moves more than this many pixels horizontally, it's treated
/// as a scroll transition rather than an in-viewport adjustment.
const SCROLL_THRESHOLD: i32 = 500;

/// Compute the diff between a previous and next actual layout.
///
/// Produces a `Vec<WindowMove>` containing:
/// - Moves for windows present in both layouts (position changed)
/// - Moves for new windows (from a parked position)
/// - Windows that disappeared are not included (handled by the caller)
#[must_use]
pub fn diff(prev: &ActualLayout, next: &ActualLayout) -> Vec<WindowMove> {
    let mut moves = Vec::new();

    for entry in &next.entries {
        let prev_rect = prev
            .find(entry.window_id)
            .map(|e| e.rect)
            .unwrap_or_else(|| Rect {
                // New window — treat as coming from parked position
                x: -10_000,
                y: entry.rect.y,
                width: entry.rect.width,
                height: entry.rect.height,
            });

        if prev_rect != entry.rect {
            moves.push(WindowMove {
                window_id: entry.window_id,
                from: prev_rect,
                to: entry.rect,
                hint: classify_hint(prev_rect, entry.rect),
            });
        }
    }

    moves
}

/// Classify the animation hint based on the nature of the position change.
fn classify_hint(from: Rect, to: Rect) -> AnimationHint {
    let dx = (to.x - from.x).abs();

    // Large horizontal movement indicates scroll transition
    if dx > SCROLL_THRESHOLD {
        if from.x < to.x {
            // Moving from left to right = entering viewport (or scrolling right)
            AnimationHint::ScrollEnter
        } else {
            // Moving from right to left = leaving viewport (scrolling left off-screen)
            AnimationHint::ScrollExit
        }
    } else {
        // Small movement = snap or displacement
        AnimationHint::Snap
    }
}

/// Find windows that exist in `prev` but not in `next` (removed windows).
#[must_use]
pub fn removed_windows(prev: &ActualLayout, next: &ActualLayout) -> Vec<WindowId> {
    let next_ids: std::collections::HashSet<WindowId> =
        next.entries.iter().map(|e| e.window_id).collect();
    prev.entries
        .iter()
        .filter(|e| !next_ids.contains(&e.window_id))
        .map(|e| e.window_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::WindowId;

    fn make_layout(entries: Vec<(WindowId, Rect)>) -> ActualLayout {
        ActualLayout {
            entries: entries
                .into_iter()
                .map(|(id, rect)| ActualEntry {
                    window_id: id,
                    rect,
                })
                .collect(),
        }
    }

    use crate::layout::types::ActualEntry;

    #[test]
    fn diff_no_change() {
        let layout = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let moves = diff(&layout, &layout);
        assert!(moves.is_empty());
    }

    #[test]
    fn diff_single_move() {
        let prev = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let next = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 200,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].window_id, WindowId(1));
        assert_eq!(moves[0].hint, AnimationHint::Snap);
    }

    #[test]
    fn diff_scroll_enter() {
        let prev = make_layout(vec![(
            WindowId(1),
            Rect {
                x: -10000,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let next = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 500,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].hint, AnimationHint::ScrollEnter);
    }

    #[test]
    fn diff_scroll_exit() {
        // Window moving from visible to off-screen left → ScrollExit
        let prev = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 500,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let next = make_layout(vec![(
            WindowId(1),
            Rect {
                x: -1000,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].hint, AnimationHint::ScrollExit);
    }

    #[test]
    fn diff_new_window_appears() {
        let prev = ActualLayout::new();
        let next = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 100,
                y: 100,
                width: 200,
                height: 200,
            },
        )]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 1);
        // New window from parked position is a scroll enter
        assert_eq!(moves[0].hint, AnimationHint::ScrollEnter);
    }

    #[test]
    fn removed_windows_detection() {
        let prev = make_layout(vec![
            (
                WindowId(1),
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                WindowId(2),
                Rect {
                    x: 200,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ]);
        let next = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let removed = removed_windows(&prev, &next);
        assert_eq!(removed, vec![WindowId(2)]);
    }

    // --- Integration: Diff edge cases ---

    #[test]
    fn diff_multiple_windows_moved() {
        // Positive: several windows move at once
        let prev = make_layout(vec![
            (
                WindowId(1),
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                WindowId(2),
                Rect {
                    x: 200,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ]);
        let next = make_layout(vec![
            (
                WindowId(1),
                Rect {
                    x: 50,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                WindowId(2),
                Rect {
                    x: 300,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0].window_id, WindowId(1));
        assert_eq!(moves[1].window_id, WindowId(2));
        // Both small moves → Snap hint
        assert_eq!(moves[0].hint, AnimationHint::Snap);
        assert_eq!(moves[1].hint, AnimationHint::Snap);
    }

    #[test]
    fn diff_empty_prev_to_populated_next() {
        // Positive: empty previous → first layout projection produces ScrollEnter for all
        let prev = ActualLayout::new();
        let next = make_layout(vec![
            (
                WindowId(1),
                Rect {
                    x: 16,
                    y: 16,
                    width: 200,
                    height: 200,
                },
            ),
            (
                WindowId(2),
                Rect {
                    x: 300,
                    y: 16,
                    width: 200,
                    height: 200,
                },
            ),
        ]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0].hint, AnimationHint::ScrollEnter);
        assert_eq!(moves[1].hint, AnimationHint::ScrollEnter);
    }

    #[test]
    fn diff_scroll_exit_classification() {
        // Positive: window moving far left → ScrollExit (leaving viewport)
        let prev = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 5000,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let next = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 100,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].hint, AnimationHint::ScrollExit); // large dx, moving left = exiting
    }

    #[test]
    fn diff_unchanged_window_not_included() {
        // Positive: a window at same position produces no move
        let prev = make_layout(vec![
            (
                WindowId(1),
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                WindowId(2),
                Rect {
                    x: 200,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ]);
        let next = make_layout(vec![
            (
                WindowId(1),
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                WindowId(2),
                Rect {
                    x: 200,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ]);
        let moves = diff(&prev, &next);
        assert!(moves.is_empty());
    }

    #[test]
    fn diff_removed_and_new_in_same_diff() {
        // Positive: window 1 removed, window 3 added → diff only contains move for window 3
        let prev = make_layout(vec![
            (
                WindowId(1),
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                WindowId(2),
                Rect {
                    x: 200,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ]);
        let next = make_layout(vec![
            (
                WindowId(2),
                Rect {
                    x: 200,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                WindowId(3),
                Rect {
                    x: 400,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].window_id, WindowId(3));
        // Window 1 removed: check via removed_windows
        let removed = removed_windows(&prev, &next);
        assert_eq!(removed, vec![WindowId(1)]);
    }

    #[test]
    fn removed_windows_empty_when_same() {
        // Positive: no removed windows when layouts are identical
        let layout = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let removed = removed_windows(&layout, &layout);
        assert!(removed.is_empty());
    }

    #[test]
    fn removed_windows_all_gone() {
        // Positive: all windows removed
        let prev = make_layout(vec![
            (
                WindowId(1),
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
            (
                WindowId(2),
                Rect {
                    x: 200,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
        ]);
        let next = ActualLayout::new();
        let removed = removed_windows(&prev, &next);
        assert_eq!(removed.len(), 2);
    }

    #[test]
    fn diff_move_barely_below_threshold_is_snap() {
        // Positive: move of exactly threshold-1 → Snap
        let prev = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let next = make_layout(vec![(
            WindowId(1),
            Rect {
                x: SCROLL_THRESHOLD - 1,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].hint, AnimationHint::Snap);
    }

    #[test]
    fn diff_move_beyond_threshold_is_scroll_enter() {
        // Positive: move of threshold+1 → ScrollEnter
        let prev = make_layout(vec![(
            WindowId(1),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let next = make_layout(vec![(
            WindowId(1),
            Rect {
                x: SCROLL_THRESHOLD + 1,
                y: 0,
                width: 100,
                height: 100,
            },
        )]);
        let moves = diff(&prev, &next);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].hint, AnimationHint::ScrollEnter);
    }
}
