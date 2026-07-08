//! Daemon autostart and AutoHotkey lifecycle.
//!
//! Two related concerns live here:
//!
//! - **AHK lifecycle** — `flow start --ahk` launches the user's `flow.ahk`
//!   keybinding script via the shell's `.ahk` file association, and `flow stop
//!   --ahk` terminates it. A pidfile records the launched PID so `stop` can
//!   kill exactly that process.
//! - **Login autostart** — `flow enable-autostart [--ahk]` writes a single
//!   `flow.lnk` into the user's `shell:startup` folder whose target is
//!   `flow.exe start [--ahk]`. Login therefore reuses the interactive startup
//!   path's pre-flight config check and double-start guard. `flow
//!   disable-autostart` removes the shortcut. Both are idempotent.
//!
//! See (`docs/src/dev-guide/ipc-and-watchdog.md`) for the command reference.

use std::ffi::OsStr;
use std::fs;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IPersistFile,
};
use windows::Win32::System::Threading::{
    GetProcessId, OpenProcess, PROCESS_TERMINATE, TerminateProcess,
};
use windows::Win32::UI::Shell::{
    IShellLinkW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{Interface, PCWSTR};

use crate::config::dirs::config_dir;

/// Filename of the autostart shortcut inside the `shell:startup` folder.
const FLOW_LNK: &str = "flow.lnk";

/// Resolve the user's `shell:startup` folder.
///
/// This is `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`. Windows
/// launches every `.lnk` (or executable) in this folder at logon.
///
/// # Errors
///
/// Returns an error string if `%APPDATA%` is not set (broken environment).
pub fn startup_dir() -> Result<PathBuf, String> {
    let appdata = std::env::var("APPDATA")
        .map_err(|_| "%APPDATA% is not set; cannot resolve shell:startup".to_string())?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup"))
}

/// Report returned by [`enable_autostart`] / [`disable_autostart`].
#[derive(Debug, Clone)]
pub struct AutostartReport {
    /// Path to the single `flow.lnk` shortcut that was created or removed.
    pub shortcut: PathBuf,
}

/// Create (or overwrite) the login autostart shortcut.
///
/// Writes a single `flow.lnk` into [`startup_dir`] whose target is `flow.exe`
/// with arguments `start` (or `start --ahk` when `ahk` is true), working
/// directory set to the executable's folder, and show-cmd `SW_HIDE` so the
/// short-lived `flow.exe` console never flashes at login. Because the shortcut
/// runs the real CLI, login reuses the interactive path's pre-flight config
/// check and double-start guard — there is no parallel startup code path.
///
/// When `ahk` is true, the user's `flow.ahk` must already exist (write it with
/// `flow config init --ahk`); this is verified up-front so a missing script is
/// reported clearly rather than only failing at login.
///
/// # Errors
///
/// Returns an error string if:
/// - `ahk` is true and `flow.ahk` does not exist in the config directory.
/// - `flow.exe`'s path cannot be determined.
/// - The shortcut cannot be created (COM failure).
pub fn enable_autostart(ahk: bool) -> Result<AutostartReport, String> {
    if ahk {
        let script = flow_ahk_path();
        if !script.exists() {
            return Err(format!(
                "flow.ahk not found at {}; run `flow config init --ahk` to create it first",
                script.display()
            ));
        }
    }

    let exe = flow_exe_path()?;
    let working_dir = exe.parent().map(Path::to_path_buf);
    let args = if ahk { "start --ahk" } else { "start" };

    let link = shortcut_path()?;
    create_shortcut(&link, &exe, Some(args), working_dir.as_deref())?;
    Ok(AutostartReport { shortcut: link })
}

/// Remove the login autostart shortcut.
///
/// Idempotent: returns `Ok` if the shortcut does not exist (treats missing as
/// success), so calling this twice is never an error.
///
/// # Errors
///
/// Returns an error string only on filesystem errors other than `NotFound`.
pub fn disable_autostart() -> Result<AutostartReport, String> {
    let link = shortcut_path()?;
    match fs::remove_file(&link) {
        Ok(()) => Ok(AutostartReport { shortcut: link }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(AutostartReport { shortcut: link })
        }
        Err(e) => Err(format!("failed to remove {}: {e}", link.display())),
    }
}

/// Launch the user's `flow.ahk` via the shell's `.ahk` file association.
///
/// Uses `ShellExecuteExW` with verb `"open"` so Windows resolves the installed
/// AutoHotkey interpreter — no install-path probing. The launched process
/// handle is read (via `SEE_MASK_NOCLOSEPROCESS`) and converted to a PID with
/// [`GetProcessId`], then written to a pidfile so [`stop_ahk_script`] can
/// terminate exactly this process later.
///
/// # Returns
///
/// The launched AutoHotkey process ID on success.
///
/// # Errors
///
/// Returns an error string if:
/// - `flow.ahk` does not exist in the config directory.
/// - The shell association launch fails (e.g. AutoHotkey is not installed).
pub fn spawn_ahk_script() -> Result<u32, String> {
    let script = flow_ahk_path();
    if !script.exists() {
        return Err(format!(
            "flow.ahk not found at {}; run `flow config init --ahk` to create it",
            script.display()
        ));
    }

    let script_w = Wide::from_os(script.as_os_str());
    let verb_w = Wide::from_str("open");

    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb_w.pcwstr(),
        lpFile: script_w.pcwstr(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };

    // SAFETY: ShellExecuteExW fills info.hProcess on success.
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| format!("ShellExecuteExW failed: {e}"))?;
    }

    if info.hProcess.is_invalid() {
        return Err(
            "ShellExecuteExW returned no process handle (is AutoHotkey installed?)".to_string(),
        );
    }

    // SAFETY: hProcess is valid (checked above) and owned by us until closed.
    let pid = unsafe { GetProcessId(info.hProcess) };
    // SAFETY: release the handle Windows lent us; the outcome is irrelevant
    // because we already have the PID.
    let _ = unsafe { CloseHandle(info.hProcess) };

    if pid == 0 {
        return Err("GetProcessId returned 0 after launching flow.ahk".to_string());
    }

    let pidfile = ahk_pidfile_path();
    if let Err(e) = fs::write(&pidfile, pid.to_string()) {
        log::warn!("failed to write AHK pidfile {}: {e}", pidfile.display());
    }
    Ok(pid)
}

