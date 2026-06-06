//! stmd — ScrollingTilingManager daemon
//!
//! This is the main daemon process that owns all state, manages windows,
//! and responds to IPC commands from the `stm` CLI client.
//!
//! # Current state
//!
//! Phase 1 MVP: the daemon creates a named pipe server and handles `Stop`
//! commands. Window management and layout will be added in later phases.

#[cfg(target_os = "windows")]
use scrolling_tiling_manager::ipc::{SocketMessage, dispatch};

/// Daemon entry point.
///
/// Initializes the named pipe server and enters the command loop.
/// The loop accepts one client connection at a time, reads commands,
/// and dispatches them until a `Stop` message is received.
fn main() {
    env_logger::init();

    #[cfg(target_os = "windows")]
    {
        if let Err(e) = run_daemon() {
            log::error!("stmd: fatal error: {e}");
            std::process::exit(1);
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        log::error!("stmd: this daemon only runs on Windows");
        std::process::exit(1);
    }
}

/// Run the daemon command loop.
///
/// Creates the named pipe, then repeatedly:
/// 1. Waits for a client to connect
/// 2. Reads and dispatches messages
/// 3. Disconnects the client
///
/// Returns on `Stop` command or fatal error.
#[cfg(target_os = "windows")]
fn run_daemon() -> Result<(), String> {
    use scrolling_tiling_manager::ipc::transport::PipeServer;

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
            match server.read_message() {
                Ok(msg) => {
                    let response = dispatch(&msg);
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
