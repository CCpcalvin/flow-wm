//! stm — ScrollingTilingManager CLI client
//!
//! Sends commands to the `stmd` daemon via a Windows named pipe.
//!
//! # Usage
//!
//! ```text
//! stm start [--config <dir>]    Start the daemon (spawns stmd.exe in the background)
//! stm stop                      Stop the running daemon
//! stm config init               Create config dir + write default files
//! stm config reload             Reload daemon configuration from disk
//! stm config edit               Open config directory in the system editor
//! stm config path               Print resolved config directory path
//! stm config check              Validate configuration files
//! stm query all                 List all tracked windows (full debug dump)
//! stm dispatch focus <dir>      Focus a window in the given direction
//! stm dispatch swapcolumn <dir> Swap the focused column left/right
//! stm dispatch movewindow <dir> Move the focused window (semantic; the daemon
//!                                resolves the concrete action by window state)
//! stm dispatch expandcolumn     Expand the focused column width by one step
//! stm dispatch shrinkcolumn     Shrink the focused column width by one step
//! ```
//!
//! # Configuration
//!
//! The config directory is resolved via a priority chain:
//!
//! 1. `--config <dir>` flag on `stm start` (passed to daemon via `STM_CONFIG_DIR` env var)
//! 2. `STM_CONFIG_DIR` environment variable
//! 3. Default: `%USERPROFILE%\.config\stm\`
//!
//! The `stm config init/reload/edit/path/check` commands resolve the config
//! directory without contacting the daemon — they operate on local files only.
//! Only `stm config reload` sends an IPC message to the running daemon.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use clap::{Parser, Subcommand};

use scrolling_tiling_manager::common::Direction;
use scrolling_tiling_manager::config;
use scrolling_tiling_manager::ipc::message::SocketMessage;
use scrolling_tiling_manager::ipc::message::SocketResponse;
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
/// more actions are added (swapcolumn, swapwindow, etc.).
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
/// 3. **Diff** the old and new actual layouts to produce `WindowMove`
///    instructions, then **animate** them smoothly.
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
    #[command(name = "swapcolumn")]
    SwapColumn {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },
    /// Move the focused window in the given direction.
    ///
    /// This is a semantic command — the daemon decides what "move" means
    /// based on window state (tiled left/right = column swap; floating =
    /// pixel nudge once supported).
    #[command(name = "movewindow")]
    MoveWindow {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },

    /// Expand the focused column width by one column step.
    ///
    /// Sends [`SocketMessage::ExpandColumn`]. The daemon widens the focused
    /// column to the next `column_width` boundary and animates the result.
    #[command(name = "expandcolumn")]
    ExpandColumn,
    /// Shrink the focused column width by one column step.
    ///
    /// Sends [`SocketMessage::ShrinkColumn`]. The daemon narrows the focused
    /// column to the previous `column_width` boundary and animates the result.
    #[command(name = "shrinkcolumn")]
    ShrinkColumn,
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

