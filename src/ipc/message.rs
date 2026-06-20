//! IPC message and response types for the stm protocol.
//!
//! The protocol uses newline-delimited JSON. The CLI sends a [`SocketMessage`],
//! the daemon replies with a [`SocketResponse`].
//!
//! # Wire format
//!
//! ```text
//! {"type":"stop"}\n
//! {"status":"ok"}\n
//! ```
//!
//! See the developer guide's *IPC & Watchdog* chapter
//! (`docs/src/dev-guide/ipc-and-watchdog.md`) for the message catalog and the
//! named-pipe transport.

use crate::common::Direction;
use serde::{Deserialize, Serialize};

/// Default named pipe path used by the daemon and CLI on Windows.
pub const PIPE_NAME: &str = r"\\.\pipe\stm";

/// Environment variable name for overriding the pipe path.
const PIPE_ENV: &str = "STM_PIPE_NAME";

/// Return the named pipe path for IPC.
///
/// Reads from the `STM_PIPE_NAME` environment variable, falling back to
/// [`PIPE_NAME`] if not set. This allows integration tests to use isolated
/// pipe names without interfering with a production daemon.
///
/// Both `stmd` and `stm` read the same variable, so spawning the daemon
/// via `stm start` inherits the variable automatically.
pub fn pipe_name() -> String {
    std::env::var(PIPE_ENV).unwrap_or_else(|_| PIPE_NAME.to_owned())
}

