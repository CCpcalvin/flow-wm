# FlowWM

A scrolling tiling window manager for Windows — inspired by [niri](https://github.com/YaLTeR/niri) and the [Hyprland](https://github.com/hyprwm/Hyprland) scroll plugin. 

Windows live as **columns on a canvas** that stretches wider than your monitor. The viewport slides left and right, so you scroll between columns instead of cramming everything into a fixed grid. It feels natural on an ultrawide — especially a 32:9.

<video src="https://github.com/user-attachments/assets/80d1c69e-df6e-4c83-9364-ed86c8635f44" controls muted width="800">
  Your browser can't render embedded MP4 — <a href="https://github.com/user-attachments/assets/80d1c69e-df6e-4c83-9364-ed86c8635f44">download the demo clip</a>.
</video>

> **Status:** early, in active development — see [Getting started](#getting-started) to try it today, and the [roadmap](https://ccpcalvin.github.io/flow-wm/dev-guide/roadmap.html) for what's done and what's next.
>
> **Docs:** The full [User Guide](https://ccpcalvin.github.io/flow-wm/user-guide/index.html) and [Developer Guide](https://ccpcalvin.github.io/flow-wm/dev-guide/index.html) are also published as a searchable mdBook at <https://ccpcalvin.github.io/flow-wm/>.

## How it compares

FlowWM exists because the existing Windows tiling managers didn't scroll the way I wanted.

| | FlowWM | [GlazeWM](https://github.com/glzr-io/glazewm) | [komorebi](https://github.com/LGUG2Z/komorebi) |
|---|:---:|:---:|:---:|
| Scrolling tiling | ✅ native | ❌ | ✅ |
| Merge windows into one column | ✅ | ❌ | ❌ |
| Ultrawide-first (32:9) | ✅ | — | — |
| New windows default to… | floating | tiled | tiled |
| Learns your tile/float choices | ✅ | — | — |

**FlowWM is early access** — actively developed, with a focused goal: bring the scroll-tiling workflow from niri and Hyprland to Windows. If that's the workflow you're after, FlowWM is built for you.

## Getting started

### Install

FlowWM is Windows-only and compiles locally, so you need the **Rust MSVC toolchain** ([rustup](https://rustup.rs/)) plus the **Visual Studio "Desktop development with C++"** workload — it links against Win32, so MinGW/GNU won't work.

**From crates.io (recommended):**

```powershell
cargo install flow-wm
```

**From source:**

```powershell
git clone https://github.com/CCpcalvin/flow-wm.git
cd flow-wm
cargo build --release
```

Both paths produce `flow.exe` (the CLI you drive) and `flowd.exe` (the daemon). `cargo install` puts them on your `PATH` automatically; a source build writes them to `target\release\` — see [Building from Source](https://ccpcalvin.github.io/flow-wm/dev-guide/building-from-source.html) for adding that to `PATH`.

FlowWM also ships **no keybinder of its own** — on purpose. Every hotkey is just a `flow` CLI call, so anything that can run a command on a keypress will do. The recommended companion is [AutoHotkey v2](https://www.autohotkey.com/), and `flow config init --ahk` writes a ready-to-use `flow.ahk` for it. (See [Design Decisions → Keybindings removed](https://ccpcalvin.github.io/flow-wm/dev-guide/design-decisions.html) for the why.)

### TL;DR

**1. Install AutoHotkey v2.** Grab the installer from <https://www.autohotkey.com/> (v2.0 or newer).

**2. Generate your config and keybindings.**

```powershell
flow config init --ahk
```

This writes `%USERPROFILE%\.config\flow\flow.toml` (sensible defaults) and `flow.ahk` (a ready-to-use AutoHotkey v2 script). Existing files are never overwritten.

**3. Start the daemon and its hotkeys together.**

```powershell
flow start --ahk
```

The daemon launches in the background, and once it's listening your `flow.ahk` fires up too. (Want this on every login? Run `flow enable-autostart --ahk` once.)

**4. Everything floats — until you say so.** Open a few windows. Notice that *nothing looks tiled yet*. That's deliberate: most apps are written to float, so FlowWM floats them by default and only tiles what you tell it to. Click one window to focus it.

**5. Tile a window.** Press `Alt + T`. The focused window snaps into the scrolling column layout. Press it again to float it back. FlowWM even *remembers* your choice — next time you open that app, it tiles automatically.

**6. Drive the layout, vim-style.** Now that you have tiled columns, here's the whole move set (hold `Alt`):

| Key | What it does |
|---|---|
| `h` `j` `k` `l` | Move focus **left / down / up / right** |
| `Shift + h/j/k/l` | **Move** the focused window that way (swaps columns sideways, swaps rows within a column) |
| `Ctrl + h / l` | **Merge** the focused window into the adjacent column (stack windows in one column — the signature scroll move) |
| `Ctrl + Shift + h / l` | **Promote** the focused window into a brand-new column on that side |
| `,`  /  `.` | **Shrink** / **expand** the focused column |
| `c` | **Center** the viewport on the focused column |
| `1`…`9`, `0` | Switch to **workspace** 1…10 (`Shift` sends the focused window there) |
| `q` | **Close** the focused window |
| `p` | **Stop** the daemon (and exit the script) |

That's the whole game. FlowWM uses `Alt` by default; if it collides with an app's menu shortcuts, see [Can I use the Win key?](#can-i-use-the-win-key) below. For the complete, annotated list, see the [Command Reference](https://ccpcalvin.github.io/flow-wm/user-guide/dispatch-reference.html).

### Can I use the Win key?

FlowWM defaults to `Alt` — it's on every keyboard and matches what other Windows tiling managers (Komorebi, GlazeWM) use. The catch: `Alt+<letter>` is also the menu-shortcut prefix in many apps, so a chord like `Alt + h` can collide with an app that wanted `Alt+H` for itself. If that bothers you, swap the modifier — open `%USERPROFILE%\.config\flow\flow.ahk` and change the single `ModKey` line at the top:

```ahk
ModKey := "Alt"   ; or "ScrollLock", "CapsLock", "F13", …
                  ; full list: https://www.autohotkey.com/docs/v2/KeyList.htm
```

Change that one line and every chord follows.

A lot of people want the **Windows key** (`LWin`) instead, since it's the "super" key on every other OS. Two ways to get there:

**1. Set `ModKey := "LWin"` directly.** Simplest, and most chords work. But Windows special-cases the Win key — Start menu, `Win+D`, `Win+L`, and friends are swallowed by the OS before AutoHotkey ever sees them — so any chord that clashes with a reserved Win shortcut will misfire. This isn't AutoHotkey's fault; the key is intercepted at the system level.

**2. Hardware-remap `LWin` to a sacrificial key.** This is the zero-compromise path. Pick a key nothing else uses (`ScrollLock`, `F13`, `CapsLock`), set `ModKey` to it in `flow.ahk`, then remap `LWin → <that key>` in your keyboard's firmware (QMK / VIA / the keyboard's config tool). Physically you press `Win + h`; the OS sees `ScrollLock + h` and never triggers a reserved shortcut. The author daily-drives exactly this setup (`LWin` firmware-remapped to `ScrollLock`).

### Driving it (CLI or hotkeys)

Here's the secret: **every binding above is just a `flow` CLI call.** AutoHotkey runs `flow dispatch <command>` under the hood — which means you can drive FlowWM from any terminal, script, or tool that can run a command:

```powershell
flow dispatch focus right          # same as Alt + l
flow dispatch move-window left     # same as Alt + Shift + h
flow dispatch expand-column        # same as Alt + .
flow dispatch set-window tile      # tile the focused window directly
flow dispatch switch-workspace 2   # jump to workspace 2
flow query all                     # see what the daemon sees
flow stop                          # shut it down
```

**Don't want AutoHotkey?** No problem. Anything that runs a command on a keypress will do — for example [whkd](https://github.com/LGUG2Z/whkd) (untested by us, but the `flow dispatch` surface is tool-agnostic), or [PowerToys](https://learn.microsoft.com/windows/powertoys/) if you already run it.

**Browse every command:**

```powershell
flow dispatch help     # list every dispatch action
flow --help            # top-level commands (start, stop, config, query, ...)
```

For the full annotated reference — what each dispatch command does, its arguments, and the default hotkey it's bound to — see the [Command Reference](https://ccpcalvin.github.io/flow-wm/user-guide/dispatch-reference.html).

## Motivation

I've been working toward a keyboard-centric workflow, and along the way I tried
different tiling schemes — from dwindle to komorebi's so-called "ultrawide"
layout. I came to realize scrolling tiling makes sense for two reasons:

1. It supports a dynamic number of windows without distorting their sizes. Most
   tiling rules cram every window onto one screen, so once you open too many they
   become basically unusable — effectively capping how many a workspace can hold.
   The usual suggestion is to open a new workspace, but honestly, separating
   windows across workspaces isn't trivial.

2. Most tiling rules handle ultrawide monitors poorly — when you only have a few
   windows, they just stretch everything across the whole screen.

It was the Hyprland scroll plugin in particular that won me over, and I fell for
scrolling tiling. Then for various reasons I had to switch back to Windows, and
nothing felt the same.

I tried the obvious options:

- **GlazeWM** — a solid tiling manager, but no scrolling mode at all.
- **komorebi** — it *does* have scrolling, but I found it unstable on my setup,
  and a bit rigid: I couldn't merge windows into a shared column, which is half
  the point of the scroll model. (Komorebi is also more complex than FlowWM
  overall — this isn't a knock, just a different scope.)

So I started FlowWM to bring scrolling tiling to Windows, with a few things I'd
been missing:

### Scrolling-native, ultrawide-friendly

The layout is an infinite horizontal canvas. Columns can be any pixel width, the viewport slides smoothly, and the whole model scales comfortably to a 32:9 — which is what I daily-drive.

### Merge windows into a column

This is the niri/Hyprland move that neither GlazeWM nor komorebi gives you on Windows: drop two windows into the same column and let them stack. It's the core of why scrolling feels better than a fixed grid.

### Float by default, and remember your choices

Most apps — even on Linux — are written assuming they're free-floating windows. The usual tiling-manager approach is "tile everything, then maintain a big blacklist of apps to float." I don't like that. I always felt like I was fighting the blacklist.

FlowWM flips it: **everything floats by default.** When you decide a window should be tiled, FlowWM writes that decision to `history-flow-rules.toml`, and the next time you open that app it tiles automatically. No giant config to maintain — the program learns your preferences as you go. That's why a freshly-started FlowWM looks like it's "doing nothing": it's floating everything until you tell it otherwise.

## Contributing

Contributions are welcome — bug reports, fixes, features, docs. The best starting point is the [Developer Guide](https://ccpcalvin.github.io/flow-wm/dev-guide/index.html), which covers the architecture, the layout pipeline, the threading model, subsystem deep dives, and the design decisions behind the current shape of the code. To build FlowWM locally, see [Building from Source](https://ccpcalvin.github.io/flow-wm/dev-guide/building-from-source.html). For how to file a useful bug report or send a patch, see [Contributing](https://ccpcalvin.github.io/flow-wm/user-guide/contributing.html).

Quick references:

- [Architecture overview](https://ccpcalvin.github.io/flow-wm/dev-guide/architecture.html)
- [Layout: virtual canvas & camera model](https://ccpcalvin.github.io/flow-wm/dev-guide/layout/overview.html)
- [Classification & learned rules](https://ccpcalvin.github.io/flow-wm/dev-guide/classification.html)
- [Design decisions](https://ccpcalvin.github.io/flow-wm/dev-guide/design-decisions.html)
- [Roadmap](https://ccpcalvin.github.io/flow-wm/dev-guide/roadmap.html)

Before sending a patch, please make sure the usual gates pass:

```powershell
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

If you're working on the docs, build the mdBook with `mdbook build docs/`.

## License

Licensed under the [MIT License](LICENSE).
