//! Build script for ScrollingTilingManager.
//!
//! Emits a compile-time warning when building binaries for non-Windows targets.
//! Library tests (`cargo test`) are allowed on any platform since message types
//! and dispatch logic are platform-independent.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let is_windows = target.contains("windows");

    if !is_windows {
        // Only warn when building binaries, not when running tests.
        // Message types and dispatch logic are testable on any OS.
        let profile = std::env::var("PROFILE").unwrap_or_default();
        let is_test = std::env::var("OUT_DIR")
            .map(|d| d.contains("target"))
            .unwrap_or(false);

        if profile != "test" && !is_test {
            println!(
                "cargo:warning=STM is a Windows-only tiling window manager. Binaries will exit with an error on this platform."
            );
        }
    }
}
