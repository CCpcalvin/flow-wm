//! Daemon logging initialization — date-stamped log file plus stderr tee.
//!
//! ## Why this module exists
//!
//! The daemon already uses the `log` facade everywhere (100+ call sites), and
//! `env_logger::init()` is called at startup. **The problem is that
//! `env_logger` writes only to stderr by default**, and the daemon is spawned
//! by `stm start` with `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` — a fully
//! detached process whose stderr goes nowhere. So every log line the code
//! already emits silently vanishes, which is why debugging feels blind.
//!
//! This module replaces bare `env_logger::init()` with a configured logger
//! that writes to **two** targets simultaneously:
//!
//! 1. **A date-stamped file** — `<config_dir>/logs/stmd-YYYY-MM-DD.log`,
//!    opened in append mode. One file per day; multiple daemon restarts on
//!    the same day accumulate into one file. All historical logs are
//!    preserved (no automatic rotation or deletion).
//! 2. **stderr** — so that running `stmd` directly from a console (rather
//!    than via `stm start`) still shows logs inline.
//!
//! The date-stamped file (target 1) can be overridden by passing
//! `stmd --log-file PATH`, which redirects all logging to that exact path
//! (truncated on each start) for a clean, isolated single-run debug log.
//! See [`init`].
//!
//! ## Why `chrono` instead of the standard library
//!
//! [`std::time::SystemTime::now`] returns UTC, and the standard library has no
//! way to determine the local timezone offset on Windows (no `TZ` semantics).
//! Log filenames should reflect the user's *local* calendar day so that
//! "today's log file" matches their intuition. [`chrono::Local::now`] handles
//! local time correctly across platforms, so we use it for the single date
//! computation at startup.
//!
//! ## Level policy
//!
//! - **Debug builds**: default level is `debug`.
//! - **Release builds**: default level is `info`.
//! - **Override**: set the `RUST_LOG` environment variable (e.g.
//!   `RUST_LOG=trace`, `RUST_LOG=scrolling_tiling_manager=debug`) before
//!   starting the daemon. This takes precedence over the build-profile
//!   default.
//!
//! ## Date rollover
//!
//! The date is computed **once at daemon startup** and the file handle is held
//! for the daemon's entire lifetime. If the daemon runs across midnight, it
//! keeps writing to the startup-day's file until restarted. This is a
//! deliberate trade-off: it avoids the complexity of a background rotation
//! thread while keeping the common case (daily daemon restarts) correct.
//!
//! # Errors during initialization
//!
//! If the log file cannot be opened (e.g. the config directory is read-only),
//! initialization falls back to a **stderr-only** logger and emits a warning.
//! The daemon must never fail to start solely because logging is unavailable.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use chrono::Local;
use env_logger::{Builder, Target};

/// Initialize the global logger.
///
/// Configures [`env_logger`] to write every record to both a log file and
/// stderr (via [`TeeWriter`]).
///
/// This function must be called **exactly once** per process — it calls
/// [`Builder::init`], which panics if a global logger is already installed.
/// In the daemon this is the first thing `main()` does.
///
/// # Log file selection
///
/// - **`log_file_override = Some(path)`** (from `stmd --log-file PATH`):
///   redirect ALL logging to that exact path, opened fresh
///   (create-or-truncate). This yields a clean, isolated file for a single
///   debugging run, separate from the shared daily log. Stderr is still
///   echoed so a console-launched `stmd --log-file` stays visible.
/// - **`log_file_override = None`** (default): the date-stamped daily log
///   (`<config_dir>/logs/stmd-YYYY-MM-DD.log`) opened in append mode, so
///   restarts on the same day accumulate into one file.
///
/// # Level resolution
///
/// 1. If `RUST_LOG` is set, its directives are used (standard `env_logger`
///    behavior — module-specific filters, etc.).
/// 2. Otherwise, the build-profile default applies: `debug` in debug builds,
///    `info` in release builds.
///
/// # File-open failure
///
/// If the log file cannot be created or opened, the logger is initialized
/// with a stderr-only target and a warning is emitted. This ensures the
/// daemon can still run and produce diagnostic output even when the log
/// directory is inaccessible.
pub fn init(log_file_override: Option<&Path>) {
    let default_filter = if cfg!(debug_assertions) {
        "debug"
    } else {
        "info"
    };
    let env = env_logger::Env::default().default_filter_or(default_filter);

    // Resolve the log file path and open mode from the override (if any).
    let (log_path, append) = match log_file_override {
        Some(path) => {
            // Explicit `--log-file`: a clean, isolated file for this run.
            (std::path::PathBuf::from(path), false)
        }
        None => {
            // Default: date-stamped daily log, appended to across restarts.
            let date = today_local_date();
            let config_dir = crate::config::dirs::resolve_config_dir(None);
            let path = crate::config::dirs::log_file_path_in(&config_dir, &date);
            (path, true)
        }
    };

    match open_log_file(&log_path, append) {
        Ok(file) => {
            // Primary path: tee to both file and stderr.
            let mut builder = Builder::from_env(env);
            builder.target(Target::Pipe(Box::new(TeeWriter::new(file))));
            builder.init();
            log::info!("stmd: logging to {}", log_path.display());
        }
        Err(e) => {
            // Fallback: stderr-only. Initialize the logger BEFORE emitting the
            // warning so the warning is actually captured by the logger.
            Builder::from_env(env).init();
            log::warn!(
                "stmd: could not open log file {}: {e}; falling back to stderr-only",
                log_path.display()
            );
        }
    }
}

