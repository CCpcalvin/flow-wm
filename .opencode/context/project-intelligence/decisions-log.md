<!-- Context: project-intelligence/decisions | Priority: high | Version: 1.0 | Updated: 2025-01-12 -->

# Decisions Log

> Record major architectural and business decisions with full context. This prevents "why was this done?" debates.

## Quick Reference

- **Purpose**: Document decisions so future team members understand context
- **Format**: Each decision as a separate entry
- **Status**: Decided | Pending | Under Review | Deprecated

## Decision Template

```markdown
## [Decision Title]

**Date**: YYYY-MM-DD
**Status**: [Decided/Pending/Under Review/Deprecated]
**Owner**: [Who owns this decision]

### Context
[What situation prompted this decision? What was the problem or opportunity?]

### Decision
[What was decided? Be specific about the choice made.]

### Rationale
[Why this decision? What were the alternatives and why were they rejected?]

### Alternatives Considered
| Alternative | Pros | Cons | Why Rejected? |
|-------------|------|------|---------------|
| [Alt 1] | [Pros] | [Cons] | [Why not chosen] |
| [Alt 2] | [Pros] | [Cons] | [Why not chosen] |

### Impact
**Positive**: [What this enables or improves]
**Negative**: [What trade-offs or limitations this creates]
**Risk**: [What could go wrong]

### Related
- [Links to related decisions, PRs, issues, or documentation]
```

---

## Decision: [Title]

**Date**: YYYY-MM-DD
**Status**: [Status]
**Owner**: [Owner]

### Context
[What was happening? Why did we need to decide?]

### Decision
[What we decided]

### Rationale
[Why this was the right choice]

### Alternatives Considered
| Alternative | Pros | Cons | Why Rejected? |
|-------------|------|------|---------------|
| [Option A] | [Good things] | [Bad things] | [Reason] |
| [Option B] | [Good things] | [Bad things] | [Reason] |

### Impact
- **Positive**: [What we gain]
- **Negative**: [What we trade off]
- **Risk**: [What to watch for]

