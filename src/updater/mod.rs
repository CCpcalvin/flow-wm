//! Self-update for `flow update`. Pulls the latest GitHub release zip +
//! sha256 sidecar, verifies the digest, stages the new `flow.exe`/`flowd.exe`
//! into `.stage/`, then spawns a detached PowerShell shim that swaps them in
//! after this process exits. See (`docs/src/dev-guide/updater.md`) for the
//! Windows-lock rationale and the full flow.

mod shim;

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest, Sha256};
use thiserror::Error;

const GH_LATEST: &str = "https://api.github.com/repos/CCpcalvin/flow-wm/releases/latest";
const USER_AGENT: &str = concat!("flow-wm/", env!("CARGO_PKG_VERSION"));
const ZIP_SUFFIX: &str = "-x86_64.zip";
const SHA_SUFFIX: &str = "-x86_64.zip.sha256";

/// Upper bound on the version-check network call.
///
/// `check_for_update` can fire automatically from `flow start` (when
/// `check_for_updates` is enabled). A bounded timeout guarantees a dead or slow
/// network never blocks startup beyond this long. The download path in
/// `perform_update` is intentionally left unbounded — that path is interactive
/// and transfers a multi-MB archive.
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Errors that can occur during a self-update.
#[derive(Debug, Error)]
pub enum UpdateError {
    /// Network / HTTP failure.
    #[error("network: {0}")]
    Network(String),
    /// A required release asset was not found.
    #[error("missing release asset: {0}")]
    AssetMissing(&'static str),
    /// The downloaded zip did not match the published SHA256.
    #[error("sha256 mismatch (expected {expected}, computed {computed})")]
    ShaMismatch {
        /// The hash published in the sidecar.
        expected: String,
        /// The hash computed from the downloaded bytes.
        computed: String,
    },
    /// The sha256 sidecar could not be parsed.
    #[error("unparseable sha256 sidecar")]
    UnparseableSidecar,
    /// A zip-archive structural error.
    #[error("zip: {0}")]
    Zip(String),
    /// A required entry was missing inside the zip.
    #[error("zip missing entry: {0}")]
    ZipMissingEntry(&'static str),
    /// I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON parse error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The running exe lives inside a Windows zip-extraction staging dir.
    #[error("running from a zip extraction ({0}); extract the archive fully first")]
    RunningFromZip(String),
    /// The install dir is not writable.
    #[error("install dir is read-only ({0}); install elsewhere or rerun as admin")]
    ReadOnlyDir(String),
    /// The detached swap shim could not be spawned.
    #[error("shim spawn failed: {0}")]
    ShimSpawn(String),
}

impl From<ureq::Error> for UpdateError {
    fn from(e: ureq::Error) -> Self {
        UpdateError::Network(e.to_string())
    }
}

#[derive(Debug, serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

/// Parse `"vX.Y.Z"` or `"X.Y.Z"` into a `(major, minor, patch)` triple.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim_start_matches('v');
    let mut p = s.split('.');
    Some((
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
    ))
}

/// True if `a` is strictly newer than `b`.
fn is_newer(a: (u32, u32, u32), b: (u32, u32, u32)) -> bool {
    a > b
}

fn fetch_latest() -> Result<Release, UpdateError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(UPDATE_CHECK_TIMEOUT))
        .build()
        .into();
    let mut resp = agent
        .get(GH_LATEST)
        .header("User-Agent", USER_AGENT)
        .call()?;
    let bytes = resp.body_mut().read_to_vec()?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn download(url: &str) -> Result<Vec<u8>, UpdateError> {
    let mut resp = ureq::get(url).header("User-Agent", USER_AGENT).call()?;
    Ok(resp.body_mut().read_to_vec()?)
}

/// Compute lowercase-hex SHA256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Parse a GNU-coreutils-style sidecar (`<hash>  <filename>\n`). Returns the
/// hash lowercased. Accepts any case on input.
fn parse_sidecar(s: &str) -> Option<String> {
    let h = s.lines().next()?.split_whitespace().next()?;
    if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(h.to_ascii_lowercase())
}

fn extract_binaries(zip_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), UpdateError> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| UpdateError::Zip(e.to_string()))?;
    let flow = read_entry(&mut archive, "flow.exe")?;
    let flowd = read_entry(&mut archive, "flowd.exe")?;
    Ok((flow, flowd))
}