/// Return today's local date as a `YYYY-MM-DD` string.
///
/// Uses [`chrono::Local`] to get the wall-clock date in the user's timezone.
/// This is computed once at daemon startup and used to build the log filename.
///
/// # Examples
///
/// The format is always zero-padded:
///
/// ```ignore
/// // Returns e.g. "2026-06-17" on June 17, 2026.
/// let date = today_local_date();
/// assert_eq!(date.len(), 10); // YYYY-MM-DD
/// ```
fn today_local_date() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Open (or create) the log file, creating parent dirs.
///
/// The `append` flag selects the open mode:
///
/// - `append = true` — opened with `.append(true).create(true)`. If the file
///   exists, new records are appended to the end; if not, it is created.
///   Existing contents are never truncated. This is used for the default
///   date-stamped daily log, where multiple daemon restarts on the same day
///   accumulate into one file.
/// - `append = false` — opened with `.write(true).create(true).truncate(true)`.
///   If the file exists it is **truncated to zero length**; if not, it is
///   created. This yields a fresh, isolated file for a single debugging run
///   via `stmd --log-file PATH`.
///
/// The parent directory tree is created if missing via
/// [`std::fs::create_dir_all`].
///
/// # Errors
///
/// Returns [`io::Error`] if directory creation or file opening fails.
fn open_log_file(path: &Path, append: bool) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).write(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    opts.open(path)
}

/// A writer that duplicates every write to both a file and stderr.
///
/// This is the bridge between [`env_logger`] (which writes via the [`Write`]
/// trait) and our two-target output strategy. Each [`write`] call sends the
/// bytes to the log file first (the primary, durable target), then to stderr
/// (best-effort — errors are silently ignored because a failing stderr must
/// never prevent the file write from succeeding).
///
/// The file is always written first so that even if stderr is broken, the
/// log record is preserved on disk. Stderr failures are swallowed because
/// stderr is a debugging convenience, not a correctness requirement.
///
/// `TeeWriter` is `Send` (because [`File`] is `Send`), satisfying
/// [`env_logger::Target::Pipe`]'s `Box<dyn Write + Send + 'static>` bound.
struct TeeWriter {
    /// The append-mode log file. Writes go here first.
    file: File,
}