/// Terminate the AutoHotkey process launched by [`spawn_ahk_script`].
///
/// Reads the PID from the pidfile, kills that process via
/// [`TerminateProcess`], and removes the pidfile regardless of outcome.
///
/// # Returns
///
/// `Ok(true)` if a process was killed, `Ok(false)` if there was no pidfile
/// (nothing to stop) or the pidfile was unreadable/stale.
///
/// # Errors
///
/// Returns an error string on filesystem errors reading the pidfile other
/// than `NotFound`, or on `TerminateProcess` failure.
pub fn stop_ahk_script() -> Result<bool, String> {
    let pidfile = ahk_pidfile_path();
    let pid_str = match fs::read_to_string(&pidfile) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("failed to read {}: {e}", pidfile.display())),
    };

    let result = match pid_str.trim().parse::<u32>() {
        Ok(pid) => kill_process(pid),
        Err(_) => Ok(false), // Corrupt pidfile — nothing to stop.
    };

    // Always remove the pidfile; either the process is gone, or it was stale.
    if let Err(e) = fs::remove_file(&pidfile)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("failed to remove AHK pidfile {}: {e}", pidfile.display());
    }
    result
}

// ── Path helpers ───────────────────────────────────────────────────────────

/// Path to the single autostart shortcut inside [`startup_dir`].
fn shortcut_path() -> Result<PathBuf, String> {
    Ok(startup_dir()?.join(FLOW_LNK))
}

/// Path to the running `flow.exe` (this binary).
///
/// Returns `current_exe()` directly — no existence check, since the OS already
/// proved the path is valid by starting this process. The only failure mode is
/// `current_exe()` itself failing (extremely rare).
///
/// # Errors
///
/// Returns an error string only if `current_exe()` fails.
fn flow_exe_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot determine current executable: {e}"))
}

