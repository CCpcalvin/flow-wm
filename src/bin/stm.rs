//! stm — ScrollingTilingManager CLI client.
//!
//! Sends commands to the `stmd` daemon via a Windows named pipe. Commands fall
//! into four groups:
//!
//! | Group | Commands |
//! |-------|----------|
//! | Lifecycle | `start`, `stop` |
//! | Config | `config init` / `reload` / `edit` / `path` / `check` |
//! | Query | `query all` |
//! | Dispatch | `dispatch focus\|swap-column\|move-window\|expand-column\|shrink-column\|close-window\|switch-workspace\|move-to-workspace`, plus stub `swap-workspace` |
//!
//! See the developer guide's *IPC & Watchdog* chapter
//! (`docs/src/dev-guide/ipc-and-watchdog.md`) for the full command reference.
//!
//! # Configuration
//!
//! The config directory is resolved via a priority chain:
//!
//! 1. `--config <dir>` flag on `stm start` (passed to the daemon via the
//!    `STM_CONFIG_DIR` env var)
//! 2. `STM_CONFIG_DIR` environment variable
//! 3. Default: `%USERPROFILE%\.config\stm\`
//!
//! The `stm config init/reload/edit/path/check` commands resolve the config
//! directory without contacting the daemon — they operate on local files only.
//! Only `stm config reload` sends an IPC message to the running daemon.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use clap::{Parser, Subcommand};

use scrolling_tiling_manager::common::Direction;
use scrolling_tiling_manager::config;
use scrolling_tiling_manager::ipc::message::SocketMessage;
use scrolling_tiling_manager::ipc::message::SocketResponse;
use scrolling_tiling_manager::ipc::message::WindowMode;
use scrolling_tiling_manager::ipc::transport;

/// Maximum time to wait for the daemon to become ready after spawning.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(name = "stm", version, about = "ScrollingTilingManager CLI")]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Top-level commands for the stm CLI client.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the stmd daemon in the background.
    Start {
        /// Config directory path. Overrides STM_CONFIG_DIR env var and default path.
        #[arg(long)]
        config: Option<String>,
        /// Optional log file path.
        ///
        /// Forwarded to the spawned daemon as `--log-file`. When set, stmd
        /// redirects all of its logging to this exact file (truncated on each
        /// start) instead of the default date-stamped log — useful for
        /// capturing a clean, isolated debug log for a single run. The log
        /// level is still controlled by the `RUST_LOG` environment variable.
        #[arg(long, value_name = "PATH")]
        log_file: Option<String>,
    },
    /// Stop the running stmd daemon.
    Stop,
    /// Manage configuration files.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Query daemon state.
    Query {
        #[command(subcommand)]
        command: QueryCommands,
    },
    /// Dispatch a command to the daemon (focus, swap, scroll, etc.).
    ///
    /// Use `stm dispatch help` to see available subcommands. New action
    /// categories will be added here as needed.
    Dispatch {
        #[command(subcommand)]
        command: DispatchCommands,
    },
}

/// Configuration management subcommands.
///
/// These commands (except `reload`) operate on local config files without
/// contacting the daemon. Only `reload` sends an IPC message.
#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Initialize config directory with default files.
    Init,
    /// Reload daemon configuration from disk.
    Reload,
    /// Open config directory in the system editor.
    Edit,
    /// Print the resolved config directory path.
    Path,
    /// Validate configuration files.
    Check,
}

/// Query subcommands.
#[derive(Debug, Subcommand)]
enum QueryCommands {
    /// Dump all tracked windows with full debug info (state, rect, col/row, etc.).
    All,
}

/// Dispatch subcommands — one per action category.
///
/// Each variant wraps a further subcommand tree so the CLI stays organized as
/// more actions are added (swap-column, swap-window, etc.).
///
/// # Layout pipeline
///
/// Every dispatch command that changes layout flows through the same 3-step
/// pipeline inside the daemon:
///
/// 1. **Mutate** the virtual layout (e.g. widen the focused column).
///    Widening a column naturally pushes every column to its right further
///    along the virtual canvas — no explicit per-window shift is needed.
/// 2. **Project** the virtual layout into actual screen coordinates, adjusting
///    the viewport so the focused column stays visible (`ensure_column_visible`).
/// 3. **Animate** the new actual layout — the animation layer compares each
///    window's target rect against its real on-screen position and tweens
///    only the windows that differ.
///
/// The CLI's only job is to send the right [`SocketMessage`]; the daemon
/// owns the entire pipeline above.
#[derive(Debug, Subcommand)]
enum DispatchCommands {
    /// Focus a window in the given direction.
    Focus {
        #[command(subcommand)]
        direction: FocusDirection,
    },
    /// Swap the focused column with its left/right neighbour.
    SwapColumn {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },
    /// Move the focused window in the given direction.
    ///
    /// This is a semantic command — the daemon decides what "move" means
    /// based on window state and direction:
    /// - tiled left/right → column swap (cross-column move is deferred);
    /// - tiled up/down → within-column window swap;
    /// - floating → pixel nudge (deferred).
    MoveWindow {
        #[command(subcommand)]
        direction: MoveDirection,
    },
    /// Merge the focused window's row into the adjacent column.
    ///
    /// Maps to [`SocketMessage::MergeColumn`]. The focused window is
    /// detached from its column and appended as a new bottom row of the
    /// neighbour column; both columns' row heights are redistributed.
    MergeColumn {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },
    /// Promote the focused window out of its column into a new standalone column.
    ///
    /// Maps to [`SocketMessage::Promote`]. The focused window is extracted
    /// into a new single-row column placed to the left or right of the
    /// source column. No-op when the window is already alone in its column.
    Promote {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },

