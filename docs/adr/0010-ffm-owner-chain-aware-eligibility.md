# FFM owner-chain aware eligibility — owned popups survive the pointer sweep

Date: 2026-08-10

## Status

Accepted — successor to [ADR-0009 — FFM active-workspace eligibility + animation suppression](0009-ffm-active-workspace-and-animation-suppression.md).

## Context

When an application opens an **owned popup** — a top-level window it owns, such as Chrome's download-history panel, an omnibox dropdown, or a dialog — the user moves the cursor from the trigger (the toolbar button) toward the popup, and the popup vanishes before they can click anything in it.

Root cause: FFM's eligibility check compared the candidate only to the *literal* OS foreground. While the popup holds the foreground, its owner is "not the foreground," so the owner is an eligible FFM target. A 25 ms dwell later, FFM calls `SetForegroundWindow` on the owner; the popup loses activation (`WM_ACTIVATE`) and the app hides it. The user can never reach the popup's contents with the pointer.

This is structurally the same class of bug ADR-0009 closed for the workspace-switch flicker: an OS fact the literal-foreground clause cannot see (there, a workspace membership; here, an ownership relationship) leaks into FFM targeting. The fix follows the same shape — a new boolean on the resolver-built candidate snapshot, a new clause in the pure `ffm_target_eligible` predicate, and a new OS lookup confined to the resolver.

## Decision

Add an **owner-chain aware** clause to the pure FFM target-eligibility predicate: a candidate that **owns the current foreground** is ineligible. Concretely, the candidate is ineligible when the OS foreground's root owner (`GetAncestor(GA_ROOTOWNER)`) equals the candidate window.

This clause **extends** the existing literal-foreground clause — the two compose without a gap. When the foreground is a tracked window, `root owner(foreground) == foreground`, so the two clauses agree; the new clause additionally covers the un-tracked owned-transient case the literal clause cannot see (the popup itself is never in the registry, so the literal-foreground clause never gets a chance to reject its owner — only the owner-chain lookup, which walks *from* the foreground, can connect the two). The literal-foreground clause is retained as load-bearing for the fail-open / OS-quirk path (see Consequences).

### Shape

- **New snapshot field.** `FfmCandidate::owns_foreground: bool` — one new boolean on the resolver-built candidate snapshot, mirroring the existing per-clause field pattern (each eligibility clause maps 1:1 to one snapshot field). The pure predicate consumes it with no Win32 access.
- **New Win32 helper.** `registry::win32::foreground_root_owner()` — a small wrapper around `GetForegroundWindow` + `GetAncestor(GA_ROOTOWNER)`, beside the existing `WindowFromPoint` → `GA_ROOT` walker. **Fail-open**: a null foreground or null root owner returns `None`, which the resolver treats as "the candidate does not own the foreground" (eligible), matching the existing fail-open posture of the FFM wiring.
- **OS-fact gathering stays in the resolver.** No OS access leaks into the pure predicate. The resolver runs on the main IPC thread, same as today; no `Arc<Mutex>` is introduced.

### Unconditional

The behavior is unconditional — no new `flow.toml` field is added. Owned popups simply stop being dismissed by FFM by default.

## Accepted behavioral boundaries

Two boundaries are accepted as deliberate, not bugs:

1. **Only the owner is shielded.** Moving the pointer to a *different* managed window while a transient is up still lets FFM fire and may dismiss the transient. This is acceptable because the user has genuinely switched attention — the cursor is no longer on the path to the popup.

2. **Un-owned / composited framework transients slip through.** Some Electron / Chromium-composited menus have no Win32 owner (`GA_ROOTOWNER` returns themselves, or the framework hosts them in a separate top-level window with no owner handle). The owner-chain clause cannot connect them to their app, so they are not shielded. This is accepted as a residual gap, revisited only if reported.

## Considered Options

- **Un-tracked-foreground suppression** ("any time the foreground is not a tracked window, freeze FFM entirely"). Rejected. It freezes the whole screen for any foreign-app foreground — including the common "desktop has focus" case — breaking user stories 3 and 4 of the parent spec (#31). The owner-chain clause is precise: it shields only the owner whose popup is up, and stays fully active when the desktop or a foreign app holds the foreground.

- **Transient-of-managed intersection** ("suppress only when the foreground is an owned transient whose owner is tracked"). Rejected as a separate clause. It is *strictly weaker* than the owner-chain clause: the owner-chain lookup already produces `root_owner(foreground) == candidate`, and the candidate being tracked is enforced by clause 1 of the predicate (state = `None` ⇒ ineligible). Adding a separate "is the foreground an owned transient" check would duplicate the rule and require an extra OS fact (the foreground's owned-ness) for no behavioral gain.

- **Inject a Win32 / foreground backend trait** to make the resolver hermetically testable. Out of scope, per the parent spec. The resolver and the new wrapper stay Win32-coupled with no `cfg(test)` injection point, consistent with the documented posture of the hover wiring and the registry Win32 wrappers. The pure predicate remains the sole test seam.

- **Shield via a global "any popup is up" flag.** Rejected. FFM has no global registry of popups (they are un-tracked by design), and inventing one would re-introduce shared state and concurrency concerns the single-threaded model deliberately avoids. The owner-chain lookup is per-poll, stateless, and main-thread-only.

## Consequences

- **Owned popups survive a pointer sweep** from their trigger toward them — the load-bearing fix. The 25 ms dwell never re-focuses the owner, so the popup keeps activation and stays open.
- **FFM is fully active when the desktop or a foreign app holds the foreground** — the owner-chain lookup returns a handle the candidate cannot equal, so `owns_foreground` is false and the candidate is eligible (no over-suppression).
- **The literal-foreground clause is retained, not removed.** It is still load-bearing for the case where `GetAncestor(GA_ROOTOWNER)` fails to return the candidate (e.g. the fail-open path, or a future OS quirk) — defense in depth. The two clauses compose without a gap.
- **All OS lookups stay in the resolver.** The pure `ffm_target_eligible` predicate remains Win32-free; every clause of FFM eligibility stays independently unit-testable.
- **No new configuration, no new concurrency.** Single-threaded ownership model preserved; behavior is on by default.
- **Residual gap: un-owned / composited framework transients** are not shielded. Accepted; revisited only if reported.

## Testing

Hermetic unit tests in the pure predicate's test module cover every clause composition:

- managed candidate owns foreground ⇒ ineligible (the load-bearing case);
- managed candidate does not own foreground ⇒ eligible (happy path preserved);
- un-tracked candidate (state = `None`) ineligible regardless of the ownership flag (defense in depth);
- ownership clause composes with the workspace clause (off-active-workspace owner ineligible by either);
- ownership and literal-foreground clauses agree when the foreground is the candidate itself.

A signature-pinning test (`foreground_root_owner_has_option_isize_return_type`) mirrors the existing `get_shell_window` / `reconcile_foreground_win32_deps_have_documented_signatures` pattern, guarding the helper against an upstream signature change. Driving a real owned transient on the isolated `--desktop` test desktop is not deterministic, so an integration test for this path is out of scope.