/// Path to the user's `flow.ahk` inside the config directory.
fn flow_ahk_path() -> PathBuf {
    config_dir().join("flow.ahk")
}

/// Path to the pidfile recording the launched AutoHotkey PID.
fn ahk_pidfile_path() -> PathBuf {
    config_dir().join("flow.ahk.pid")
}

// ── Win32: process kill ────────────────────────────────────────────────────

/// Terminate a process by PID.
///
/// # Returns
///
/// `Ok(true)` if the process was terminated, `Ok(false)` if the PID does not
/// refer to a running process (stale pidfile).
///
/// # Errors
///
/// Returns an error string if `TerminateProcess` fails for a live process.
fn kill_process(pid: u32) -> Result<bool, String> {
    // SAFETY: OpenProcess with PROCESS_TERMINATE only queries/terminates the
    // named PID; a stale PID either fails (Ok(false)) or targets a process the
    // user already has permission to terminate.
    let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
        Ok(h) => h,
        Err(_) => return Ok(false), // Process not running (stale pidfile).
    };

    let result = unsafe { TerminateProcess(handle, 1) }
        .map(|()| true)
        .map_err(|e| format!("TerminateProcess failed: {e}"));

    // Always release the handle regardless of the kill outcome.
    let _ = unsafe { CloseHandle(handle) };
    result
}

// ── Win32: COM shortcut creation ───────────────────────────────────────────

/// Owned wide-string backing for `PCWSTR` borrows.
///
/// Keeps the `Vec<u16>` (null-terminated) alive so the borrowed `PCWSTR`
/// remains valid as long as `self` is held. The borrow through `&self`
/// enforces the lifetime at the call site.
struct Wide {
    buf: Vec<u16>,
}

impl Wide {
    /// Encode an `OsStr` to a null-terminated UTF-16 buffer.
    fn from_os(s: &OsStr) -> Self {
        Self {
            buf: s.encode_wide().chain(std::iter::once(0)).collect(),
        }
    }

    /// Encode a `&str` to a null-terminated UTF-16 buffer.
    fn from_str(s: &str) -> Self {
        Self::from_os(OsStr::new(s))
    }

    /// Borrow the buffer as a `PCWSTR`.
    fn pcwstr(&self) -> PCWSTR {
        PCWSTR::from_raw(self.buf.as_ptr())
    }
}

