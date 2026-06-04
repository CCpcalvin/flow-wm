//! stm-watchdog — crash recovery helper
//!
//! Spawned by stmd as a child process. If the daemon exits unexpectedly,
//! the watchdog restores windows from the recovery snapshot.

fn main() {
    // TODO: Parse --parent-pid and --recovery-path args, poll parent, recover on exit
    println!("stm-watchdog: not yet implemented");
}