### Related
- [Link to PR #000]
- [Link to issue #000]
- [Link to documentation]

---

## Decision: [Title]

**Date**: YYYY-MM-DD
**Status**: [Status]
**Owner**: [Owner]

### Context
[What was happening?]

### Decision
[What we decided]

### Rationale
[Why this was right]

### Alternatives Considered
| Alternative | Pros | Cons | Why Rejected? |
|-------------|------|------|---------------|
| [Option A] | [Good things] | [Bad things] | [Reason] |

### Impact
- **Positive**: [What we gain]
- **Negative**: [What we trade off]

### Related
- [Link]

---

## Decision: Collapse center-grid / center-absolute into prefix-sum center primitives

**Date**: 2026-06-20
**Status**: Decided
**Owner**: center-logic-refactor (feat/center-logic-refactor)

### Context

The original centering implementation had two functions: `mutations::center_viewport_grid` (slot-aligned) and `mutations::center_viewport_absolute` (free-form). Both took `(num_columns, focus_col, config)` and computed positions as `f * slot` or `gap + N * slot` — they never saw `&VirtualLayout`. This was correct when every column had the same width, but once expand/shrink/drag-resize landed (allowing per-column widths), the math silently went wrong: `projection::project` and `ensure_column_visible` already used prefix sums correctly, so centering was the lone holdout producing visibly off-center results whenever any column was non-default width.

A separate `flow dispatch centergrid` command exposed the grid variant to users, but the grid/absolute distinction turned out to be a red herring: the real axis is "what should be centered" (focused column vs entire canvas vs nothing), not "is the offset quantized to slot boundaries".

### Decision

1. **Replace** `center_viewport_grid` and `center_viewport_absolute` with two prefix-sum primitives that take `&VirtualLayout`:
   - `center_viewport_on_focused(layout, focus_col, config)` — centers the focused column at the monitor midpoint. **Always** centers, even when all columns fit.
   - `center_viewport_canvas(layout, config)` — centers the entire canvas via `(canvas_width - monitor_width) / 2`. May be negative.
2. **Delete** the `flow dispatch centergrid` command, the `SocketMessage::CenterGrid` IPC variant, and the `dispatch_center_grid` handler. The single `flow dispatch center` command (now backed by `center_viewport_on_focused`) is the only user-facing center operation.
3. **Fit predicate rewrite.** Replace `columns.len() < columns_per_screen` with `canvas_width(layout, window_gap) ≤ monitor_width` in both `initialize_windows` and the move-to-workspace auto-center hook. The `columns_per_screen` config field is retained for compatibility but is no longer consulted by the centering logic.
4. **Three behaviors**, mapped to triggers:
   - Explicit user command → `center_viewport_on_focused` (always centers).
   - Automated flow + everything fits → `center_viewport_canvas`.
   - Automated flow + overflow → existing `ensure_column_visible` (no centering).
5. **Width preservation.** `MoveWindowToWorkspace` reads the moved window's `width_px` from the source layout's column (before removal) and inserts it via `insert_window_with_width`. `initialize_windows` accepts `Option<&[u32]>` of pre-captured widths (from the registry's `pre_manage_rect.width`) and quantizes each to the nearest slot-ladder rung via `quantize_to_ladder`, clamped to `[column_width, abs_max_width]`.

### Rationale

The bug was structural: any function that doesn't see the actual layout cannot center correctly once widths vary. Adding the layout as an input is the only sound fix. The grid/absolute axis was actively misleading — it described a property of the offset (quantization) that nobody actually cared about, while hiding the property users do care about (what gets centered). Collapsing to two primitives named after their *intent* makes the code self-documenting.

The "always center on explicit command" rule (even when all columns fit) matches user expectation: when someone presses the center hotkey, they want the focused column at the midpoint, full stop. The "center canvas when everything fits, else ensure-visible" rule for automated flows avoids the jarring behavior of centering a single column in the middle of an otherwise-empty monitor during routine workspace operations.

### Alternatives Considered

| Alternative | Pros | Cons | Why Rejected? |
|-------------|------|------|---------------|
| Keep both grid + absolute, just add layout parameter | Backward-compatible API | Doubles the test surface; the distinction is still meaningless once both see the layout | The grid/absolute split was the source of confusion — keeping it perpetuates the wrong mental model. |
| Mark `CenterGrid` deprecated, remove in a later release | Gentler rollout | The feature is pre-1.0 with no external consumers; zombie variants invite bugs (someone calls the deprecated one) | No real users to deprecate for; atomic removal is cleaner. |
| Add cross-monitor width clamping in MoveWindowToWorkspace | Handles edge case of widths calibrated for one monitor being wrong on another | MoveWindowToWorkspace only operates within the active monitor (same `abs_max_width`), so the case cannot arise today | Deferred until cross-monitor moves exist; premature complexity now. |
| Compute fit predicate from `columns_per_screen` * max column width | No new dependency on `projection::canvas_width` from daemon | Wrong — overestimates canvas width when columns are at base, underestimates when expanded | The actual canvas width is the only correct input. |

### Impact

- **Positive**: Centering is now correct for any combination of column widths. The mental model is simpler (one user command, one policy axis). `MoveWindowToWorkspace` preserves user-customized widths across workspaces. `initialize_windows` respects pre-existing window sizes instead of forcing every window to `column_width`.
- **Negative**: The `columns_per_screen` config field is no longer consulted by centering (only by scroll-step sizing). Users who tuned `columns_per_screen` to control centering will see different behavior — but the new behavior is more correct, so this is a bug fix disguised as a behavior change.
- **Risk**: The `quantize_to_ladder` policy at init time means a window that was 1000px wide (between rungs 0 and 1) will snap to either 960 or 1924 depending on which side of the midpoint it falls. This is intentional (matches expand/shrink rounding) but may surprise users who expect pixel-perfect preservation. Drag-resize remains the escape hatch for free-form widths.

### Related

- `docs/src/dev-guide/layout/mutations.md` — the [Center behaviors: three modes](../../docs/src/dev-guide/layout/mutations.md#center-behaviors-three-modes) section documents the new model.
- Worktree branch: `feat/center-logic-refactor`.
- Implementing PR: (to be linked after commit).

---



Decisions that were later overturned (for historical context):

| Decision | Date | Replaced By | Why |
|----------|------|-------------|-----|
| [Old decision] | [Date] | [New decision] | [Reason] |

## Onboarding Checklist

- [ ] Understand the philosophy behind major architectural choices
- [ ] Know why certain technologies were chosen over alternatives
- [ ] Understand trade-offs that were made
- [ ] Know where to find decision context when questions arise
- [ ] Understand what decisions are pending and why

## Related Files

- `technical-domain.md` - Technical implementation affected by these decisions
- `business-tech-bridge.md` - How decisions connect business and technical
- `living-notes.md` - Current open questions that may become decisions