/// Create or overwrite a Windows `.lnk` shortcut via COM.
///
/// Initializes COM for the current thread, builds an `IShellLinkW` with the
/// supplied target/arguments/working-directory, persists it via
/// `IPersistFile::Save`, and uninitializes COM.
///
/// # Arguments
///
/// * `link_path` — Where to write the `.lnk`.
/// * `target` — What the shortcut runs (an executable).
/// * `args` — Optional command-line arguments.
/// * `working_dir` — Optional working directory.
///
/// # Errors
///
/// Returns an error string on any COM/HRESULT failure.
///
/// # Safety
///
/// `CoInitializeEx` / `CoUninitialize` are paired. Each COM method below is
/// `pub unsafe fn` in `windows-0.62.2`, so each call has its own minimal
/// `unsafe` scope. `Wide` buffers outlive every `PCWSTR` borrow. `S_FALSE`
/// (already-initialized) is a success HRESULT, so `is_err()` treats it as Ok.
fn create_shortcut(
    link_path: &Path,
    target: &Path,
    args: Option<&str>,
    working_dir: Option<&Path>,
) -> Result<(), String> {
    let target_w = Wide::from_os(target.as_os_str());
    let args_w = args.map(Wide::from_str);
    let work_w = working_dir.map(|p| Wide::from_os(p.as_os_str()));
    let link_w = Wide::from_os(link_path.as_os_str());

    // SAFETY: CoInitializeEx on this thread; S_FALSE (already-initialized) is
    // a success code so is_err() treats it as Ok.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(format!("CoInitializeEx failed: {hr}"));
    }

    let result = (|| -> windows::core::Result<()> {
        // SAFETY: CoCreateInstance on ShellLink CLSID is the documented
        // constructor for IShellLinkW.
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };

        // SAFETY: target_w outlives this call.
        unsafe { link.SetPath(target_w.pcwstr())? };
        if let Some(a) = &args_w {
            // SAFETY: a outlives this call.
            unsafe { link.SetArguments(a.pcwstr())? };
        }
        if let Some(w) = &work_w {
            // SAFETY: w outlives this call.
            unsafe { link.SetWorkingDirectory(w.pcwstr())? };
        }
        // SAFETY: SW_HIDE suppresses the short-lived console at login.
        unsafe { link.SetShowCmd(SW_HIDE)? };

        let persist: IPersistFile = link.cast()?;
        // SAFETY: link_w outlives this call; `true` overwrites an existing .lnk.
        unsafe { persist.Save(link_w.pcwstr(), true)? };
        Ok(())
    })();

    // SAFETY: pairs with CoInitializeEx above; always runs so COM is torn down
    // even on the error path.
    unsafe { CoUninitialize() };

    result.map_err(|e| format!("failed to create shortcut: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate `APPDATA` / `FLOW_CONFIG_DIR`. See the
    /// identical pattern in `src/config/dirs.rs`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Capture the original values of `APPDATA` and `FLOW_CONFIG_DIR` so a test
    /// can restore them on drop.
    struct EnvGuard {
        appdata: Option<String>,
        config_dir: Option<String>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            Self {
                appdata: std::env::var("APPDATA").ok(),
                config_dir: std::env::var("FLOW_CONFIG_DIR").ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: serialized by ENV_LOCK; tests are short-lived and run
            // single-threaded within the lock.
            match self.appdata.take() {
                Some(v) => unsafe { std::env::set_var("APPDATA", v) },
                None => unsafe { std::env::remove_var("APPDATA") },
            }
            match self.config_dir.take() {
                Some(v) => unsafe { std::env::set_var("FLOW_CONFIG_DIR", v) },
                None => unsafe { std::env::remove_var("FLOW_CONFIG_DIR") },
            }
        }
    }

    /// Build a tempdir tree and point both `%APPDATA%` and `FLOW_CONFIG_DIR`
    /// at it, returning `(startup_dir, config_dir)`.
    ///
    /// The caller must hold `ENV_LOCK` for the duration.
    fn make_startup_tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let appdata = tmp.path().join("AppData");
        let startup = appdata
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup");
        fs::create_dir_all(&startup).unwrap();
        let cfg = tmp.path().join("config").join("flow");
        fs::create_dir_all(&cfg).unwrap();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::set_var("APPDATA", &appdata) };
        unsafe { std::env::set_var("FLOW_CONFIG_DIR", &cfg) };
        (tmp, startup, cfg)
    }

    // ── startup_dir ────────────────────────────────────────────────────────

    #[test]
    fn startup_dir_ends_with_startup_when_appdata_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::set_var("APPDATA", "C:\\fake_roaming") };

        let dir = startup_dir().expect("APPDATA is set");
        assert!(dir.ends_with("Startup"), "dir was: {dir:?}");
        assert!(
            dir.to_string_lossy().contains("Start Menu"),
            "dir was: {dir:?}"
        );
    }

    #[test]
    fn startup_dir_errors_without_appdata() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("APPDATA") };

        let result = startup_dir();
        assert!(result.is_err(), "should error when APPDATA is unset");
    }

    // ── path helpers ───────────────────────────────────────────────────────

    #[test]
    fn ahk_pidfile_path_is_stable_name_in_config_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, cfg) = make_startup_tree();

        let path = ahk_pidfile_path();
        assert_eq!(path, cfg.join("flow.ahk.pid"));
    }

    #[test]
    fn flow_ahk_path_points_at_config_dir_script() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, cfg) = make_startup_tree();

        let path = flow_ahk_path();
        assert_eq!(path, cfg.join("flow.ahk"));
    }

    #[test]
    fn shortcut_filename_is_stable() {
        assert_eq!(FLOW_LNK, "flow.lnk");
    }

    // ── disable_autostart ──────────────────────────────────────────────────

    #[test]
    fn disable_autostart_no_op_when_shortcut_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, startup, _cfg) = make_startup_tree();

        // No flow.lnk exists yet.
        let report = disable_autostart().expect("missing shortcut is a no-op");
        assert_eq!(report.shortcut, startup.join(FLOW_LNK));
        assert!(!report.shortcut.exists());
    }

    #[test]
    fn disable_autostart_removes_existing_shortcut() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, startup, _cfg) = make_startup_tree();

        let link = startup.join(FLOW_LNK);
        fs::write(&link, b"placeholder").unwrap();
        assert!(link.exists());

        let report = disable_autostart().expect("removing existing shortcut");
        assert!(!link.exists(), "shortcut should be removed");
        assert_eq!(report.shortcut, link);
    }

    // ── enable_autostart ───────────────────────────────────────────────────

    #[test]
    fn enable_autostart_with_ahk_errors_when_script_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, startup, _cfg) = make_startup_tree();

        // flow.ahk does NOT exist in the config dir.
        let result = enable_autostart(true);
        let err = result.expect_err("should error when flow.ahk missing");
        assert!(
            err.contains("flow.ahk not found"),
            "error should mention flow.ahk: {err}"
        );
        assert!(
            err.contains("flow config init --ahk"),
            "error should suggest the fix: {err}"
        );
        // The pre-flight check must run BEFORE COM, so no shortcut is created.
        assert!(
            !startup.join(FLOW_LNK).exists(),
            "no shortcut should be left behind"
        );
    }

    // ── stop_ahk_script ────────────────────────────────────────────────────

    #[test]
    fn stop_ahk_script_returns_false_when_pidfile_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, _cfg) = make_startup_tree();

        let result = stop_ahk_script().expect("missing pidfile is a no-op");
        assert!(!result, "should report nothing was stopped");
    }

    #[test]
    fn stop_ahk_script_handles_corrupt_pidfile() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, cfg) = make_startup_tree();

        let pidfile = cfg.join("flow.ahk.pid");
        fs::write(&pidfile, "not-a-number").unwrap();

        let result = stop_ahk_script().expect("corrupt pidfile is a no-op");
        assert!(!result, "should report nothing was stopped");
        // The corrupt pidfile must be cleaned up so the next start is clean.
        assert!(!pidfile.exists(), "corrupt pidfile should be removed");
    }

    #[test]
    fn stop_ahk_script_errors_when_pidfile_unreadable() {
        // Negative: a pidfile path that exists but cannot be read as a file
        // (here: a directory) is neither `NotFound` nor a corrupt-content
        // case, so it must fall through to the generic I/O error branch and
        // surface an `Err`. On Windows, `read_to_string` on a directory
        // yields `PermissionDenied`, which is `!= NotFound` → Err path.
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, cfg) = make_startup_tree();

        let pidfile = cfg.join("flow.ahk.pid");
        fs::create_dir(&pidfile).expect("create dir as pidfile");

        let result = stop_ahk_script();
        assert!(
            result.is_err(),
            "unreadable pidfile should surface an error, got: {result:?}"
        );
        // The error must mention the pidfile path so the user can diagnose.
        let err = result.unwrap_err();
        assert!(
            err.contains("flow.ahk.pid"),
            "error should reference the pidfile path: {err}"
        );
    }

    // ── error propagation when APPDATA is unset ───────────────────────────
    //
    // `startup_dir()` is the root of the path-resolution chain; if APPDATA
    // is missing, every public command that needs `shortcut_path()` must
    // surface that error verbatim (via `?`) rather than panicking or
    // silently succeeding.

    #[test]
    fn disable_autostart_errors_when_appdata_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("APPDATA") };

        let result = disable_autostart();
        let err = result.expect_err("should error when APPDATA is unset");
        assert!(
            err.contains("%APPDATA%"),
            "error should mention %APPDATA%: {err}"
        );
    }

    #[test]
    fn enable_autostart_without_ahk_errors_when_appdata_unset() {
        // With `ahk = false`, the flow.ahk pre-flight check is skipped, so
        // the next call is `flow_exe_path()` (which succeeds — the test
        // binary exists) followed by `shortcut_path()`, which propagates
        // the APPDATA-unset error BEFORE any COM call. This pins that the
        // APPDATA check runs ahead of COM initialization.
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("APPDATA") };

        let result = enable_autostart(false);
        let err = result.expect_err("should error when APPDATA is unset");
        assert!(
            err.contains("%APPDATA%"),
            "error should mention %APPDATA%: {err}"
        );
    }

    // ── Wide: UTF-16 encoding helpers ──────────────────────────────────────
    //
    // `Wide` is the bridge between Rust `&str`/`OsStr` and the borrowed
    // `PCWSTR` Win32 ABI. Its correctness contract is:
    //   1. The buffer is null-terminated.
    //   2. `from_str` and `from_os` agree for ASCII input.
    //   3. `pcwstr()` returns a pointer into the live buffer.
    // These tests pin all three.

    #[test]
    fn wide_from_str_ascii_encodes_with_null_terminator() {
        // Nominal: each ASCII byte maps to one UTF-16 code unit, plus the
        // trailing NUL.
        let w = Wide::from_str("ABC");
        assert_eq!(w.buf, vec![0x41, 0x42, 0x43, 0x00]);
    }

    #[test]
    fn wide_from_str_empty_yields_only_null_terminator() {
        // Edge: empty input must still produce a valid (zero-length) wide
        // string — the buffer holds only the NUL terminator. Win32 APIs
        // that accept empty strings rely on this.
        let w = Wide::from_str("");
        assert_eq!(w.buf, vec![0x00]);
    }

    #[test]
    fn wide_from_str_unicode_bmp_encodes_as_single_code_unit() {
        // Non-ASCII within the Basic Multilingual Plane stays a single
        // UTF-16 code unit: U+00E9 (é) → 0xE9.
        let w = Wide::from_str("café");
        assert_eq!(w.buf, vec![0x63, 0x61, 0x66, 0xE9, 0x00]);
    }

    #[test]
    fn wide_from_str_supplemental_plane_encodes_as_surrogate_pair() {
        // Astral plane (U+1F600 GRINNING FACE) requires a surrogate pair.
        // Computed: (0x1F600 - 0x10000) >> 10 = 0x3D  → high = 0xD800 + 0x3D = 0xD83D
        //           (0x1F600 - 0x10000) & 0x3FF = 0x200 → low  = 0xDC00 + 0x200 = 0xDE00
        // This pins that `encode_wide` (Rust's WTF-8 → UTF-16) is used unchanged.
        let w = Wide::from_str("\u{1F600}");
        assert_eq!(w.buf, vec![0xD83D, 0xDE00, 0x00]);
    }

    #[test]
    fn wide_from_os_matches_from_str_for_ascii() {
        // `from_os` and `from_str` must agree on ASCII input, since
        // `from_str` delegates to `from_os(OsStr::new(s))`. This pins the
        // delegation and guards against a future divergence.
        let s = "hello";
        let from_os = Wide::from_os(OsStr::new(s));
        let from_str = Wide::from_str(s);
        assert_eq!(from_os.buf, from_str.buf);
    }

    #[test]
    fn wide_pcwstr_points_into_buffer_with_null_terminator() {
        // The PCWSTR returned by `pcwstr()` must point at the buffer's
        // first code unit and the buffer must be NUL-terminated so Win32
        // APIs can find the end. We read it back through PCWSTR's own
        // unsafe accessor to round-trip the data.
        let w = Wide::from_str("hi");
        let ptr = w.pcwstr();
        assert!(!ptr.is_null(), "PCWSTR must not be null");

        // SAFETY: `ptr` borrows `w.buf`, which is alive for the duration of
        // this read. `as_wide` calls `wcslen` and slices up to the NUL,
        // which we know is present at index 2 from the encoder's contract.
        let round_trip = unsafe { ptr.to_string() }.expect("valid UTF-16");
        assert_eq!(round_trip, "hi", "PCWSTR should round-trip the input");
    }
}