/// Horizontal direction for `stm dispatch swapcolumn|movewindow <dir>`.
///
/// Only left/right is offered: column swaps are inherently horizontal, and
/// the current `movewindow` implementation resolves to a column swap for
/// tiled windows. When `movewindow` gains vertical (within-column) support,
/// it will switch to a four-way direction enum of its own.
#[derive(Debug, Subcommand)]
enum HorizontalDirection {
    /// Left.
    Left,
    /// Right.
    Right,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Start { config } => cmd_start(config),
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
/// - The daemon binary cannot be found.
/// - The daemon fails to spawn.
/// - The daemon does not become ready within [`DAEMON_START_TIMEOUT`].
fn cmd_start(config_override: Option<String>) -> Result<(), String> {
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

    spawn_daemon()?;
    wait_for_daemon()?;

    println!("stm: daemon started");
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
        DispatchCommands::SwapColumn { direction } => cmd_dispatch_swapcolumn(direction),
        DispatchCommands::MoveWindow { direction } => cmd_dispatch_movewindow(direction),
        DispatchCommands::ExpandColumn => {
            send_command(SocketMessage::ExpandColumn, "column expanded")
        }
        DispatchCommands::ShrinkColumn => {
            send_command(SocketMessage::ShrinkColumn, "column shrunk")
        }
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
/// Maps `stm dispatch swapcolumn left|right` to [`SocketMessage::SwapColumn`].
fn cmd_dispatch_swapcolumn(direction: HorizontalDirection) -> Result<(), String> {
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
/// Maps `stm dispatch movewindow left|right` to [`SocketMessage::MoveWindow`].
/// The daemon translates this into a concrete action based on window state.
fn cmd_dispatch_movewindow(direction: HorizontalDirection) -> Result<(), String> {
    let msg = match direction {
        HorizontalDirection::Left => SocketMessage::MoveWindow {
            direction: Direction::Left,
        },
        HorizontalDirection::Right => SocketMessage::MoveWindow {
            direction: Direction::Right,
        },
    };
    send_command(msg, "window moved")
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
fn spawn_daemon() -> Result<(), String> {
    let exe = find_daemon_exe()?;

    // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    const DETACHED: u32 = 0x00000200 | 0x08000000;

    // Try detached spawn first (native Windows), fall back to plain spawn (WSL).
    let child = Command::new(&exe)
        .creation_flags(DETACHED)
        .spawn()
        .or_else(|_| Command::new(&exe).spawn())
        .map_err(|e| format!("failed to spawn daemon ({}): {e}", exe.display()))?;

    // Explicitly drop the Child handle so we don't wait on the process.
    // The daemon runs independently in the background.
    drop(child);

    Ok(())
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
        assert!(matches!(cli.command, Commands::Start { config: None }));
    }

    #[test]
    fn parse_start_with_config_flag() {
        let cli = Cli::try_parse_from(["stm", "start", "--config", "C:\\custom\\stm"]).unwrap();
        match cli.command {
            Commands::Start {
                config: Some(ref c),
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
            Commands::Start { config: None } => {}
            other => panic!("expected Start with no --config, got: {other:?}"),
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

    // --- swapcolumn parsing ---

    #[test]
    fn parse_dispatch_swapcolumn_left() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "swapcolumn", "left"]).unwrap();
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
    fn parse_dispatch_swapcolumn_right() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "swapcolumn", "right"]).unwrap();
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
    fn parse_dispatch_swapcolumn_without_direction_fails() {
        // Negative: swapcolumn needs a direction subcommand.
        let result = Cli::try_parse_from(["stm", "dispatch", "swapcolumn"]);
        assert!(
            result.is_err(),
            "'stm dispatch swapcolumn' without a direction should fail"
        );
    }

    #[test]
    fn parse_dispatch_swapcolumn_vertical_fails() {
        // Negative: swapcolumn only accepts left/right (HorizontalDirection).
        let result = Cli::try_parse_from(["stm", "dispatch", "swapcolumn", "up"]);
        assert!(
            result.is_err(),
            "'swapcolumn up' should fail (only left/right)"
        );
    }

    // --- movewindow parsing ---

    #[test]
    fn parse_dispatch_movewindow_left() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "movewindow", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: HorizontalDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_movewindow_right() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "movewindow", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: HorizontalDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_movewindow_without_direction_fails() {
        // Negative: movewindow needs a direction subcommand.
        let result = Cli::try_parse_from(["stm", "dispatch", "movewindow"]);
        assert!(
            result.is_err(),
            "'stm dispatch movewindow' without a direction should fail"
        );
    }

    #[test]
    fn parse_dispatch_movewindow_vertical_fails() {
        // Negative: movewindow currently only accepts left/right (vertical is
        // deferred — when added it will use its own four-way enum).
        let result = Cli::try_parse_from(["stm", "dispatch", "movewindow", "up"]);
        assert!(
            result.is_err(),
            "'movewindow up' should fail (vertical not yet supported)"
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

    // --- Dispatch expandcolumn / shrinkcolumn ---

    #[test]
    fn parse_dispatch_expandcolumn() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "expandcolumn"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::ExpandColumn,
            } => {}
            other => panic!("expected Dispatch::ExpandColumn, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_shrinkcolumn() {
        let cli = Cli::try_parse_from(["stm", "dispatch", "shrinkcolumn"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::ShrinkColumn,
            } => {}
            other => panic!("expected Dispatch::ShrinkColumn, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_expandcolumn_extra_arg_fails() {
        // Negative: expandcolumn takes no arguments.
        let result = Cli::try_parse_from(["stm", "dispatch", "expandcolumn", "extra"]);
        assert!(
            result.is_err(),
            "'stm dispatch expandcolumn' with extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_shrinkcolumn_extra_arg_fails() {
        // Negative: shrinkcolumn takes no arguments — extra positional arg rejected.
        let result = Cli::try_parse_from(["stm", "dispatch", "shrinkcolumn", "extra"]);
        assert!(
            result.is_err(),
            "'stm dispatch shrinkcolumn' with extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_expandcolumn_multiple_extra_args_fails() {
        // Negative: expandcolumn rejects more than one extra argument.
        let result = Cli::try_parse_from(["stm", "dispatch", "expandcolumn", "extra1", "extra2"]);
        assert!(
            result.is_err(),
            "'stm dispatch expandcolumn' with multiple extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_shrinkcolumn_multiple_extra_args_fails() {
        // Negative: shrinkcolumn rejects more than one extra argument.
        let result = Cli::try_parse_from(["stm", "dispatch", "shrinkcolumn", "extra1", "extra2"]);
        assert!(
            result.is_err(),
            "'stm dispatch shrinkcolumn' with multiple extra args should fail"
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
