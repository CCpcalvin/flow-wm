//! Serde data model for the versioned loadout file.
//!
//! These types describe the JSON schema that `flowd` reads and writes when
//! saving/restoring a workspace arrangement across restarts. The daemon
//! owns the actual file I/O; this module is a pure data layer.

use serde::{Deserialize, Serialize};

/// Top-level loadout file envelope.
///
/// Serialized as a JSON object with a `version` field for forward-compat
/// and an RFC3339 `saved_at` timestamp for staleness checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadoutFile {
    /// File-format version number. Increment on breaking schema changes.
    pub version: u32,
    /// RFC3339 timestamp of when this snapshot was taken.
    pub saved_at: String,
    /// Per-workspace snapshots in the order they were saved.
    pub workspaces: Vec<WorkspaceSnapshot>,
}

/// Snapshot of a single workspace's tiling and floating state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    /// 0-based workspace index.
    pub workspace_id: u32,
    /// Whether this workspace was active (visible) when saved.
    pub active: bool,
    /// Tiling area snapshot (scrolling columns + rows).
    pub scrolling: ScrollingSnapshot,
    /// Floating window entries that were on this workspace.
    pub floating: Vec<FloatingEntry>,
}

/// Snapshot of the tiling canvas within a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollingSnapshot {
    /// Horizontal pixel offset the viewport was scrolled to.
    pub viewport_offset: i32,
    /// The window that had keyboard focus, if any.
    pub focus: Option<WindowRef>,
    /// Ordered list of tile columns.
    pub columns: Vec<ColumnSnapshot>,
}

/// Snapshot of a single tile column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnSnapshot {
    /// Pixel width of this column.
    pub width_px: u32,
    /// Ordered list of tile rows within this column.
    pub rows: Vec<RowSnapshot>,
}

/// Snapshot of a single tiled window row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowSnapshot {
    /// Window identity for match-based restoration.
    pub window: WindowRef,
    /// Pixel height allocated to this row.
    pub height_px: i32,
}

/// A floating window entry with its position and size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatingEntry {
    /// Window identity for match-based restoration.
    pub window: WindowRef,
    /// Screen rectangle where the window was placed.
    pub rect: RectJson,
}

/// Strict match triple for identifying a window across daemon restarts.
///
/// `WindowId` (HWND) is ephemeral — this triple survives process restarts
/// because it is based on observable window properties rather than handles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRef {
    /// Executable basename (e.g. `"firefox.exe"`).
    pub exe: String,
    /// Window class name (e.g. `"MozillaWindowClass"`).
    pub class: String,
    /// Window title at the time of snapshot.
    pub title: String,
}

/// Plain pixel rectangle for floating window placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RectJson {
    /// Left-edge pixel coordinate.
    pub x: i32,
    /// Top-edge pixel coordinate.
    pub y: i32,
    /// Width in pixels.
    pub w: i32,
    /// Height in pixels.
    pub h: i32,
}

