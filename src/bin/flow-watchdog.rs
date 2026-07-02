//! flow-watchdog — crash recovery helper
//!
//! Spawned by flowd as a child process. If the daemon exits unexpectedly,
//! the watchdog restores windows from the recovery snapshot.

fn main() {
    // TODO: Parse --parent-pid and --recovery-path args, poll parent, recover on exit
    println!("flow-watchdog: not yet implemented");
}