fn read_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &'static str,
) -> Result<Vec<u8>, UpdateError> {
    let mut f = archive
        .by_name(name)
        .map_err(|_| UpdateError::ZipMissingEntry(name))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn find_asset<F: Fn(&str) -> bool>(release: &Release, pred: F) -> Option<String> {
    release
        .assets
        .iter()
        .find(|a| pred(&a.name))
        .map(|a| a.browser_download_url.clone())
}

/// Resolve the install directory as the parent of the currently running exe.
///
/// Refuses to proceed if the exe lives inside a Windows zip-extraction staging
/// dir (`%TEMP%\TempN_<digits>\`) or if the dir is not writable.
pub fn install_dir() -> Result<PathBuf, UpdateError> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| UpdateError::ReadOnlyDir("cannot resolve parent of current exe".into()))?;
    let s = dir.to_string_lossy();
    if s.contains("\\TempN_") {
        return Err(UpdateError::RunningFromZip(s.into_owned()));
    }
    // Probe writability with a throwaway file rather than guessing ACLs.
    let probe = dir.join(".flow-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(UpdateError::ReadOnlyDir(s.into_owned()));
        }
        Err(_) => {
            let _ = std::fs::remove_file(&probe);
        }
    }
    Ok(dir.to_path_buf())
}

/// Check whether a newer GitHub release exists.
///
/// Returns `Some(tag)` (e.g. `"v0.2.0"`) if a newer release is available, or
/// `None` if the running version is current. Safe to call while the daemon is
/// running — performs network + version comparison only.
pub fn check_for_update() -> Result<Option<String>, UpdateError> {
    let release = fetch_latest()?;
    let latest = parse_version(&release.tag_name)
        .ok_or_else(|| UpdateError::Network(format!("unparseable tag: {}", release.tag_name)))?;
    let current = parse_version(env!("CARGO_PKG_VERSION"))
        .ok_or_else(|| UpdateError::Network("unparseable CARGO_PKG_VERSION".into()))?;
    Ok(if is_newer(latest, current) {
        Some(release.tag_name)
    } else {
        None
    })
}