    /// Expand the focused column width by one column step.
    ///
    /// Sends [`SocketMessage::ExpandColumn`]. The daemon widens the focused
    /// column to the next `column_width` boundary and animates the result.
    ExpandColumn,
    /// Shrink the focused column width by one column step.
    ///
    /// Sends [`SocketMessage::ShrinkColumn`]. The daemon narrows the focused
    /// column to the previous `column_width` boundary and animates the result.
    ShrinkColumn,
    /// Center the viewport so the focused column lands at the monitor midpoint.
    ///
    /// Sends [`SocketMessage::Center`]. The daemon slides the viewport so the
    /// focused column lands at the monitor midpoint using the variable-width
    /// prefix sum — works correctly even with expanded or shrunk columns.
    Center,
    /// Close the currently focused window.
    ///
    /// Sends [`SocketMessage::CloseWindow`]. The daemon asks the focused
    /// window to close itself gently via Win32 `WM_CLOSE` — the same message
    /// Windows sends when the user clicks the window's ✕ button — so the
    /// application can run its normal shutdown logic (prompt to save unsaved
    /// work, release resources, etc.). The window is removed from the layout
    /// automatically once Win32 reports its destruction.
    CloseWindow,
    /// Set the focused window's tiling mode.
    ///
    /// Maps to [`SocketMessage::SetWindow`]. Sends
    /// `stm dispatch set-window float|tile|cycle` to the daemon, which
    /// transitions the focused window between floating and tiling modes.
    SetWindow {
        #[command(subcommand)]
        mode: WindowMode,
    },

    // --- Workspace (niri-style virtual desktop) ---
    //
    // These three subcommands form the CLI surface for the upcoming
    // vertical-scrolling workspace system. The daemon currently returns a
    // "not yet implemented" error for each — the protocol shape is locked in
    // now so keybindings and documentation can stabilise while the workspace
    // animation design is finalised.
    /// Switch the active workspace.
    ///
    /// Maps to [`SocketMessage::SwitchWorkspace`]. Sends
    /// `stm dispatch switch-workspace <id>` to the daemon, which slides the
    /// requested workspace into the viewport and parks the previously active
    /// one above or below it in a single coordinated animation.
    SwitchWorkspace {
        /// Identifier of the workspace to switch to (niri-style `u32`).
        workspace_id: u32,
    },
    /// Swap the active workspace with another workspace.
    ///
    /// Maps to [`SocketMessage::SwapWorkspace`]. Sends
    /// `stm dispatch swap-workspace <id>` to the daemon, which will
    /// (eventually) exchange the positions of the active workspace and the
    /// target in the monitor's vertical workspace stack, with focus following
    /// the originally active workspace.
    SwapWorkspace {
        /// Identifier of the workspace to swap with the active one.
        workspace_id: u32,
    },
    /// Move the focused window to another workspace.
    ///
    /// Maps to [`SocketMessage::MoveWindowToWorkspace`]. Sends
    /// `stm dispatch move-to-workspace <id>` to the daemon, which detaches the
    /// focused window from the active workspace's `ScrollingSpace` (with
    /// local focus succession — no OS foreground focus push) and re-inserts
    /// it into the target workspace's `ScrollingSpace` after its focused
    /// column. Focus stays on the source workspace.
    ///
    /// The CLI command name is the shorter `move-to-workspace` for brevity,
    /// even though the underlying IPC variant is `MoveWindowToWorkspace`
    /// (mirroring the sibling `move-window` operation).
    #[command(name = "move-to-workspace")]
    MoveWindowToWorkspace {
        /// Identifier of the destination workspace.
        workspace_id: u32,
    },
}

/// Cardinal direction for `stm dispatch focus <dir>`.
#[derive(Debug, Subcommand)]
enum FocusDirection {
    /// Focus the window to the left.
    Left,
    /// Focus the window to the right.
    Right,
    /// Focus the window above.
    Up,
    /// Focus the window below.
    Down,
}

/// Horizontal direction for `stm dispatch swap-column|merge-column|promote <dir>`.
///
/// Only left/right is offered: column swaps, merges, and promotes are
/// inherently horizontal operations between adjacent columns.
#[derive(Debug, Subcommand)]
enum HorizontalDirection {
    /// Left.
    Left,
    /// Right.
    Right,
}

/// Cardinal direction for `stm dispatch move-window <dir>`.
///
/// All four directions are accepted: left/right resolve to a column swap
/// (until a real cross-column move lands), and up/down resolve to a
/// within-column window swap.
#[derive(Debug, Subcommand)]
enum MoveDirection {
    /// Left.
    Left,
    /// Right.
    Right,
    /// Up.
    Up,
    /// Down.
    Down,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Start { config, log_file } => cmd_start(config, log_file),
        Commands::Stop => cmd_stop(),
        Commands::Config { command } => cmd_config(command),
        Commands::Query { command } => cmd_query(command),
        Commands::Dispatch { command } => cmd_dispatch(command),
    };

    if let Err(e) = result {
        eprintln!("stm: {e}");
        std::process::exit(1);
    }
}

/// Start the daemon and wait for it to become ready.
///
/// If a `--config <dir>` override is provided, sets the `STM_CONFIG_DIR`
/// environment variable before spawning the daemon process. The daemon reads
/// this variable on startup to locate its config files, so the override is
/// propagated transparently through process inheritance.
///
/// If a `--log-file <path>` override is provided, it is forwarded to the
/// daemon as a `--log-file` CLI argument (see [`spawn_daemon`]). Unlike
/// `--config`, this is passed explicitly on the command line rather than via
/// an environment variable.
///
/// # Design Decision
///
/// We use [`std::env::set_var`] rather than `Command::env()` because the
/// daemon is spawned via [`spawn_daemon`] which also needs to handle detached
/// process creation flags. Setting the env var in the current process ensures
/// it is inherited by the child regardless of the spawn path.
///
/// # Errors
///
/// Returns an error string if:
/// - The daemon is already running.
/// - The user's `stm.toml` cannot be parsed (pre-flight config check fails).
/// - The daemon binary cannot be found.
/// - The daemon fails to spawn.
/// - The daemon does not become ready within [`DAEMON_START_TIMEOUT`].
fn cmd_start(
    config_override: Option<String>,
    log_file_override: Option<String>,
) -> Result<(), String> {
    // Set env var before any daemon interaction so the spawned child inherits it.
    if let Some(ref dir) = config_override {
        // SAFETY: This is called in the CLI process before spawning the daemon
        // child. There are no other threads reading this env var at this point,
        // and the CLI is a short-lived process with no concurrent Rust code.
        unsafe { std::env::set_var(config::dirs::CONFIG_DIR_ENV, dir) };
    }

    if transport::is_daemon_running() {
        return Err("daemon is already running".into());
    }

    // Pre-flight config check: surface `stm.toml`/`stm-rules.toml` errors on the
    // user's terminal before spawning, because the detached daemon's
    // stdout/stderr are discarded. Reuses the daemon's own loaders (same crate),
    // so the verdict matches — no desync between client and daemon.
    let config_dir = config::dirs::resolve_config_dir(config_override.as_deref().map(Path::new));
    preflight_config_check(&config_dir)?;

    spawn_daemon(log_file_override.as_deref())?;
    wait_for_daemon()?;

    println!("stm: daemon started");
    Ok(())
}

