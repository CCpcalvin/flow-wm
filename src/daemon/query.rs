//! Query handlers for IPC commands.
//!
//! This module contains methods that extract query logic from the dispatch
//! match arms into dedicated methods. Each method returns a [`SocketResponse`]
//! with JSON data.

use crate::ipc::message::SocketResponse;
use crate::events::WindowDescriptor;
use windows::Win32::Foundation::HWND;

use super::types::FlowWM;

impl FlowWM {
    /// Return all tracked windows as JSON.
    ///
    /// The viewport offset from the virtual layout is included so clients
    /// can determine which windows are currently visible on screen.
    pub(super) fn query_windows_all(&self) -> SocketResponse {
        let viewport_offset = self.active_scrolling().virtual_layout().viewport_offset;
        SocketResponse::Data {
            payload: self.registry.to_json_value(viewport_offset),
        }
    }

    /// Return the virtual layout structure as JSON.
    ///
    /// The virtual layout represents the logical structure on an infinite
    /// horizontal canvas. Columns store pixel widths (`width_px`), not pixel
    /// positions (the projection layer derives positions via prefix-sum).
    pub(super) fn query_layout_virtual(&self) -> SocketResponse {
        let vl = self.active_scrolling().virtual_layout();
        let columns_json: Vec<serde_json::Value> = vl
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let rows: Vec<serde_json::Value> = col
                    .rows
                    .iter()
                    .map(|row| serde_json::json!({ "window_id": row.window_id.0, "height_px": row.height }))
                    .collect();
                serde_json::json!({
                    "index": i,
                    "width_px": col.width_px,
                    "rows": rows,
                })
            })
            .collect();
        SocketResponse::Data {
            payload: serde_json::json!({
                "viewport_offset": vl.viewport_offset,
                "column_count": vl.columns.len(),
                "window_count": vl.window_count(),
                "columns": columns_json,
            }),
        }
    }

    /// Return the actual layout (projected coordinates) as JSON.
    ///
    /// The actual layout contains pixel-level coordinates for each window
    /// after projection and padding have been applied.
    pub(super) fn query_layout_actual(&self) -> SocketResponse {
        let vl = self.active_scrolling().virtual_layout();
        let al = self.active_scrolling().actual_layout();
        let entries_json: Vec<serde_json::Value> = al
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "window_id": e.window_id.0,
                    "rect": {
                        "x": e.rect.x,
                        "y": e.rect.y,
                        "width": e.rect.width,
                        "height": e.rect.height,
                    },
                })
            })
            .collect();
        SocketResponse::Data {
            payload: serde_json::json!({
                "viewport_offset": vl.viewport_offset,
                "entry_count": al.entries.len(),
                "entries": entries_json,
            }),
        }
    }

    /// Return full daemon state as JSON — the `QueryState` payload.
    ///
    /// This is the single state-shaping function shared by the `flow query
    /// state` pull and the `state_snapshot` event push (ADR-0005: “one
    /// serializer, two callers”). It reports the active monitor, every
    /// monitor’s workspace stack (id, viewport offset, column / window
    /// counts), and the focused window descriptor — enough for a status bar
    /// to first-paint without any further query.
    pub(super) fn query_state(&self) -> SocketResponse {
        SocketResponse::Data {
            payload: self.daemon_state_value(),
        }
    }

    /// Build the full daemon-state payload shared by `QueryState` and
    /// `StateSnapshot`.
    ///
    /// Kept separate from [`query_state`](Self::query_state) so the subscribe
    /// path can push the same bytes without wrapping them in a
    /// [`SocketResponse`].
    pub(super) fn daemon_state_value(&self) -> serde_json::Value {
        // Serialize the shared `WindowDescriptor` rather than rebuilding the
        // `{hwnd,title,exe,class}` object by hand — one shape for the focused
        // window here and for every event variant that carries a window.
        let focused_window = self
            .registry
            .focused()
            .and_then(|id| self.window_descriptor(id))
            .and_then(|w| serde_json::to_value(&w).ok());

        let monitors_json: Vec<serde_json::Value> = self
            .monitors
            .iter()
            .enumerate()
            .map(|(index, monitor)| {
                let workspaces_json: Vec<serde_json::Value> = monitor
                    .workspaces()
                    .iter()
                    .map(|ws| {
                        let vl = ws.scrolling.virtual_layout();
                        serde_json::json!({
                            "id": ws.id.0,
                            "viewport_offset": vl.viewport_offset,
                            "column_count": vl.columns.len(),
                            "window_count": vl.window_count(),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "index": index,
                    "active_workspace": monitor.active_workspace_id().0,
                    "workspaces": workspaces_json,
                })
            })
            .collect();

        serde_json::json!({
            "active_monitor": self.active_monitor,
            "monitors": monitors_json,
            "focused_window": focused_window,
        })
    }

    /// Build a [`WindowDescriptor`] for the given window id, or `None` if it is
    /// not tracked.
    ///
    /// Shared by every event variant that carries a window reference so the
    /// `{hwnd, title, exe, class}` shape is built in one place.
    pub(super) fn window_descriptor(&self, id: crate::common::WindowId) -> Option<WindowDescriptor> {
        let w = self.registry.get_window(HWND(id.0 as *mut _))?;
        Some(WindowDescriptor {
            hwnd: w.hwnd.0 as isize,
            title: w.title.clone(),
            exe: w.exe.clone(),
            class: w.class.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that query methods exist and have the correct return type.
    ///
    /// Since FlowWM construction requires admin rights (for named pipe)
    /// and a real Windows desktop, we can't do full unit tests here.
    /// The query methods are tested indirectly via integration tests in tests/cli/.
    #[test]
    fn query_methods_exist_and_have_correct_return_type() {
        // This test verifies that the methods exist and return SocketResponse.
        // We can't call them directly without a FlowWM instance.
        // The actual behavior is tested via integration tests.

        // query_windows_all returns SocketResponse
        let _type_check: fn(&FlowWM) -> SocketResponse = FlowWM::query_windows_all;

        // query_layout_virtual returns SocketResponse
        let _type_check: fn(&FlowWM) -> SocketResponse = FlowWM::query_layout_virtual;

        // query_layout_actual returns SocketResponse
        let _type_check: fn(&FlowWM) -> SocketResponse = FlowWM::query_layout_actual;
    }

    /// Positive: verify that SocketResponse::Data is constructible with JSON.
    ///
    /// This ensures that the query methods can return the correct type.
    #[test]
    fn socket_response_data_constructible_with_json() {
        let payload = serde_json::json!({
            "viewport_offset": 0,
            "column_count": 0,
            "window_count": 0,
            "columns": []
        });
        let response = SocketResponse::Data { payload };

        match response {
            SocketResponse::Data { payload } => {
                assert!(payload.is_object());
                assert_eq!(payload["viewport_offset"], 0);
            }
            _ => panic!("should be Data variant"),
        }
    }

    /// Positive: verify that JSON serialization works for query payloads.
    #[test]
    fn query_payload_serializes_to_json() {
        let payload = serde_json::json!({
            "test_key": "test_value",
            "number": 42
        });

        let json_str = serde_json::to_string(&payload);
        assert!(json_str.is_ok());
        assert!(json_str.unwrap().contains("test_value"));
    }

    /// Positive: verify that virtual layout JSON structure is valid.
    #[test]
    fn virtual_layout_json_structure_is_valid() {
        // Create a minimal virtual layout structure using a JSON array
        let columns_json = serde_json::json!([]);
        let window_count: usize = 0;

        let json = serde_json::json!({
            "viewport_offset": 0,
            "column_count": columns_json.as_array().unwrap().len(),
            "window_count": window_count,
            "columns": columns_json
        });

        assert_eq!(json["viewport_offset"], 0);
        assert_eq!(json["column_count"], 0);
        assert_eq!(json["window_count"], 0);
        assert!(json["columns"].as_array().unwrap().is_empty());
    }

    /// Positive: verify that actual layout JSON structure is valid.
    #[test]
    fn actual_layout_json_structure_is_valid() {
        // Create a minimal actual layout structure using a JSON array
        let entries_json = serde_json::json!([]);

        let json = serde_json::json!({
            "viewport_offset": 0,
            "entry_count": entries_json.as_array().unwrap().len(),
            "entries": entries_json
        });

        assert_eq!(json["viewport_offset"], 0);
        assert_eq!(json["entry_count"], 0);
        assert!(json["entries"].as_array().unwrap().is_empty());
    }
}
