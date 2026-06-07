//! stmd — ScrollingTilingManager daemon
//!
//! This is the main daemon process that owns all state, manages windows,
//! and responds to IPC commands from the `stm` CLI client.
//!
//! The daemon:
//! 1. Optionally switches to a test desktop (`--desktop` flag)
//! 2. Loads configuration and creates the shared [`WindowRegistry`](scrolling_tiling_manager::registry::WindowRegistry)
//! 3. Starts a WinEvent hook thread for window lifecycle tracking
//! 4. Enters the IPC command loop, processing hook events between messages

use std::sync::{Arc, Mutex};

use clap::Parser;

use scrolling_tiling_manager::config::StmConfig;
use scrolling_tiling_manager::ipc::transport::PipeServer;
use scrolling_tiling_manager::ipc::{SocketMessage, dispatch_with_registry};
use scrolling_tiling_manager::registry::{WindowRegistry, hooks};

#[cfg(debug_assertions)]
use scrolling_tiling_manager::registry::desktop;

/// Daemon CLI arguments.
#[derive(Parser)]
#[command(name = "stmd", version, about = "ScrollingTilingManager daemon")]
#[command(propagate_version = true)]
struct Args {
    /// Desktop name for test mode (opens and switches to this desktop).
    /// Only available in debug builds.
    #[cfg(debug_assertions)]
    #[arg(long)]
    desktop: Option<String>,
}

/// Daemon entry point.
fn main() {
    env_logger::init();

    let args = Args::parse();
    if let Err(e) = run_daemon(args) {
        log::error!("stmd: fatal error: {e}");
        std::process::exit(1);
    }
}

/// Run the daemon command loop.
///
/// 1. Optionally switches to a test desktop
/// 2. Creates the shared window registry
/// 3. Scans existing windows and starts the hook thread
/// 4. Enters the IPC loop, processing hook events between each message
///
/// Returns on `Stop` command or fatal error.
fn run_daemon(args: Args) -> Result<(), String> {
    // In release builds, Args has no fields, so suppress unused-variable warning.
    let _ = &args;
    // 1. Optional: switch to test desktop (debug builds only).
    #[cfg(debug_assertions)]
    if let Some(ref desktop_name) = args.desktop {
        desktop::switch_to_desktop(desktop_name)?;
    }

    // 2. Load config (TODO: load from file).
    let config = StmConfig::default();

    // 3. Create shared registry.
    let registry = Arc::new(Mutex::new(WindowRegistry::new(&config)));

    // 4. Scan existing windows.
    registry
        .lock()
        .map_err(|e| format!("registry lock: {e}"))?
        .scan_existing_windows()?;

    // 5. Start hook thread (pass desktop name so it joins the same isolated desktop).
    #[cfg(debug_assertions)]
    let hook_desktop = args.desktop.clone();
    #[cfg(not(debug_assertions))]
    let hook_desktop = None;
    let (hook_receiver, _hook_handle) = hooks::start_hook_thread(hook_desktop)
        .map_err(|e| format!("failed to start hook thread: {e}"))?;

    // 6. IPC server loop.
    let server = PipeServer::create()
        .map_err(|e| format!("failed to create pipe (is another daemon running?): {e}"))?;

    log::info!("stmd: daemon started, listening on named pipe");

    loop {
        // Wait for a client connection.
        server
            .wait_for_client()
            .map_err(|e| format!("failed to accept client: {e}"))?;

        log::debug!("stmd: client connected");

        // Process messages from this client until they disconnect or send Stop.
        let mut should_stop = false;
        loop {
            // Process hook events before each message.
            if let Ok(mut reg) = registry.lock() {
                reg.process_pending_events(&hook_receiver);
            }

            match server.read_message() {
                Ok(msg) => {
                    let response = dispatch_with_registry(&msg, &registry);
                    let is_stop = matches!(msg, SocketMessage::Stop);

                    if let Err(e) = server.write_response(&response) {
                        log::warn!("stmd: failed to write response: {e}");
                        break;
                    }

                    if is_stop {
                        should_stop = true;
                        break;
                    }
                }
                Err(e) => {
                    // Client disconnected or read error — not fatal for the daemon.
                    log::debug!("stmd: client read error: {e}");
                    break;
                }
            }
        }

        // Disconnect the client so a new one can connect.
        if let Err(e) = server.disconnect() {
            log::warn!("stmd: failed to disconnect client: {e}");
        }

        if should_stop {
            log::info!("stmd: shutting down");
            break;
        }
    }

    Ok(())
}