/// A command sent from `stm` CLI to the `stmd` daemon.
///
/// Serialised with an externally tagged `"type"` field and snake_case variant
/// names. See the developer guide (`docs/src/dev-guide/ipc-and-watchdog.md`)
/// for the full message catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SocketMessage {
    /// Shut down the daemon gracefully.
    Stop,
    /// Reload configuration from disk.
    ReloadConfig,
    /// Validate the current config file without applying it.
    CheckConfig,

    // --- Focus ---
    /// Move focus left.
    FocusLeft,
    /// Move focus right.
    FocusRight,
    /// Move focus up (within column).
    FocusUp,
    /// Move focus down (within column).
    FocusDown,

    // --- Swap (per-window) ---
    /// Swap the focused window with its left neighbour.
    SwapLeft,
    /// Swap the focused window with its right neighbour.
    SwapRight,
    /// Swap the focused window with the one above it (same column).
    SwapUp,
    /// Swap the focused window with the one below it (same column).
    SwapDown,

    // --- Column swap ---
    /// Swap the focused column with its neighbour in the given direction.
    ///
    /// Unlike the per-window `Swap*` messages above, this operates on the
    /// *entire column* containing the focused window. The layout engine's
    /// `swap_column` handles the viewport scroll automatically (via
    /// `ensure_column_visible`), so off-screen columns are brought into view
    /// as part of the same diff — no separate "offscreen" message is needed.
    SwapColumn {
        /// Direction to swap the column (left or right).
        direction: Direction,
    },

    // --- Semantic move ---
    /// Move the focused window in the given direction.
    ///
    /// This is a high-level, *semantic* command: the daemon translates the
    /// intended movement based on the window's current state and the layout.
    ///
    /// - Tiled window, left/right → swap the entire column (delegates to
    ///   [`SocketMessage::SwapColumn`]).
    /// - Tiled window, up/down → swap with the adjacent window in the same
    ///   column. *[deferred — not yet wired]*
    /// - Floating window, any direction → nudge by a configurable shift.
    ///   *[deferred — not yet wired]*
    ///
    /// Keeping `movewindow` separate from the concrete `SwapColumn`/`Swap*`
    /// messages lets the daemon own the "what does *move* mean here?"
    /// decision, so keybindings stay stable as floating support lands.
    MoveWindow {
        /// Direction to move the focused window.
        direction: Direction,
    },

    // --- Scroll ---
    /// Scroll viewport left.
    ScrollLeft,
    /// Scroll viewport right.
    ScrollRight,

    // --- Column resize ---
    /// Expand the focused column width.
    ExpandColumn,
    /// Shrink the focused column width.
    ShrinkColumn,
    /// Set focused column width to an explicit pixel value.
    ///
    /// The width is free-form (not snapped to the expand/shrink slot ladder)
    /// and is validated by the daemon against the engine's current bounds
    /// (`[min_column_width_px, abs_max_width]`).
    SetColumnWidth {
        /// Target column width in pixels.
        width_px: u32,
    },

    // --- Viewport center ---
    /// Center the viewport on the focused window, free-form (no slot snapping).
    ///
    /// Maps to [`ScrollingSpace::center_absolute`](crate::workspace::ScrollingSpace::center_absolute).
    /// Contrast with [`CenterGrid`](Self::CenterGrid), which snaps to the grid.
    Center,
    /// Center the viewport on the grid (slot-aligned, reuses initial-layout logic).
    ///
    /// Maps to [`ScrollingSpace::center_grid`](crate::workspace::ScrollingSpace::center_grid).
    /// When the column count fits `columns_per_screen`, this is equivalent to
    /// re-running `initialize_windows`'s viewport offset computation.
    CenterGrid,

    // --- Window state ---
    /// Toggle the focused window between tiling and floating.
    ToggleFloat,
    /// Toggle monocle mode on the focused column.
    ToggleMonocle,
    /// Place the focused window above (z-order).
    PlaceAbove,
    /// Promote the focused window to master (first position).
    Promote,
    /// Close the focused window.
    CloseWindow,

    // --- Workspace ---
    //
    // These three messages form the niri-style virtual-desktop surface.
    // `SwitchWorkspace` and `MoveWindowToWorkspace` are implemented (see
    // `daemon::dispatch`); `SwapWorkspace` is still a stub pending a separate
    // animation model.
    //
    // The `workspace_id` field carries a [`crate::workspace::WorkspaceId`]
    // (a `u32` newtype over niri-style IDs). On the wire it serialises as a
    // bare integer, matching `WorkspaceId`'s `#[serde(transparent)]` impl.
    /// Switch the active monitor's focus to the given workspace.
    ///
    /// Mirrors `stm dispatch switchworkspace <id>`. The previously active
    /// workspace is conceptually frozen in its packed position (above or
    /// below the target, the vertical analogue of horizontal column packing
    /// inside a `ScrollingSpace`) and the target slides into the viewport.
    SwitchWorkspace {
        /// Target workspace identifier (niri-style `u32`).
        workspace_id: u32,
    },
    /// Swap the active workspace with the target workspace in the monitor's
    /// workspace list.
    ///
    /// Mirrors `stm dispatch swapworkspace <id>`. The two workspaces exchange
    /// positions in the packed vertical stack; focus follows the originally
    /// active workspace to its new slot. This is the operation most other
    /// tiling managers omit but that is essential for rearranging a long
    /// workspace list without re-creating workspaces.
    SwapWorkspace {
        /// Target workspace identifier to swap with the active workspace.
        workspace_id: u32,
    },
    /// Move the focused window to the target workspace.
    ///
    /// Mirrors `stm dispatch movetoworkspace <id>`. The focused window is
    /// detached from the active workspace's `ScrollingSpace` (with local
    /// focus succession — no OS foreground focus push) and re-inserted into
    /// the target workspace's `ScrollingSpace` after its currently focused
    /// column. The camera then follows the moved window: the active workspace
    /// switches to the target so the moved window is visible (see
    /// `dispatch_move_window_to_workspace`). Moving to the currently active
    /// workspace is a no-op.
    ///
    /// The variant is named `MoveWindowToWorkspace` (rather than the shorter
    /// `MoveToWorkspace`) to mirror the sibling [`MoveWindow`](Self::MoveWindow)
    /// message: both operate on the focused *window*, and the longer name
    /// disambiguates from future workspace-level moves. The CLI surface
    /// keeps the shorter `movetoworkspace` for brevity.
    MoveWindowToWorkspace {
        /// Destination workspace identifier.
        workspace_id: u32,
    },

    // --- Queries ---
    /// Query all managed windows.
    QueryWindowsAll,
    /// Query the virtual layout.
    QueryLayoutVirtual,
    /// Query the actual (projected) layout.
    QueryLayoutActual,
    /// Query full daemon state.
    QueryState,

    // --- Config mutation ---
    /// Set a single config value at runtime.
    SetConfigValue {
        /// Dot-separated config key.
        key: String,
        /// JSON value to set.
        value: serde_json::Value,
    },
    /// Remove per-app learned preferences for the given executable.
    ForgetApp {
        /// Executable name (e.g. `"firefox.exe"`).
        exe: String,
    },
    /// Remove all per-app learned preferences.
    ForgetAllApps,
}

/// A response sent from the `stmd` daemon back to the `stm` CLI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SocketResponse {
    /// Command succeeded.
    Ok,
    /// Command failed with an error message.
    Error {
        /// Human-readable error description.
        message: String,
    },
    /// Response to a query, carrying arbitrary JSON data.
    Data {
        /// The query result payload.
        payload: serde_json::Value,
    },
}

