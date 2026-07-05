# FlowWM

A scrolling tiling window manager for Windows — inspired by [niri](https://github.com/YaLTeR/niri) and the [Hyprland](https://github.com/hyprwm/Hyprland) scroll plugin, built for people who'd rather stay on Windows.

Windows live as **columns on a canvas** that stretches wider than your monitor. The viewport slides left and right, so you scroll between columns instead of cramming everything into a fixed grid. It feels natural on an ultrawide — especially a 32:9.

<!-- ─────────────────────────────────────────────────────────────────────── -->
<!-- DEMO                                                                     -->
<!-- Drop a recording under docs/assets/ (e.g. demo.gif) and uncomment the    -->
<!-- line below. A good demo: 10–20s showing scroll between columns, tiling   -->
<!-- a window, and merging two windows into one column on an ultrawide.       -->
<!-- ─────────────────────────────────────────────────────────────────────── -->

<!-- ![FlowWM demo](docs/assets/demo.gif) -->

> **Status:** early, in active development. Not on `winget` / `cargo install` yet — see [Getting started](#getting-started) to try it today, and the [roadmap](docs/src/dev-guide/roadmap.md) for what's done and what's next.

## How it compares

FlowWM exists because the existing Windows tiling managers didn't scroll the way I wanted.

| | FlowWM | [GlazeWM](https://github.com/glzr-io/glazewm) | [komorebi](https://github.com/LGUG2Z/komorebi) |
|---|:---:|:---:|:---:|
| Scrolling tiling | ✅ native | ❌ | ✅ |
| Merge windows into one column | ✅ | ❌ | ❌ |
| Ultrawide-first (32:9) | ✅ | — | — |
| New windows default to… | floating | tiled | tiled |
| Learns your tile/float choices | ✅ | — | — |

**FlowWM is early access** — actively developed, with a focused goal: bring the scrolling, column-merging tiling workflow from niri and Hyprland to Windows. If that's the workflow you're after, FlowWM is built for you.

## Getting started

### Prerequisites

FlowWM is driven by hotkeys, but it ships **no built-in keybinder** — by design. You pick the hotkey daemon you like; anything that can run a command on a keypress will do:

- **[AutoHotkey](https://www.autohotkey.com/) v2** (recommended) — pairs with the bundled `flow.ahk` sample that `flow config init --ahk` generates for you.
- **[whkd](https://github.com/LGUG2Z/whkd)** — if you prefer a komorebi-style TOML hotkey config.
- **[PowerToys](https://learn.microsoft.com/windows/powertoys/)** — if you already run it.

See [Design Decisions → Keybindings removed](docs/src/dev-guide/design-decisions.md) for why FlowWM stays out of the keybinding business.

### Install

FlowWM isn't on a package manager yet. Once it is, either of these will work:

```powershell
winget install CCpcalvin.flow-wm    # planned
cargo install flow-wm               # planned
```

Until then, [build from source](docs/src/dev-guide/building-from-source.md) (needs Rust + the Visual Studio "Desktop development with C++" workload).

### First run

```powershell
flow config init          # writes default config to %USERPROFILE%\.config\flow\
flow config init --ahk    # also writes flow.ahk — a ready-to-use AutoHotkey v2 script
flow start                # launches the daemon in the background
```

Then start your hotkey daemon. If you used `--ahk`, just double-click the generated `flow.ahk` — or drop it in your startup folder so it runs on login (`Win+R` → `shell:startup` → drop the file in).

That's it — FlowWM is now running. The first thing you'll notice is that **nothing looks tiled yet**. That's on purpose (see [Motivation](#motivation) below). Hit your focus key, pick a window, and tile it — FlowWM remembers your choice for that app next time.

### Driving it (CLI or hotkeys)

Every binding in `flow.ahk` just calls the `flow` CLI under the hood. You can run the same commands yourself any time:

```powershell
flow dispatch focus right          # move focus between columns
flow dispatch expand-column        # widen the focused column
flow dispatch move-window right    # move a window across columns
flow dispatch switch-workspace 2   # jump to workspace 2
flow query all                     # see what the daemon sees
flow stop                          # shut it down
```

Run `flow --help` and `flow dispatch --help` to see the full list.

## Motivation

I used Linux for a long time — [niri](https://github.com/YaLTeR/niri) and the Hyprland scroll plugin in particular — and I fell for scrolling tiling. Then I had to move back to Windows and nothing felt the same.

I tried the obvious options:

- **GlazeWM** — a solid tiling manager, but no scrolling mode at all.
- **komorebi** — it *does* have scrolling, but I found it unstable on my setup, and a bit rigid: I couldn't merge windows into a shared column, which is half the point of the scroll model. (Komorebi is also more complex than FlowWM overall — this isn't a knock, just a different scope.)

So I started FlowWM to bring scrolling tiling to Windows, with a few things I'd been missing:

### Scrolling-native, ultrawide-friendly

The layout is an infinite horizontal canvas. Columns can be any pixel width, the viewport slides smoothly, and the whole model scales comfortably to a 32:9 — which is what I daily-drive.

### Merge windows into a column

This is the niri/Hyprland move that neither GlazeWM nor komorebi gives you on Windows: drop two windows into the same column and let them stack. It's the core of why scrolling feels better than a fixed grid.

### Float by default, and remember your choices

Most apps — even on Linux — are written assuming they're free-floating windows. The usual tiling-manager approach is "tile everything, then maintain a big blacklist of apps to float." I don't like that. I always felt like I was fighting the blacklist.

FlowWM flips it: **everything floats by default.** When you decide a window should be tiled, FlowWM writes that decision to `history-flow-rules.toml`, and the next time you open that app it tiles automatically. No giant config to maintain — the program learns your preferences as you go. That's why a freshly-started FlowWM looks like it's "doing nothing": it's floating everything until you tell it otherwise.

## Contributing

Contributions are welcome — bug reports, fixes, features, docs. The best starting point is the [Developer Guide](docs/src/dev-guide/README.md), which covers the architecture, the layout pipeline, the threading model, subsystem deep dives, and the design decisions behind the current shape of the code. To build FlowWM locally, see [Building from Source](docs/src/dev-guide/building-from-source.md).

Quick references:

- [Architecture overview](docs/src/dev-guide/architecture.md)
- [Layout: virtual canvas & camera model](docs/src/dev-guide/layout/overview.md)
- [Classification & learned rules](docs/src/dev-guide/classification.md)
- [Design decisions](docs/src/dev-guide/design-decisions.md)
- [Roadmap](docs/src/dev-guide/roadmap.md)

Before sending a patch, please make sure the usual gates pass:

```powershell
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

If you're working on the docs, build the mdBook with `mdbook build docs/` and the API docs with `cargo doc --no-deps --document-private-items`.

## License

Licensed under the [MIT License](LICENSE).