impl TeeWriter {
    /// Wrap a [`File`] so that writes are also echoed to stderr.
    fn new(file: File) -> Self {
        Self { file }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Primary: write to the durable log file.
        self.file.write_all(buf)?;
        // Secondary: echo to stderr. Errors here are non-fatal — a broken
        // stderr pipe must not prevent the file write (already done above)
        // from being reported as successful to env_logger.
        let _ = io::stderr().write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let _ = io::stderr().flush();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Positive: `today_local_date` returns a `YYYY-MM-DD` formatted string.
    ///
    /// We validate the format structurally (not the specific date, which
    /// depends on when the test runs) by checking the length and that each
    /// component parses as digits.
    #[test]
    fn today_local_date_is_iso_format() {
        let date = today_local_date();
        let parts: Vec<&str> = date.split('-').collect();
        assert_eq!(parts.len(), 3, "expected YYYY-MM-DD, got: {date}");
        assert_eq!(parts[0].len(), 4, "year should be 4 digits: {date}");
        assert_eq!(parts[1].len(), 2, "month should be 2 digits: {date}");
        assert_eq!(parts[2].len(), 2, "day should be 2 digits: {date}");

        // Year should be a plausible recent year (guards against garbage).
        let year: u32 = parts[0]
            .parse()
            .unwrap_or_else(|_| panic!("year should be numeric: {date}"));
        assert!(
            (2024..=2100).contains(&year),
            "year should be plausible: {date}"
        );
    }

    /// Positive: `TeeWriter` writes the full payload to the file.
    ///
    /// We cannot easily verify the stderr side of the tee (it would require
    /// capturing process stderr), but we can confirm the file receives every
    /// byte, which is the primary durable target.
    #[test]
    fn tee_writer_writes_payload_to_file() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let path = temp.path().join("tee-test.log");
        let file = File::create(&path).expect("file creation failed");

        let mut tee = TeeWriter::new(file);
        let payload = b"hello log line\nsecond line\n";
        tee.write_all(payload).expect("write_all failed");
        tee.flush().expect("flush failed");
        drop(tee); // close the file handle

        let contents = std::fs::read_to_string(&path).expect("read back failed");
        assert_eq!(contents.as_bytes(), payload);
    }

    /// Positive: `TeeWriter` reports the correct byte count.
    ///
    /// The [`Write::write`] contract requires returning the number of bytes
    /// written. Our impl writes all bytes via `write_all` and reports the full
    /// length, so callers (env_logger) see an accurate count.
    #[test]
    fn tee_writer_reports_byte_count() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let path = temp.path().join("tee-count.log");
        let file = File::create(&path).expect("file creation failed");

        let mut tee = TeeWriter::new(file);
        let payload = b"exactly 13 bytes"; // 15 bytes
        let written = tee.write(payload).expect("write failed");
        assert_eq!(written, payload.len(), "should report all bytes written");
    }

    /// Positive: `open_log_file` creates the parent directory tree.
    ///
    /// Passing a path with non-existent parent directories should succeed:
    /// the function calls `create_dir_all` before opening the file.
    #[test]
    fn open_log_file_creates_parent_dirs() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let nested = temp.path().join("a").join("b").join("logs");
        let file_path = nested.join("stmd-test.log");

        // The returned handle is intentionally unused: the test only verifies
        // that `open_log_file` creates the file and its parent directories.
        let _file = open_log_file(&file_path, true).expect("open_log_file failed");
        assert!(
            file_path.exists(),
            "log file should exist after open_log_file"
        );
        assert!(
            nested.exists(),
            "parent directory tree should have been created"
        );
    }

    /// Positive: `open_log_file` appends to an existing file rather than
    /// truncating it.
    ///
    /// This is the core invariant of the date-based append strategy: a second
    /// open of the same path preserves prior contents and adds to them.
    #[test]
    fn open_log_file_appends_without_truncating() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let path = temp.path().join("append-test.log");

        // First open + write.
        {
            let mut f = open_log_file(&path, true).expect("first open failed");
            f.write_all(b"first line\n").expect("first write failed");
            f.flush().expect("first flush failed");
        }

        // Second open + write — must NOT destroy "first line".
        {
            let mut f = open_log_file(&path, true).expect("second open failed");
            f.write_all(b"second line\n").expect("second write failed");
            f.flush().expect("second flush failed");
        }

        let contents = std::fs::read_to_string(&path).expect("read back failed");
        assert_eq!(contents, "first line\nsecond line\n");
    }

    /// Positive: `open_log_file` with `append = false` truncates an existing
    /// file to a clean slate.
    ///
    /// This is the `--log-file` override behaviour: each launch of
    /// `stmd --log-file PATH` must start from an empty file so the captured
    /// log contains only that single run's output.
    #[test]
    fn open_log_file_truncates_when_override() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let path = temp.path().join("override-test.log");

        // Pre-create the file with stale content from a previous run.
        std::fs::write(&path, "leftover from a previous debug run\n").expect("pre-write failed");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back failed"),
            "leftover from a previous debug run\n"
        );

        // Open in override (truncate) mode — existing content must be wiped.
        let _file = open_log_file(&path, false).expect("open in override mode failed");

        let contents = std::fs::read_to_string(&path).expect("read back failed");
        assert!(
            contents.is_empty(),
            "override mode should truncate the file to empty, got: {contents}"
        );
    }
}