/// Perform a self-update.
///
/// The caller MUST guarantee the daemon is NOT running (`flowd.exe` would be
/// locked by Windows and the swap would fail); this function does not check.
/// On success the new version tag is returned and a detached shim has been
/// spawned to finish the swap after this process exits.
pub fn perform_update() -> Result<String, UpdateError> {
    let release = fetch_latest()?;
    let latest = parse_version(&release.tag_name)
        .ok_or_else(|| UpdateError::Network(format!("unparseable tag: {}", release.tag_name)))?;
    let current = parse_version(env!("CARGO_PKG_VERSION"))
        .ok_or_else(|| UpdateError::Network("unparseable CARGO_PKG_VERSION".into()))?;
    if !is_newer(latest, current) {
        return Ok(release.tag_name); // already current, nothing to do
    }

    let zip_url = find_asset(&release, |n| n.ends_with(ZIP_SUFFIX))
        .ok_or(UpdateError::AssetMissing("release zip"))?;
    let sha_url = find_asset(&release, |n| n.ends_with(SHA_SUFFIX))
        .ok_or(UpdateError::AssetMissing("sha256 sidecar"))?;

    let sha_text = String::from_utf8_lossy(&download(&sha_url)?).into_owned();
    let expected = parse_sidecar(&sha_text).ok_or(UpdateError::UnparseableSidecar)?;

    let zip_bytes = download(&zip_url)?;
    let computed = sha256_hex(&zip_bytes);
    if computed != expected {
        return Err(UpdateError::ShaMismatch { expected, computed });
    }

    let (flow_bytes, flowd_bytes) = extract_binaries(&zip_bytes)?;

    let dir = install_dir()?;
    let stage = dir.join(".stage");
    if stage.exists() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    std::fs::create_dir_all(&stage)?;
    std::fs::write(stage.join("flow.exe"), &flow_bytes)?;
    std::fs::write(stage.join("flowd.exe"), &flowd_bytes)?;

    shim::spawn_swap_shim(&dir, std::process::id())?;

    Ok(release.tag_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn version_parse_and_compare() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("v0.0.0"), Some((0, 0, 0)));
        assert_eq!(parse_version("garbage"), None);
        assert_eq!(parse_version("1.2"), None);
        assert!(is_newer((1, 0, 0), (0, 9, 9)));
        assert!(is_newer((1, 0, 1), (1, 0, 0)));
        assert!(!is_newer((1, 0, 0), (1, 0, 0)));
        assert!(!is_newer((0, 9, 9), (1, 0, 0)));
    }

    #[test]
    fn sidecar_parsing() {
        let h64 = "abc123def4567890abc123def4567890abc123def4567890abc123def4567890";
        assert_eq!(
            parse_sidecar(&format!("{h64}  flow-wm-0.1.0-x86_64.zip\n")),
            Some(h64.into())
        );
        // Uppercase input normalizes to lowercase.
        let up = "ABCDABCDABCDABCDABCDABCDABCDABCDABCDABCDABCDABCDABCDABCDABCDABCD";
        let lo = "abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";
        assert_eq!(parse_sidecar(&format!("{up}  x.zip")), Some(lo.into()));
        // Too short / non-hex rejected.
        assert_eq!(parse_sidecar("short  x.zip"), None);
        assert_eq!(parse_sidecar(&format!("{}  x.zip", "z".repeat(64))), None);
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_hex_longer_known_vector() {
        // Second NIST test block (56 bytes). Pairs with the empty/abc cases
        // above to cover a multi-block input for the security-critical digest
        // used to verify the downloaded release.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Output must always be lowercase hex of length 64.
        let h = sha256_hex(b"arbitrary");
        assert_eq!(h.len(), 64);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn parse_sidecar_rejects_empty_and_blank_input() {
        // Negative edge: GitHub sidecars are never empty in practice, but a
        // truncated/corrupt download could yield an empty body. The parser
        // must reject it rather than return a bogus empty hash.
        assert_eq!(parse_sidecar(""), None);
        assert_eq!(parse_sidecar("   \n\t\n"), None);
        // A first line with only whitespace and no hash token also yields None.
        assert_eq!(parse_sidecar("   \nABCDEF"), None);
    }

    #[test]
    fn find_asset_matches_by_predicate() {
        let release = Release {
            tag_name: "v0.2.0".into(),
            assets: vec![
                Asset {
                    name: "flow-wm-0.2.0-x86_64.zip".into(),
                    browser_download_url: "https://example/zip".into(),
                },
                Asset {
                    name: "flow-wm-0.2.0-x86_64.zip.sha256".into(),
                    browser_download_url: "https://example/sha".into(),
                },
                Asset {
                    name: "README.md".into(),
                    browser_download_url: "https://example/readme".into(),
                },
            ],
        };
        // Predicate matching the zip suffix returns its URL.
        assert_eq!(
            find_asset(&release, |n| n.ends_with(ZIP_SUFFIX)),
            Some("https://example/zip".into())
        );
        // Predicate matching the sha suffix returns its URL.
        assert_eq!(
            find_asset(&release, |n| n.ends_with(SHA_SUFFIX)),
            Some("https://example/sha".into())
        );
        // Predicate matching nothing returns None.
        assert_eq!(find_asset(&release, |n| n.ends_with(".tar.gz")), None);
    }

    #[test]
    fn find_asset_on_empty_release() {
        let release = Release {
            tag_name: "v0.2.0".into(),
            assets: vec![],
        };
        assert_eq!(find_asset(&release, |_| true), None);
    }

    /// Build an in-memory zip whose entries are given by `entries`.
    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, data) in entries {
                writer.start_file(*name, opts).unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extract_binaries_round_trips_both_exes() {
        let zip_bytes = build_zip(&[
            ("flow.exe", b"FLOW-BYTES"),
            ("flowd.exe", b"FLOWD-BYTES"),
            ("README.md", b"docs"),
        ]);
        let (flow, flowd) = extract_binaries(&zip_bytes).expect("extraction should succeed");
        assert_eq!(flow, b"FLOW-BYTES");
        assert_eq!(flowd, b"FLOWD-BYTES");
    }

    #[test]
    fn extract_binaries_missing_flow_returns_zip_missing_entry() {
        let zip_bytes = build_zip(&[("flowd.exe", b"only daemon")]);
        let err = extract_binaries(&zip_bytes).expect_err("should fail");
        assert!(matches!(err, UpdateError::ZipMissingEntry("flow.exe")));
    }

    #[test]
    fn extract_binaries_missing_flowd_returns_zip_missing_entry() {
        let zip_bytes = build_zip(&[("flow.exe", b"only client")]);
        let err = extract_binaries(&zip_bytes).expect_err("should fail");
        assert!(matches!(err, UpdateError::ZipMissingEntry("flowd.exe")));
    }

    #[test]
    fn extract_binaries_bad_zip_returns_zip_error() {
        let err = extract_binaries(b"not a zip").expect_err("should fail");
        assert!(matches!(err, UpdateError::Zip(_)));
    }
}
