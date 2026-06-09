//! stmd — ScrollingTilingManager daemon
//!
//! This is the main daemon process that owns all state, manages windows,
//! and responds to IPC commands from the `stm` CLI client.
//!
//! The daemon:
//! 1. Optionally switches to a test desktop (`--desktop` flag, debug only)
//! 2. Loads configuration from `stm.yml` and `stm-rules.yml`
//! 3. Constructs a [`ScrollTilingManager`](scrolling_tiling_manager::daemon::ScrollTilingManager)
//!    which performs all initialization (window scan, layout init, hook setup)
//! 4. Enters the IPC event loop via `stm.run()`

use std::path::Path;

use clap::Parser;

use scrolling_tiling_manager::config::{
    dirs, init_config_dir, load_app_config, load_default_rules, load_rules_config,
};
use scrolling_tiling_manager::daemon::ScrollTilingManager;

#[cfg(debug_assertions)]
use scrolling_tiling_manager::registry::desktop;

/// Daemon CLI arguments.
#[derive(Parser)]
#[command(name = "stmd", version, about = "ScrollingTilingManager daemon")]
#[command(propagate_version = true)]
struct Args {
    /// Config directory path. Overrides `STM_CONFIG_DIR` env var and default path.
    /// Usually set by `stm start --config` which passes it via `STM_CONFIG_DIR`.
    #[arg(long)]
    config: Option<String>,

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
    if let Err(e) = run(args) {
        log::error!("stmd: fatal error: {e}");
        std::process::exit(1);
    }
}

/// Run the daemon: load config, build STM, run event loop.
///
/// This function is the thin orchestration layer between CLI argument
/// parsing and the [`ScrollTilingManager`] constructor. All actual daemon
/// logic lives inside `ScrollTilingManager`.
///
/// # Steps
///
/// 1. Optionally switch to a test desktop (debug builds only).
/// 2. Resolve and initialize the config directory.
/// 3. Load application config and window rules.
/// 4. Construct [`ScrollTilingManager`] (performs all subsystem init).
/// 5. Call `stm.run()` to enter the IPC event loop.
///
/// Returns on `Stop` command or fatal error.
fn run(args: Args) -> Result<(), String> {
    // 1. Optional: switch to test desktop (debug builds only).
    #[cfg(debug_assertions)]
    if let Some(ref name) = args.desktop {
        desktop::switch_to_desktop(name)?;
    }

    // 2. Resolve config directory using the priority chain:
    //    --config flag > STM_CONFIG_DIR env var > ~/.config/stm/ default.
    let config_dir = dirs::resolve_config_dir(args.config.as_deref().map(Path::new));
    log::info!("using config directory: {}", config_dir.display());

    // Ensure default config files exist (idempotent — safe to call every startup).
    init_config_dir(&config_dir)?;

    // 3. Load configuration.
    let app_config = load_app_config(&dirs::user_app_config_path_in(&config_dir));
    let user_rules = load_rules_config(&dirs::user_rules_path_in(&config_dir));
    let default_rules = load_default_rules();

    // 4. Resolve desktop name for hook thread (debug builds pass it through;
    //    release builds always use None).
    #[cfg(debug_assertions)]
    let desktop_name = args.desktop.clone();
    #[cfg(not(debug_assertions))]
    let desktop_name = None;

    // 5. Build and run — STM owns everything from here.
    let mut stm = ScrollTilingManager::new(
        app_config,
        user_rules,
        default_rules,
        config_dir,
        desktop_name,
    )?;
    stm.run();

    Ok(())
}