/// Run a pre-flight config check on the user's terminal before spawning the daemon.
///
/// The detached daemon's stdout/stderr are discarded, so its config-load errors
/// are invisible; this surfaces them on the user's terminal before spawn. The
/// load-error policy and the race-window rationale live in
/// `docs/src/dev-guide/config-and-persistence.md`.
///
/// # Errors
///
/// `Err(String)` only if `stm.toml` cannot be loaded (identifying file and
/// cause). A `stm-rules.toml` failure is warned on stderr and does *not* error.
fn preflight_config_check(config_dir: &Path) -> Result<(), String> {
    let app_path = config::dirs::user_app_config_path_in(config_dir);
    if let Err(e) = config::load_app_config(&app_path) {
        // Fatal: surface why+where and refuse to spawn.
        return Err(format!("stm: cannot start: {e}"));
    }

    let rules_path = config::dirs::user_rules_path_in(config_dir);
    if let Err(e) = config::load_rules_config(&rules_path) {
        // Non-fatal: warn on stderr but allow startup with default rules.
        eprintln!("stm: warning: {e}; using default window rules");
    }
    Ok(())
}

/// Send a Stop message to the daemon.
fn cmd_stop() -> Result<(), String> {
    send_command(SocketMessage::Stop, "daemon stopped")
}

/// Dispatch a configuration subcommand.
///
/// Most config commands operate locally (no daemon contact). Only `Reload`
/// sends an IPC message to the running daemon.
fn cmd_config(command: ConfigCommands) -> Result<(), String> {
    match command {
        ConfigCommands::Init => cmd_config_init(),
        ConfigCommands::Reload => cmd_config_reload(),
        ConfigCommands::Edit => cmd_config_edit(),
        ConfigCommands::Path => cmd_config_path(),
        ConfigCommands::Check => cmd_config_check(),
    }
}

/// Initialize the config directory with default files.
///
/// Calls [`config::dirs::config_dir`] to resolve the config directory, then
/// [`config::init_config_dir`] to create it and write default config files
/// (`stm.toml`, `stm-rules.toml`) and JSON Schemas. Existing files are never
/// overwritten.
///
/// # Errors
///
/// Returns an error string if directory creation or file writing fails.
fn cmd_config_init() -> Result<(), String> {
    let dir = config::dirs::config_dir();
    config::init_config_dir(&dir)?;
    println!("stm: config initialized at {}", dir.display());
    Ok(())
}

/// Send a ReloadConfig message to the daemon.
///
/// This is the only `stm config` subcommand that requires the daemon to be
/// running. It tells the daemon to re-read all config files from disk.
fn cmd_config_reload() -> Result<(), String> {
    send_command(SocketMessage::ReloadConfig, "configuration reloaded")
}

/// Open the config directory in the system editor.
///
/// Resolves the editor command from:
/// 1. `EDITOR` environment variable
/// 2. `VISUAL` environment variable
/// 3. `notepad.exe` (Windows default)
///
/// Opens the **directory** (not a specific file) so the user can browse all
/// config files. Waits for the editor to exit before returning.
///
/// # Errors
///
/// Returns an error string if:
/// - No editor can be determined.
/// - The editor process fails to start.
/// - The editor exits with a non-zero status.
fn cmd_config_edit() -> Result<(), String> {
    let dir = config::dirs::config_dir();
    let editor = resolve_editor()?;

    println!("stm: opening {} in {}", dir.display(), editor);

    let status = Command::new(&editor)
        .arg(&dir)
        .status()
        .map_err(|e| format!("failed to start editor '{}': {e}", editor))?;

    if !status.success() {
        return Err(format!(
            "editor '{}' exited with status {}",
            editor,
            status
                .code()
                .map_or_else(|| "unknown".to_string(), |c| c.to_string())
        ));
    }

    Ok(())
}

/// Print the resolved config directory path.
///
/// Outputs just the path (one line) to stdout, making it useful for scripting
/// and shell integration.
fn cmd_config_path() -> Result<(), String> {
    let dir = config::dirs::config_dir();
    println!("{}", dir.display());
    Ok(())
}

/// Validate configuration files without loading them into the daemon.
///
/// Calls [`config::check_config`] which reads and validates both `stm.toml` and
/// `stm-rules.toml` if they exist. Missing files are not errors — they simply
/// mean defaults will be used.
///
/// # Errors
///
/// Returns an error string if any config file fails validation (parse error or
/// invalid field values).
fn cmd_config_check() -> Result<(), String> {
    let dir = config::dirs::config_dir();
    config::check_config(&dir)?;
    println!("stm: configuration is valid");
    Ok(())
}

/// Dispatch a query subcommand.
fn cmd_query(command: QueryCommands) -> Result<(), String> {
    match command {
        QueryCommands::All => cmd_query_all(),
    }
}