/// Returns `true` when the loadout snapshot is older than `max_age_secs`
/// or when `saved_at` cannot be parsed.
///
/// Unparseable timestamps are treated as stale so the daemon skips them
/// rather than crashing on corrupt data.
#[must_use]
pub fn is_stale(saved_at: &str, max_age_secs: u64) -> bool {
    let saved = match chrono::DateTime::parse_from_rfc3339(saved_at) {
        Ok(dt) => dt,
        Err(_) => return true,
    };
    let now = chrono::Utc::now();
    let elapsed = now.signed_duration_since(saved);
    elapsed.num_seconds() > max_age_secs as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Positive: serde round-trip ────────────────────────────────────

    /// Round-trip a complete [`LoadoutFile`] through JSON.
    // Positive: full loadout serializes and deserializes correctly.
    #[test]
    fn round_trip_full_loadout() {
        let file = LoadoutFile {
            version: 1,
            saved_at: "2026-07-24T12:00:00Z".to_string(),
            workspaces: vec![WorkspaceSnapshot {
                workspace_id: 0,
                active: true,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 128,
                    focus: Some(WindowRef {
                        exe: "code.exe".into(),
                        class: "Chrome_WidgetWin_1".into(),
                        title: "main.rs — flow-wm".into(),
                    }),
                    columns: vec![ColumnSnapshot {
                        width_px: 960,
                        rows: vec![RowSnapshot {
                            window: WindowRef {
                                exe: "code.exe".into(),
                                class: "Chrome_WidgetWin_1".into(),
                                title: "main.rs — flow-wm".into(),
                            },
                            height_px: 600,
                        }],
                    }],
                },
                floating: vec![FloatingEntry {
                    window: WindowRef {
                        exe: "slack.exe".into(),
                        class: "Slack_Window".into(),
                        title: "Slack".into(),
                    },
                    rect: RectJson {
                        x: 100,
                        y: 100,
                        w: 400,
                        h: 300,
                    },
                }],
            }],
        };

        let json = serde_json::to_string(&file).expect("serialize");
        let restored: LoadoutFile = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.version, 1);
        assert_eq!(restored.saved_at, "2026-07-24T12:00:00Z");
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].workspace_id, 0);
        assert!(restored.workspaces[0].active);
        assert_eq!(restored.workspaces[0].scrolling.viewport_offset, 128);
        assert!(restored.workspaces[0].scrolling.focus.is_some());
        assert_eq!(restored.workspaces[0].scrolling.columns.len(), 1);
        assert_eq!(restored.workspaces[0].scrolling.columns[0].width_px, 960);
        assert_eq!(restored.workspaces[0].floating.len(), 1);
        assert_eq!(restored.workspaces[0].floating[0].rect.x, 100);
    }

    /// Round-trip an empty loadout (no workspaces).
    // Positive: empty loadout round-trips without loss.
    #[test]
    fn round_trip_empty_loadout() {
        let file = LoadoutFile {
            version: 1,
            saved_at: "2026-01-01T00:00:00Z".into(),
            workspaces: vec![],
        };

        let json = serde_json::to_string(&file).expect("serialize");
        let restored: LoadoutFile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.workspaces.len(), 0);
    }

    // ── Positive: is_stale ────────────────────────────────────────────

    /// A snapshot from the past older than the threshold is stale.
    // Positive: old timestamp is stale.
    #[test]
    fn stale_when_old() {
        let saved = "2020-01-01T00:00:00Z";
        assert!(is_stale(saved, 60));
    }

    /// A snapshot taken just now (within threshold) is fresh.
    // Positive: now-relative timestamp is fresh.
    #[test]
    fn fresh_when_recent() {
        let now = chrono::Utc::now().to_rfc3339();
        assert!(!is_stale(&now, 3600));
    }

    // ── Negative: is_stale ───────────────────────────────────────────

    /// Garbage strings are treated as stale (never crash).
    // Negative: unparseable input returns true (stale).
    #[test]
    fn unparseable_is_stale() {
        assert!(is_stale("not-a-date", 3600));
        assert!(is_stale("", 3600));
    }

    /// A timestamp exactly at the threshold boundary is NOT stale
    /// (must be strictly greater than max_age_secs).
    // Negative: boundary — equal age is fresh.
    #[test]
    fn boundary_equals_is_fresh() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::seconds(300);
        assert!(!is_stale(&past.to_rfc3339(), 300));
    }

    /// One second beyond the threshold IS stale.
    // Negative: one second over threshold is stale.
    #[test]
    fn one_second_over_is_stale() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::seconds(301);
        assert!(is_stale(&past.to_rfc3339(), 300));
    }

    /// A timestamp in the future (clock skew ahead of `now`) is NOT stale.
    ///
    /// `is_stale` computes `now - saved`; when `saved` is ahead of `now` the
    /// signed duration is negative, and `negative > max_age_secs` is `false`.
    /// This is the desired behavior: a slightly-future timestamp from clock
    /// skew must not cause the loadout to be silently dropped on auto-restore.
    // Negative: future timestamp boundary — returns fresh (not stale).
    #[test]
    fn future_timestamp_is_fresh() {
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::seconds(600);
        assert!(
            !is_stale(&future.to_rfc3339(), 60),
            "a future saved_at must be treated as fresh (negative elapsed)"
        );
    }

    // ── Negative: serde malformed input ──────────────────────────────

    /// JSON missing a required field (`version`) fails to deserialize.
    ///
    /// Every field of [`LoadoutFile`] is required (no `#[serde(default)]`),
    /// so a payload omitting any one of them must be rejected at parse time
    /// rather than silently producing a half-populated struct.
    // Negative: missing required field rejects deserialization.
    #[test]
    fn missing_version_field_fails_to_deserialize() {
        let json = r#"{"saved_at":"2026-07-24T12:00:00Z","workspaces":[]}"#;
        let result: Result<LoadoutFile, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "missing 'version' field must fail deserialization"
        );
    }

    /// Completely invalid JSON fails to deserialize.
    // Negative: garbage input rejects deserialization.
    #[test]
    fn invalid_json_fails_to_deserialize() {
        let result: Result<LoadoutFile, _> = serde_json::from_str("not json {{{");
        assert!(result.is_err(), "garbage input must fail deserialization");
    }
}
