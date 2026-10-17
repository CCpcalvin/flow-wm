# Declare DPI awareness via an embedded manifest

The layout engine works exclusively in physical pixels — `GetMonitorInfoW` work areas in, `SetWindowPos` rects out, pixel-valued config throughout — but the process never declared DPI awareness, so Windows virtualized every coordinate it touched and placements drifted toward the top-left on scaled displays (2560×1600 @ 150% being the motivating case). We embed a `PerMonitorV2` manifest (`flowd.exe.manifest`, compiled into the binaries via `flowd.rc` in `build.rs`) rather than calling `SetProcessDpiAwarenessContext` at startup, because a manifest is read before any code runs and survives refactors. With it, all existing pixel math becomes correct as-is — there is deliberately **no DPI conversion code anywhere**.

## Considered options

- Runtime `SetProcessDpiAwarenessContext` call: functionally equivalent, rejected as less robust (no-ops if anything sets awareness earlier).
- Treating config `*_px` values as logical pixels scaled by DPI: rejected for now — config values stay literal physical pixels. A future config mode may add logical units; until then the concept does not exist in flow-wm.
- Applying the manifest to `flowd` only: cargo links Windows resources package-wide, and both binaries share one package. The manifest therefore lands on `flow` too — harmless, since the CLI client owns no windows and no coordinate math.

## Consequences

- Monitor geometry is still read once at startup (`src/daemon/new.rs`); changing resolution or scaling while the daemon runs leaves layouts computed against stale geometry. That refresh bug (`WM_DISPLAYCHANGE`) is pre-existing, DPI-independent, and deferred.