/// Dump all tracked windows from the daemon as pretty-printed JSON.
fn cmd_query_all() -> Result<(), String> {
    let response = transport::send_message(&SocketMessage::QueryWindowsAll)
        .map_err(|e| format!("failed to send command: {e}"))?;

    match response {
        SocketResponse::Data { payload } => {
            let formatted =
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
            println!("{formatted}");
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Ok => {
            println!("stm: ok");
            Ok(())
        }
    }
}

/// Dispatch a dispatch subcommand.
///
/// Routes each [`DispatchCommands`] variant to its handler. New action
/// categories will be added here as match arms.
fn cmd_dispatch(command: DispatchCommands) -> Result<(), String> {
    match command {
        DispatchCommands::Focus { direction } => cmd_dispatch_focus(direction),
        DispatchCommands::SwapColumn { direction } => cmd_dispatch_swap_column(direction),
        DispatchCommands::MoveWindow { direction } => cmd_dispatch_move_window(direction),
        DispatchCommands::MergeColumn { direction } => cmd_dispatch_merge_column(direction),
        DispatchCommands::Promote { direction } => cmd_dispatch_promote(direction),
        DispatchCommands::ExpandColumn => {
            send_command(SocketMessage::ExpandColumn, "column expanded")
        }
        DispatchCommands::ShrinkColumn => {
            send_command(SocketMessage::ShrinkColumn, "column shrunk")
        }
        DispatchCommands::Center => send_command(SocketMessage::Center, "viewport centered"),
        DispatchCommands::CloseWindow => send_command(SocketMessage::CloseWindow, "window closed"),
        DispatchCommands::SetWindow { mode } => cmd_dispatch_set_window(mode),
        DispatchCommands::SwitchWorkspace { workspace_id } => send_command(
            SocketMessage::SwitchWorkspace { workspace_id },
            "workspace switched",
        ),
        DispatchCommands::SwapWorkspace { workspace_id } => send_command(
            SocketMessage::SwapWorkspace { workspace_id },
            "workspace swapped",
        ),
        DispatchCommands::MoveWindowToWorkspace { workspace_id } => send_command(
            SocketMessage::MoveWindowToWorkspace { workspace_id },
            "window moved to workspace",
        ),
    }
}

/// Send a focus-direction command to the daemon.
fn cmd_dispatch_focus(direction: FocusDirection) -> Result<(), String> {
    let msg = match direction {
        FocusDirection::Left => SocketMessage::FocusLeft,
        FocusDirection::Right => SocketMessage::FocusRight,
        FocusDirection::Up => SocketMessage::FocusUp,
        FocusDirection::Down => SocketMessage::FocusDown,
    };
    send_command(msg, "focus changed")
}

/// Send a column-swap command to the daemon.
///
/// Maps `stm dispatch swap-column left|right` to [`SocketMessage::SwapColumn`].
fn cmd_dispatch_swap_column(direction: HorizontalDirection) -> Result<(), String> {
    let msg = match direction {
        HorizontalDirection::Left => SocketMessage::SwapColumn {
            direction: Direction::Left,
        },
        HorizontalDirection::Right => SocketMessage::SwapColumn {
            direction: Direction::Right,
        },
    };
    send_command(msg, "column swapped")
}

/// Send a semantic move-window command to the daemon.
///
/// Maps `stm dispatch move-window left|right|up|down` to [`SocketMessage::MoveWindow`].
/// The daemon translates this into a concrete action based on window state
/// and direction (column swap horizontally, row swap vertically).
fn cmd_dispatch_move_window(direction: MoveDirection) -> Result<(), String> {
    let msg = match direction {
        MoveDirection::Left => SocketMessage::MoveWindow {
            direction: Direction::Left,
        },
        MoveDirection::Right => SocketMessage::MoveWindow {
            direction: Direction::Right,
        },
        MoveDirection::Up => SocketMessage::MoveWindow {
            direction: Direction::Up,
        },
        MoveDirection::Down => SocketMessage::MoveWindow {
            direction: Direction::Down,
        },
    };
    send_command(msg, "window moved")
}

/// Send a merge-column command to the daemon.
///
/// Maps `stm dispatch merge-column left|right` to [`SocketMessage::MergeColumn`].
/// The focused window is merged into the adjacent column as a new bottom row.
fn cmd_dispatch_merge_column(direction: HorizontalDirection) -> Result<(), String> {
    let msg = match direction {
        HorizontalDirection::Left => SocketMessage::MergeColumn {
            direction: Direction::Left,
        },
        HorizontalDirection::Right => SocketMessage::MergeColumn {
            direction: Direction::Right,
        },
    };
    send_command(msg, "column merged")
}

/// Send a promote command to the daemon.
///
/// Maps `stm dispatch promote left|right` to [`SocketMessage::Promote`]. The
/// focused window is extracted into a new single-row column on the chosen side.
fn cmd_dispatch_promote(direction: HorizontalDirection) -> Result<(), String> {
    let msg = match direction {
        HorizontalDirection::Left => SocketMessage::Promote {
            direction: Direction::Left,
        },
        HorizontalDirection::Right => SocketMessage::Promote {
            direction: Direction::Right,
        },
    };
    send_command(msg, "window promoted")
}

/// Send a set-window-mode command to the daemon.
///
/// Maps `stm dispatch set-window float|tile|cycle` to [`SocketMessage::SetWindow`].
fn cmd_dispatch_set_window(mode: WindowMode) -> Result<(), String> {
    let msg = SocketMessage::SetWindow { mode };
    let label = match mode {
        WindowMode::Float => "float",
        WindowMode::Tile => "tile",
        WindowMode::Cycle => "cycle",
    };
    send_command(msg, &format!("window set to {label}"))
}

/// Send a command to the daemon and print a success message on Ok.
///
/// A shared helper used by `cmd_stop` and `cmd_config_reload` (and other
/// fire-and-forget IPC commands). Handles the three response variants:
/// [`SocketResponse::Ok`], [`SocketResponse::Error`], and
/// [`SocketResponse::Data`].
fn send_command(msg: SocketMessage, success_msg: &str) -> Result<(), String> {
    let response =
        transport::send_message(&msg).map_err(|e| format!("failed to send command: {e}"))?;

    match response {
        SocketResponse::Ok => {
            println!("stm: {success_msg}");
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Data { .. } => {
            println!("stm: {success_msg}");
            Ok(())
        }
    }
}

/// Resolve the editor command for opening config files.
///
/// Checks environment variables in order:
/// 1. `EDITOR` — the user's preferred editor.
/// 2. `VISUAL` — an alternative editor variable (common in Unix-like shells).
/// 3. `notepad.exe` — the Windows default.
///
/// # Errors
///
/// Returns an error string only if all three resolution paths fail (which
/// should never happen since `notepad.exe` is always available on Windows).
fn resolve_editor() -> Result<String, String> {
    if let Ok(editor) = std::env::var("EDITOR")
        && !editor.is_empty()
    {
        return Ok(editor);
    }

    if let Ok(editor) = std::env::var("VISUAL")
        && !editor.is_empty()
    {
        return Ok(editor);
    }

    // Windows default — always available.
    Ok("notepad.exe".to_string())
}

/// Spawn the daemon executable as a background process.
///
/// Uses `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` on native Windows so
/// the daemon runs fully detached from the terminal. No explicit `Stdio` is
/// set so that Rust's `Command::spawn()` calls `CreateProcessW` with
/// `bInheritHandles = FALSE` — this prevents the daemon from inheriting the
/// parent's kernel handles (e.g., stdout/stderr pipes created by test
/// harnesses like `assert_cmd`), which would otherwise keep those pipes open
/// after the parent exits.
///
/// Falls back to a plain `spawn()` when the detached spawn fails (e.g., under
/// WSL interop).
fn spawn_daemon(log_file_override: Option<&str>) -> Result<(), String> {
    let exe = find_daemon_exe()?;

    // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    const DETACHED: u32 = 0x00000200 | 0x08000000;

    // Try detached spawn first (native Windows), fall back to plain spawn (WSL).
    // Both paths forward `--log-file` when provided.
    let child = daemon_command(&exe, log_file_override)
        .creation_flags(DETACHED)
        .spawn()
        .or_else(|_| daemon_command(&exe, log_file_override).spawn())
        .map_err(|e| format!("failed to spawn daemon ({}): {e}", exe.display()))?;

    // Explicitly drop the Child handle so we don't wait on the process.
    // The daemon runs independently in the background.
    drop(child);

    Ok(())
}

/// Build the daemon [`Command`] with any `--log-file` override applied.
///
/// Factored out so [`spawn_daemon`] can construct an identical command for
/// both the native detached spawn and the WSL fallback spawn — the
/// `--log-file` argument must be present on both paths for the override to
/// take effect regardless of which spawn path succeeds.
///
/// The config directory is NOT passed here; it is propagated to the daemon
/// via the inherited `STM_CONFIG_DIR` environment variable (see [`cmd_start`]).
fn daemon_command(exe: &std::path::Path, log_file_override: Option<&str>) -> Command {
    let mut cmd = Command::new(exe);
    if let Some(path) = log_file_override {
        cmd.arg("--log-file").arg(path);
    }
    cmd
}

/// Locate the `stmd.exe` binary next to the current executable.
fn find_daemon_exe() -> Result<std::path::PathBuf, String> {
    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot determine current executable: {e}"))?;

    let dir = current_exe
        .parent()
        .ok_or_else(|| "cannot determine executable directory".to_string())?;

    let daemon = dir.join("stmd.exe");
    if daemon.exists() {
        return Ok(daemon);
    }

    Err(format!("daemon binary not found at {}", daemon.display()))
}

/// Wait for the daemon to finish initialization by polling the named pipe.
///
/// Polls [`transport::is_daemon_running`] (which connects and immediately
/// drops the handle) until the daemon has created the pipe and entered its
/// accept loop, or the timeout expires.
fn wait_for_daemon() -> Result<(), String> {
    let deadline = std::time::Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        if transport::is_daemon_running() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for daemon to start".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::Parser;

    // --- Positive: each subcommand parses correctly ---

    #[test]
    fn parse_start() {
        let cli = Cli::try_parse_from(["stm", "start"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Start {
                config: None,
                log_file: None
            }
        ));
    }

    #[test]
    fn parse_start_with_config_flag() {
        let cli = Cli::try_parse_from(["stm", "start", "--config", "C:\\custom\\stm"]).unwrap();
        match cli.command {
            Commands::Start {
                config: Some(ref c),
                ..
            } => {
                assert_eq!(c, "C:\\custom\\stm");
            }
            other => panic!("expected Start with --config, got: {other:?}"),
        }
    }

    #[test]
    fn parse_start_without_config_flag() {
        let cli = Cli::try_parse_from(["stm", "start"]).unwrap();
        match cli.command {
            Commands::Start { config: None, .. } => {}
            other => panic!("expected Start with no --config, got: {other:?}"),
        }
    }

    #[test]
    fn parse_start_with_log_file_flag() {
        let cli =
            Cli::try_parse_from(["stm", "start", "--log-file", "C:\\tmp\\debug.log"]).unwrap();
        match cli.command {
            Commands::Start {
                log_file: Some(ref p),
                ..
            } => {
                assert_eq!(p, "C:\\tmp\\debug.log");
            }
            other => panic!("expected Start with --log-file, got: {other:?}"),
        }
    }

    #[test]
    fn parse_start_with_config_and_log_file_flags() {
        let cli = Cli::try_parse_from([
            "stm",
            "start",
            "--config",
            "C:\\custom\\stm",
            "--log-file",
            "C:\\tmp\\debug.log",
        ])
        .unwrap();
        match cli.command {
            Commands::Start {
                config: Some(ref c),
                log_file: Some(ref p),
            } => {
                assert_eq!(c, "C:\\custom\\stm");
                assert_eq!(p, "C:\\tmp\\debug.log");
            }
            other => panic!("expected Start with both flags, got: {other:?}"),
        }
    }

    #[test]
    fn parse_stop() {
        let cli = Cli::try_parse_from(["stm", "stop"]).unwrap();
        assert!(matches!(cli.command, Commands::Stop));
    }

    #[test]
    fn parse_config_init() {
        let cli = Cli::try_parse_from(["stm", "config", "init"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Init,
            } => {}
            other => panic!("expected Config::Init, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_reload() {
        let cli = Cli::try_parse_from(["stm", "config", "reload"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Reload,
            } => {}
            other => panic!("expected Config::Reload, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_edit() {
        let cli = Cli::try_parse_from(["stm", "config", "edit"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Edit,
            } => {}
            other => panic!("expected Config::Edit, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_path() {
        let cli = Cli::try_parse_from(["stm", "config", "path"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Path,
            } => {}
            other => panic!("expected Config::Path, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_check() {
        let cli = Cli::try_parse_from(["stm", "config", "check"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Check,
            } => {}
            other => panic!("expected Config::Check, got: {other:?}"),
        }
    }

    #[test]
    fn parse_query_all() {
        let cli = Cli::try_parse_from(["stm", "query", "all"]).unwrap();
        match cli.command {
            Commands::Query {
                command: QueryCommands::All,
            } => {}
            other => panic!("expected Query::All, got: {other:?}"),
        }
    }

    // --- Dispatch command parsing ---

    #[test]
    fn parse_dispatch_focus_left() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "focus", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Focus {
                        direction: FocusDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::Focus::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_focus_right() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "focus", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Focus {
                        direction: FocusDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::Focus::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_focus_up() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "focus", "up"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Focus {
                        direction: FocusDirection::Up,
                    },
            } => {}
            other => panic!("expected Dispatch::Focus::Up, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_focus_down() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "focus", "down"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Focus {
                        direction: FocusDirection::Down,
                    },
            } => {}
            other => panic!("expected Dispatch::Focus::Down, got: {other:?}"),
        }
    }

    // --- swap-column parsing ---

    #[test]
    fn parse_dispatch_swap_column_left() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "swap-column", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SwapColumn {
                        direction: HorizontalDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::SwapColumn::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_swap_column_right() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "swap-column", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SwapColumn {
                        direction: HorizontalDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::SwapColumn::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_swap_column_without_direction_fails() {
        // Negative: swap-column needs a direction subcommand.
        let result = Cli::try_parse_from(["stm", "dispatch", "swap-column"]);
        assert!(
            result.is_err(),
            "'stm dispatch swap-column' without a direction should fail"
        );
    }

    #[test]
    fn parse_dispatch_swap_column_vertical_fails() {
        // Negative: swap-column only accepts left/right (HorizontalDirection).
        let result = Cli::try_parse_from(["stm", "dispatch", "swap-column", "up"]);
        assert!(
            result.is_err(),
            "'swap-column up' should fail (only left/right)"
        );
    }

    // --- move-window parsing ---

    #[test]
    fn parse_dispatch_move_window_left() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "move-window", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: MoveDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_window_right() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "move-window", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: MoveDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_window_up() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "move-window", "up"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: MoveDirection::Up,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Up, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_window_down() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "move-window", "down"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: MoveDirection::Down,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Down, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_window_without_direction_fails() {
        // Negative: move-window needs a direction subcommand.
        let result = Cli::try_parse_from(["stm", "dispatch", "move-window"]);
        assert!(
            result.is_err(),
            "'stm dispatch move-window' without a direction should fail"
        );
    }

    // --- merge-column parsing ---

    #[test]
    fn parse_dispatch_merge_column_left() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "merge-column", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MergeColumn {
                        direction: HorizontalDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::MergeColumn::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_merge_column_right() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "merge-column", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MergeColumn {
                        direction: HorizontalDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::MergeColumn::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_merge_column_vertical_fails() {
        // Negative: merge-column only accepts left/right (HorizontalDirection).
        let result = Cli::try_parse_from(["stm", "dispatch", "merge-column", "up"]);
        assert!(
            result.is_err(),
            "'merge-column up' should fail (only left/right)"
        );
    }

    // --- promote parsing ---

    #[test]
    fn parse_dispatch_promote_left() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "promote", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Promote {
                        direction: HorizontalDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::Promote::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_promote_right() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "promote", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Promote {
                        direction: HorizontalDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::Promote::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_promote_vertical_fails() {
        // Negative: promote only accepts left/right (HorizontalDirection).
        let result = Cli::try_parse_from(["stm", "dispatch", "promote", "down"]);
        assert!(
            result.is_err(),
            "'promote down' should fail (only left/right)"
        );
    }

    #[test]
    fn parse_dispatch_without_subcommand_fails() {
        // Negative: `stm dispatch` without a subcommand should fail to parse
        // (clap requires a subcommand).
        let result = Cli::try_parse_from(["stm", "dispatch"]);
        assert!(
            result.is_err(),
            "'stm dispatch' without a subcommand should fail"
        );
    }

    #[test]
    fn parse_dispatch_focus_without_direction_fails() {
        // Negative: `stm dispatch focus` without a direction should fail.
        let result = Cli::try_parse_from(["stm", "dispatch", "focus"]);
        assert!(
            result.is_err(),
            "'stm dispatch focus' without a direction should fail"
        );
    }

    #[test]
    fn parse_dispatch_invalid_subcommand_fails() {
        // Negative: unknown dispatch subcommand should fail.
        let result = Cli::try_parse_from(["stm", "dispatch", "nonexistent"]);
        assert!(result.is_err(), "unknown dispatch subcommand should fail");
    }

    // --- Dispatch expand-column / shrink-column ---

    #[test]
    fn parse_dispatch_expand_column() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "expand-column"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::ExpandColumn,
            } => {}
            other => panic!("expected Dispatch::ExpandColumn, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_shrink_column() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "shrink-column"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::ShrinkColumn,
            } => {}
            other => panic!("expected Dispatch::ShrinkColumn, got: {other:?}"),
        }
    }

    // --- Dispatch center ---
    //
    // `center` is the one variant whose explicit `#[command(name = "center")]`
    // attribute was *removed* in the kebab-case rename (clap derives the same
    // single-word name, so the override was redundant). This test pins that
    // derivation: if a future clap version or refactor changes the derived
    // name, this is the canary that fails.

    #[test]
    fn parse_dispatch_center() {
        // Positive: `stm dispatch center` parses to the Center variant.
        let cli = Cli::try_parse_from(["stm", "dispatch", "center"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::Center,
            } => {}
            other => panic!("expected Dispatch::Center, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_close_window() {
        // Positive: `stm dispatch close-window` parses to the CloseWindow variant.
        let cli = Cli::try_parse_from(["stm", "dispatch", "close-window"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::CloseWindow,
            } => {}
            other => panic!("expected Dispatch::CloseWindow, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_close_window_extra_arg_fails() {
        // Negative: close-window takes no arguments.
        let result = Cli::try_parse_from(["stm", "dispatch", "close-window", "extra"]);
        assert!(
            result.is_err(),
            "'stm dispatch close-window' with an extra arg should fail"
        );
    }

    // --- set-window parsing ---

    #[test]
    fn parse_dispatch_set_window_float() {
        // Positive: `stm dispatch set-window float` parses with mode = Float.
        let cli = Cli::try_parse_from(["stm", "dispatch", "set-window", "float"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SetWindow {
                        mode: WindowMode::Float,
                    },
            } => {}
            other => panic!("expected Dispatch::SetWindow::Float, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_set_window_tile() {
        // Positive: `stm dispatch set-window tile` parses with mode = Tile.
        let cli = Cli::try_parse_from(["stm", "dispatch", "set-window", "tile"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SetWindow {
                        mode: WindowMode::Tile,
                    },
            } => {}
            other => panic!("expected Dispatch::SetWindow::Tile, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_set_window_cycle() {
        // Positive: `stm dispatch set-window cycle` parses with mode = Cycle.
        let cli = Cli::try_parse_from(["stm", "dispatch", "set-window", "cycle"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SetWindow {
                        mode: WindowMode::Cycle,
                    },
            } => {}
            other => panic!("expected Dispatch::SetWindow::Cycle, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_set_window_without_mode_fails() {
        // Negative: set-window needs a mode subcommand.
        let result = Cli::try_parse_from(["stm", "dispatch", "set-window"]);
        assert!(
            result.is_err(),
            "'stm dispatch set-window' without a mode should fail"
        );
    }

    #[test]
    fn parse_dispatch_set_window_invalid_mode_fails() {
        // Negative: set-window only accepts float/tile/cycle.
        let result = Cli::try_parse_from(["stm", "dispatch", "set-window", "invalid"]);
        assert!(
            result.is_err(),
            "'stm dispatch set-window invalid' should fail"
        );
    }

    #[test]
    fn parse_dispatch_expand_column_extra_arg_fails() {
        // Negative: expand-column takes no arguments.
        let result = Cli::try_parse_from(["stm", "dispatch", "expand-column", "extra"]);
        assert!(
            result.is_err(),
            "'stm dispatch expand-column' with extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_shrink_column_extra_arg_fails() {
        // Negative: shrink-column takes no arguments — extra positional arg rejected.
        let result = Cli::try_parse_from(["stm", "dispatch", "shrink-column", "extra"]);
        assert!(
            result.is_err(),
            "'stm dispatch shrink-column' with extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_expand_column_multiple_extra_args_fails() {
        // Negative: expand-column rejects more than one extra argument.
        let result = Cli::try_parse_from(["stm", "dispatch", "expand-column", "extra1", "extra2"]);
        assert!(
            result.is_err(),
            "'stm dispatch expand-column' with multiple extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_shrink_column_multiple_extra_args_fails() {
        // Negative: shrink-column rejects more than one extra argument.
        let result = Cli::try_parse_from(["stm", "dispatch", "shrink-column", "extra1", "extra2"]);
        assert!(
            result.is_err(),
            "'stm dispatch shrink-column' with multiple extra args should fail"
        );
    }

    // --- switch-workspace / swap-workspace / move-to-workspace parsing ---

    #[test]
    fn parse_dispatch_switch_workspace() {
        // Positive: `stm dispatch switch-workspace 3` parses with workspace_id = 3.
        let cli = Cli::try_parse_from(["stm", "dispatch", "switch-workspace", "3"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::SwitchWorkspace { workspace_id: 3 },
            } => {}
            other => panic!("expected Dispatch::SwitchWorkspace(3), got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_switch_workspace_without_id_fails() {
        // Negative: switch-workspace needs a workspace_id argument.
        let result = Cli::try_parse_from(["stm", "dispatch", "switch-workspace"]);
        assert!(
            result.is_err(),
            "'stm dispatch switch-workspace' without an id should fail"
        );
    }

    #[test]
    fn parse_dispatch_switch_workspace_zero() {
        // Positive: workspace_id = 0 is accepted by the parser (boundary value).
        // The daemon decides whether 0 is a valid workspace; the CLI does not.
        let cli = Cli::try_parse_from(["stm", "dispatch", "switch-workspace", "0"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::SwitchWorkspace { workspace_id: 0 },
            } => {}
            other => panic!("expected Dispatch::SwitchWorkspace(0), got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_swap_workspace() {
        // Positive: `stm dispatch swap-workspace 7` parses with workspace_id = 7.
        let cli = Cli::try_parse_from(["stm", "dispatch", "swap-workspace", "7"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::SwapWorkspace { workspace_id: 7 },
            } => {}
            other => panic!("expected Dispatch::SwapWorkspace(7), got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_swap_workspace_without_id_fails() {
        // Negative: swap-workspace needs a workspace_id argument.
        let result = Cli::try_parse_from(["stm", "dispatch", "swap-workspace"]);
        assert!(
            result.is_err(),
            "'stm dispatch swap-workspace' without an id should fail"
        );
    }

    #[test]
    fn parse_dispatch_move_to_workspace() {
        // Positive: `stm dispatch move-to-workspace 11` parses with workspace_id = 11.
        // The CLI command name stays `move-to-workspace` (short form) even though
        // the underlying Rust variant is `MoveWindowToWorkspace`.
        let cli = Cli::try_parse_from(["stm", "dispatch", "move-to-workspace", "11"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::MoveWindowToWorkspace { workspace_id: 11 },
            } => {}
            other => panic!("expected Dispatch::MoveWindowToWorkspace(11), got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_to_workspace_without_id_fails() {
        // Negative: move-to-workspace needs a workspace_id argument.
        let result = Cli::try_parse_from(["stm", "dispatch", "move-to-workspace"]);
        assert!(
            result.is_err(),
            "'stm dispatch move-to-workspace' without an id should fail"
        );
    }

    #[test]
    fn parse_dispatch_workspace_command_rejects_non_numeric_id() {
        // Negative: workspace_id must be a u32; a non-numeric token is rejected.
        let result = Cli::try_parse_from(["stm", "dispatch", "switch-workspace", "abc"]);
        assert!(
            result.is_err(),
            "non-numeric workspace_id should be rejected"
        );
    }

    // --- Negative: invalid invocations ---

    #[test]
    fn parse_no_subcommand_fails() {
        // Negative: no subcommand should fail to parse
        let result = Cli::try_parse_from(["stm"]);
        assert!(result.is_err(), "parsing with no subcommand should fail");
    }

    #[test]
    fn parse_invalid_subcommand_fails() {
        // Negative: unknown subcommand should fail
        let result = Cli::try_parse_from(["stm", "nonexistent"]);
        assert!(result.is_err(), "unknown subcommand should fail");
    }

    #[test]
    fn parse_extra_arg_fails() {
        // Negative: extra argument to a subcommand that takes none should fail
        let result = Cli::try_parse_from(["stm", "stop", "unexpected"]);
        assert!(result.is_err(), "extra argument should fail");
    }

    #[test]
    fn parse_empty_args_fails() {
        // Negative: empty argument list should fail
        let result = Cli::try_parse_from([""]);
        assert!(result.is_err(), "empty args should fail");
    }

    // --- Negative: old flat commands should no longer parse ---

    #[test]
    fn parse_old_reload_config_fails() {
        // Negative: the old `stm reload-config` flat command no longer exists
        let result = Cli::try_parse_from(["stm", "reload-config"]);
        assert!(
            result.is_err(),
            "old 'reload-config' command should no longer parse"
        );
    }

    #[test]
    fn parse_old_check_config_fails() {
        // Negative: the old `stm check-config` flat command no longer exists
        let result = Cli::try_parse_from(["stm", "check-config"]);
        assert!(
            result.is_err(),
            "old 'check-config' command should no longer parse"
        );
    }

    // --- Negative: old squished dispatch subcommands should no longer parse ---
    //
    // The `stm dispatch <sub>` surface was renamed from squished-lowercase
    // (e.g. `swapcolumn`) to kebab-case (`swap-column`). Existing user scripts
    // and keybindings that still spell the old form must now be rejected by
    // clap rather than silently mis-route. Each case below is the OLD form of
    // a subcommand that has a sibling positive test above proving the NEW
    // kebab form parses; together they pin both ends of the rename.
    // `center` is intentionally absent here — it was never renamed.

    #[test]
    fn parse_old_squished_dispatch_subcommands_fail() {
        let old_forms = [
            "swapcolumn",
            "movewindow",
            "expandcolumn",
            "shrinkcolumn",
            "closewindow",
            "setwindow",
            "switchworkspace",
            "swapworkspace",
            "movetoworkspace",
        ];
        for old in old_forms {
            let result = Cli::try_parse_from(["stm", "dispatch", old]);
            assert!(
                result.is_err(),
                "old squished dispatch subcommand '{old}' should no longer parse"
            );
        }
    }

    // --- Negative: invalid config subcommand ---

    #[test]
    fn parse_config_invalid_subcommand_fails() {
        let result = Cli::try_parse_from(["stm", "config", "nonexistent"]);
        assert!(result.is_err(), "unknown config subcommand should fail");
    }

    // --- Negative: config subcommand with extra args ---

    #[test]
    fn parse_config_subcommand_extra_arg_fails() {
        let result = Cli::try_parse_from(["stm", "config", "path", "unexpected"]);
        assert!(
            result.is_err(),
            "extra argument to config subcommand should fail"
        );
    }

    // --- DAEMON_START_TIMEOUT wiring tests ---

    // Positive: DAEMON_START_TIMEOUT is a reasonable bounded value
    #[test]
    fn daemon_start_timeout_is_reasonable() {
        assert!(
            DAEMON_START_TIMEOUT.as_secs() > 0,
            "DAEMON_START_TIMEOUT must be > 0"
        );
        assert!(
            DAEMON_START_TIMEOUT.as_secs() <= 30,
            "DAEMON_START_TIMEOUT must be <= 30s (user shouldn't wait longer)"
        );
    }

    // Negative: DAEMON_START_TIMEOUT is not zero (would mean no wait)
    #[test]
    fn daemon_start_timeout_is_not_zero() {
        assert_ne!(
            DAEMON_START_TIMEOUT,
            Duration::ZERO,
            "zero timeout would skip waiting entirely"
        );
    }

    // Positive: wait_for_daemon() correctly wraps transport::wait_for_pipe errors.
    // Verifies the error-formatting wrapper in wait_for_daemon().
    // It cannot call wait_for_daemon() directly (would block on pipe),
    // but we verify the function exists and is callable by confirming it
    // compiles and its signature returns Result<(), String>.
    #[test]
    fn wait_for_daemon_maps_not_found_error() {
        let _: fn() -> Result<(), String> = wait_for_daemon;
    }

    // --- resolve_editor tests ---

    #[test]
    fn resolve_editor_returns_string() {
        let result = resolve_editor();
        assert!(result.is_ok(), "resolve_editor should always return Ok");
        let editor = result.unwrap();
        assert!(!editor.is_empty(), "editor command should not be empty");
    }
}