/// Serialise a message as a single line of JSON terminated by `\\n`.
///
/// This is the wire format for the named pipe transport.
///
/// # Errors
///
/// Returns an error if `serde_json` fails to serialise `msg`.
pub fn encode_message<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Parse a single newline-delimited JSON message.
///
/// Returns `None` if the line is empty or whitespace-only.
pub fn decode_message<'de, T: Deserialize<'de>>(line: &'de str) -> Option<T> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Positive: round-trip Stop message
    #[test]
    fn roundtrip_stop() {
        let msg = SocketMessage::Stop;
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"stop"}"#);

        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SocketMessage::Stop);
    }

    // Positive: round-trip SetColumnWidth
    #[test]
    fn roundtrip_set_column_width() {
        let msg = SocketMessage::SetColumnWidth { width_px: 960 };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"set_column_width","width_px":960}"#);

        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SocketMessage::SetColumnWidth { width_px: 960 });
    }

    // Positive: round-trip SwapColumn
    #[test]
    fn roundtrip_swap_column() {
        let msg = SocketMessage::SwapColumn {
            direction: Direction::Right,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"swap_column","direction":"Right"}"#);

        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed,
            SocketMessage::SwapColumn {
                direction: Direction::Right,
            }
        );
    }

    // Positive: round-trip MoveWindow
    #[test]
    fn roundtrip_move_window() {
        let msg = SocketMessage::MoveWindow {
            direction: Direction::Left,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"move_window","direction":"Left"}"#);

        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed,
            SocketMessage::MoveWindow {
                direction: Direction::Left,
            }
        );
    }

    // Positive: round-trip SocketResponse::Ok
    #[test]
    fn roundtrip_response_ok() {
        let resp = SocketResponse::Ok;
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"status":"ok"}"#);

        let parsed: SocketResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SocketResponse::Ok);
    }

    // Positive: round-trip SocketResponse::Error
    #[test]
    fn roundtrip_response_error() {
        let resp = SocketResponse::Error {
            message: "daemon busy".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"status":"error","message":"daemon busy"}"#);

        let parsed: SocketResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    // Positive: round-trip SocketResponse::Data
    #[test]
    fn roundtrip_response_data() {
        let resp = SocketResponse::Data {
            payload: serde_json::json!({"windows": []}),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: SocketResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    // Positive: encode_message adds newline
    #[test]
    fn encode_adds_newline() {
        let encoded = encode_message(&SocketMessage::Stop).unwrap();
        assert!(encoded.ends_with('\n'));
        assert_eq!(encoded, "{\"type\":\"stop\"}\n");
    }

    // Positive: decode_message handles whitespace
    #[test]
    fn decode_trims_whitespace() {
        let msg: Option<SocketMessage> = decode_message("  {\"type\":\"stop\"}  \n");
        assert_eq!(msg, Some(SocketMessage::Stop));
    }

    // Negative: decode_message returns None for empty line
    #[test]
    fn decode_empty_returns_none() {
        let msg: Option<SocketMessage> = decode_message("");
        assert_eq!(msg, None);
    }

    // Negative: decode_message returns None for invalid JSON
    #[test]
    fn decode_invalid_returns_none() {
        let msg: Option<SocketMessage> = decode_message("not json");
        assert_eq!(msg, None);
    }

    // Positive: encode/decode round-trip through wire format
    #[test]
    fn wire_format_roundtrip() {
        let msg = SocketMessage::FocusLeft;
        let wire = encode_message(&msg).unwrap();
        let parsed: Option<SocketMessage> = decode_message(&wire);
        assert_eq!(parsed, Some(SocketMessage::FocusLeft));
    }

    // Positive: SetConfigValue round-trip
    #[test]
    fn roundtrip_set_config_value() {
        let msg = SocketMessage::SetConfigValue {
            key: "gaps.inner".into(),
            value: serde_json::json!(10),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    // --- Full coverage: all remaining unit variants via wire roundtrip ---

    // Positive: round-trip all simple (unit) variants not yet covered individually
    #[test]
    fn roundtrip_all_unit_variants() {
        // Every unit variant (no fields) except Stop (already tested above)
        let variants = vec![
            SocketMessage::ReloadConfig,
            SocketMessage::CheckConfig,
            SocketMessage::FocusRight,
            SocketMessage::FocusUp,
            SocketMessage::FocusDown,
            SocketMessage::SwapLeft,
            SocketMessage::SwapRight,
            SocketMessage::SwapUp,
            SocketMessage::SwapDown,
            SocketMessage::ScrollLeft,
            SocketMessage::ScrollRight,
            SocketMessage::ExpandColumn,
            SocketMessage::ShrinkColumn,
            SocketMessage::Center,
            SocketMessage::CenterGrid,
            SocketMessage::ToggleFloat,
            SocketMessage::ToggleMonocle,
            SocketMessage::PlaceAbove,
            SocketMessage::Promote,
            SocketMessage::CloseWindow,
            SocketMessage::QueryWindowsAll,
            SocketMessage::QueryLayoutVirtual,
            SocketMessage::QueryLayoutActual,
            SocketMessage::QueryState,
            SocketMessage::ForgetAllApps,
        ];

        for msg in &variants {
            let wire = encode_message(msg).unwrap();
            let parsed: Option<SocketMessage> = decode_message(&wire);
            assert_eq!(parsed.as_ref(), Some(msg), "roundtrip failed for: {msg:?}");
        }
    }

    // Positive: round-trip ForgetApp variant (has string field)
    #[test]
    fn roundtrip_forget_app() {
        let msg = SocketMessage::ForgetApp {
            exe: "firefox.exe".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""forget_app""#));
        assert!(json.contains(r#""exe":"firefox.exe""#));

        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    // Positive: round-trip SwitchWorkspace (struct variant carrying workspace_id)
    #[test]
    fn roundtrip_switch_workspace() {
        let msg = SocketMessage::SwitchWorkspace { workspace_id: 3 };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"switch_workspace","workspace_id":3}"#);

        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SocketMessage::SwitchWorkspace { workspace_id: 3 });
    }

    // Positive: round-trip SwapWorkspace
    #[test]
    fn roundtrip_swap_workspace() {
        let msg = SocketMessage::SwapWorkspace { workspace_id: 7 };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"swap_workspace","workspace_id":7}"#);

        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SocketMessage::SwapWorkspace { workspace_id: 7 });
    }

    // Positive: round-trip MoveWindowToWorkspace
    #[test]
    fn roundtrip_move_window_to_workspace() {
        let msg = SocketMessage::MoveWindowToWorkspace { workspace_id: 11 };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"move_window_to_workspace","workspace_id":11}"#
        );

        let parsed: SocketMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed,
            SocketMessage::MoveWindowToWorkspace { workspace_id: 11 }
        );
    }

    // Positive: workspace_id of 0 round-trips (boundary value)
    #[test]
    fn roundtrip_workspace_id_zero() {
        // WorkspaceId(0) is a valid construction; ensure the wire protocol
        // does not implicitly reject or mangle zero.
        for msg in [
            SocketMessage::SwitchWorkspace { workspace_id: 0 },
            SocketMessage::SwapWorkspace { workspace_id: 0 },
            SocketMessage::MoveWindowToWorkspace { workspace_id: 0 },
        ] {
            let wire = encode_message(&msg).unwrap();
            let parsed: Option<SocketMessage> = decode_message(&wire);
            assert_eq!(parsed.as_ref(), Some(&msg), "zero id failed: {wire}");
        }
    }

    // Positive: wire format roundtrip covers FocusLeft via encode+decode
    #[test]
    fn wire_format_roundtrip_set_column_width() {
        // Positive: set_column_width with an explicit pixel value round-trips
        let msg = SocketMessage::SetColumnWidth { width_px: 1280 };
        let wire = encode_message(&msg).unwrap();
        let parsed: Option<SocketMessage> = decode_message(&wire);
        assert_eq!(
            parsed,
            Some(SocketMessage::SetColumnWidth { width_px: 1280 })
        );
    }

    // --- encode_message error path ---

    // Negative: encode_message returns Err for a type that fails serialization
    #[test]
    fn encode_message_returns_err_for_unserializable() {
        /// A type that always fails during serialization, to exercise the error
        /// path of `encode_message`.
        #[derive(Debug)]
        struct AlwaysFailSerialize;

        impl serde::Serialize for AlwaysFailSerialize {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom(
                    "intentional serialization failure",
                ))
            }
        }

        let result = encode_message(&AlwaysFailSerialize);
        assert!(
            result.is_err(),
            "encode_message should return Err for failing serializer"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("intentional serialization failure")
        );
    }

    // Positive: encode_message returns Ok for all SocketMessage variants
    #[test]
    fn encode_message_succeeds_for_all_variants() {
        // Covers every variant to ensure none triggers an unexpected error
        let all_variants = vec![
            SocketMessage::Stop,
            SocketMessage::ReloadConfig,
            SocketMessage::CheckConfig,
            SocketMessage::FocusLeft,
            SocketMessage::FocusRight,
            SocketMessage::FocusUp,
            SocketMessage::FocusDown,
            SocketMessage::SwapLeft,
            SocketMessage::SwapRight,
            SocketMessage::SwapUp,
            SocketMessage::SwapDown,
            SocketMessage::SwapColumn {
                direction: Direction::Right,
            },
            SocketMessage::MoveWindow {
                direction: Direction::Left,
            },
            SocketMessage::ScrollLeft,
            SocketMessage::ScrollRight,
            SocketMessage::ExpandColumn,
            SocketMessage::ShrinkColumn,
            SocketMessage::Center,
            SocketMessage::CenterGrid,
            SocketMessage::SetColumnWidth { width_px: 800 },
            SocketMessage::ToggleFloat,
            SocketMessage::ToggleMonocle,
            SocketMessage::PlaceAbove,
            SocketMessage::Promote,
            SocketMessage::CloseWindow,
            SocketMessage::QueryWindowsAll,
            SocketMessage::QueryLayoutVirtual,
            SocketMessage::QueryLayoutActual,
            SocketMessage::QueryState,
            SocketMessage::SetConfigValue {
                key: "outer_gap".into(),
                value: serde_json::json!(16),
            },
            SocketMessage::ForgetApp {
                exe: "explorer.exe".into(),
            },
            SocketMessage::ForgetAllApps,
            SocketMessage::SwitchWorkspace { workspace_id: 1 },
            SocketMessage::SwapWorkspace { workspace_id: 2 },
            SocketMessage::MoveWindowToWorkspace { workspace_id: 3 },
        ];

        for msg in &all_variants {
            let result = encode_message(msg);
            assert!(result.is_ok(), "encode_message failed for variant: {msg:?}");
            let wire = result.unwrap();
            assert!(
                wire.ends_with('\n'),
                "wire format must end with newline for: {msg:?}"
            );
        }
    }

    // --- decode_message negative edge cases ---

    // Negative: decode_message returns None for unknown type tag
    #[test]
    fn decode_unknown_type_tag_returns_none() {
        let line = r#"{"type":"nonexistent_command"}"#;
        let msg: Option<SocketMessage> = decode_message(line);
        assert_eq!(msg, None, "unknown type tag should return None");
    }

    // Negative: decode_message returns None for missing type tag
    #[test]
    fn decode_missing_type_tag_returns_none() {
        let line = r#"{"not_a_type":"stop"}"#;
        let msg: Option<SocketMessage> = decode_message(line);
        assert_eq!(msg, None, "missing type tag should return None");
    }

    // Negative: decode_message returns None for plain object (no fields)
    #[test]
    fn decode_empty_object_returns_none() {
        let line = "{}";
        let msg: Option<SocketMessage> = decode_message(line);
        assert_eq!(msg, None, "empty object should return None");
    }

    // Negative: decode_message returns None for array instead of object
    #[test]
    fn decode_array_returns_none() {
        let line = "[1, 2, 3]";
        let msg: Option<SocketMessage> = decode_message(line);
        assert_eq!(msg, None, "array input should return None");
    }

    // Positive: decode_message handles trailing newline in wire
    #[test]
    fn decode_trailing_newline() {
        let line = "{\"type\":\"stop\"}\n";
        let msg: Option<SocketMessage> = decode_message(line);
        assert_eq!(msg, Some(SocketMessage::Stop));
    }

    // Positive: SocketResponse::Data with complex payload round-trips
    #[test]
    fn roundtrip_response_data_complex() {
        let resp = SocketResponse::Data {
            payload: serde_json::json!({
                "windows": [
                    {"id": 1, "title": "Firefox", "rect": [0, 0, 960, 1080]},
                    {"id": 2, "title": "VS Code", "rect": [960, 0, 960, 1080]}
                ],
                "focused": 1
            }),
        };
        let wire = encode_message(&resp).unwrap();
        let parsed: Option<SocketResponse> = decode_message(&wire);
        assert_eq!(parsed, Some(resp));
    }

    // Negative: decode_message returns None for response-shaped data on message type
    #[test]
    fn decode_response_status_on_message_returns_none() {
        // A response JSON has "status" tag, not "type" tag — should not parse as SocketMessage
        let line = r#"{"status":"ok"}"#;
        let msg: Option<SocketMessage> = decode_message(line);
        assert_eq!(
            msg, None,
            "response-shaped JSON should not parse as SocketMessage"
        );
    }

    // Negative: decode_message returns None for message-shaped data on response type
    #[test]
    fn decode_message_type_on_response_returns_none() {
        // A message JSON has "type" tag, not "status" tag — should not parse as SocketResponse
        let line = r#"{"type":"stop"}"#;
        let resp: Option<SocketResponse> = decode_message(line);
        assert_eq!(
            resp, None,
            "message-shaped JSON should not parse as SocketResponse"
        );
    }
}
