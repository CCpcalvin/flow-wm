//! Published semantic [`Event`]s for the event-broadcast subsystem.
//!
//! An [`Event`] is a semantic state change the daemon announces to
//! subscribers over their subscriber-owned named pipes. The [`Event`] enum
//! *is* the wire schema: it is serialized with a flat `"type"` tag
//! (`#[serde(tag = "type", rename_all = "snake_case")]`), one JSON object per
//! newline. Consumers dispatch on `type` and ignore values they do not
//! recognise, so adding an event is one variant plus one emit call site and
//! never breaks existing subscribers.
//!
//! Events are never raw [`HookEvent`](crate::registry::HookEvent)s — they are
//! derived semantic state changes. See ADR-0005
//! (`docs/adr/0005-event-broadcast-named-pipe.md`) and the *Event publishing*
//! section of `CONTEXT.md` for the canonical terms.
//!
//! The crate only *serializes* [`Event`]s (the daemon is the writer); the
//! `Event` enum therefore derives [`serde::Serialize`] but not
//! [`serde::Deserialize`]. Subscribers parse the wire bytes themselves.

use serde::Serialize;

/// A window descriptor carried by events that reference a specific window.
///
/// Serialized as a nested object `{ "hwnd", "title", "exe", "class" }` so a
/// subscriber can render the active application without re-querying. Shared by
/// every event variant that names a window.
#[derive(Debug, Clone, Serialize)]
pub struct WindowDescriptor {
    /// Win32 window handle value.
    pub hwnd: isize,
    /// Window title bar text.
    pub title: String,
    /// Executable name (e.g. `"code.exe"`).
    pub exe: String,
    /// Win32 window class name.
    pub class: String,
}

/// A published semantic state change announced to subscribers.
///
/// Serialized as newline-delimited JSON with a flat `"type"` tag, e.g.
/// `{"type":"state_snapshot", ...}`. Each variant carries enough for a
/// subscriber to render without re-querying the daemon.
///
/// # Extensibility
///
/// The enum is the wire contract. Adding a new event is one variant here plus
/// one `emit`/broadcast call site where the state change occurs; no transport
/// code changes, and existing subscribers keep working because they ignore
/// unknown `type` values.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A full snapshot of daemon state, pushed once to each subscriber
    /// immediately after it subscribes so it can first-paint without issuing
    /// a separate query.
    ///
    /// The payload is exactly the [`QueryState`](crate::ipc::message::SocketMessage::QueryState)
    /// payload (one state-shaping function, two callers: the `flow query`
    /// pull and the subscribe push). Its keys are flattened into the event
    /// object alongside the `type` tag, matching the wire shape documented in
    /// ADR-0005.
    StateSnapshot {
        /// The full daemon-state payload, flattened into the event object.
        #[serde(flatten)]
        state: serde_json::Value,
    },

    /// The active (visible) workspace on a monitor changed.
    ///
    /// Fires on every active-workspace switch regardless of cause — a direct
    /// `switch-workspace`, or the camera-follow step of a `move-to-workspace`
    /// — so a subscriber tracking only the current workspace never misses a
    /// switch. See (`docs/adr/0005-event-broadcast-named-pipe.md`).
    WorkspaceChanged {
        /// The monitor the switch occurred on (index into the monitor stack).
        monitor: usize,
        /// The now-active workspace id.
        workspace: u32,
    },

    /// A window was relocated from one workspace to another.
    ///
    /// Carries the moved window and its `from`/`to` workspace ids. Emitted
    /// alongside [`WorkspaceChanged`](Self::WorkspaceChanged) by
    /// `move-to-workspace`, because that command performs both a contents
    /// mutation (the window moves) and a switch (the camera follows it). See
    /// (`docs/adr/0005-event-broadcast-named-pipe.md`).
    WindowMovedToWorkspace {
        /// The window that was moved.
        window: WindowDescriptor,
        /// The source workspace id (where the window came from).
        from: u32,
        /// The destination workspace id (where the window now lives).
        to: u32,
    },

    /// The focused window changed.
    ///
    /// Carries the now-focused window so a subscriber can render the active
    /// application without re-querying. See
    /// (`docs/adr/0005-event-broadcast-named-pipe.md`).
    FocusChanged {
        /// The monitor the focus change occurred on.
        monitor: usize,
        /// The now-active workspace id.
        workspace: u32,
        /// The now-focused window.
        window: WindowDescriptor,
    },

    /// The viewport scrolled along the infinite horizontal canvas.
    ///
    /// Carries the monitor and workspace it concerns, the new viewport
    /// offset (pixels along the canvas), and the total column count — enough
    /// for a subscriber to render a "column N of M" scroll indicator unique to
    /// flow-wm’s scrolling model (ADR-0005).
    ViewportScrolled {
        /// The monitor whose viewport scrolled.
        monitor: usize,
        /// The workspace whose viewport scrolled.
        workspace: u32,
        /// The new viewport offset in pixels along the horizontal canvas.
        offset: i32,
        /// The total number of columns on that workspace’s canvas.
        columns: usize,
    },

    /// A window’s tile/float decision flipped.
    ///
    /// Emitted on a successful `set-window`/`toggle-float` transition, with
    /// the window’s descriptor and its *new* state. See (`docs/adr/0005-event-broadcast-named-pipe.md`).
    TileStateChanged {
        /// The window whose state changed.
        window: WindowDescriptor,
        /// The window’s new tile/float state.
        state: TileState,
    },

    /// The daemon is shutting down gracefully.
    ///
    /// Pushed to every subscriber immediately before teardown, so a bar can
    /// render a “disconnected” state instead of freezing on a stale last
    /// frame. After this line the daemon closes the subscriber pipes, so the
    /// subscriber’s read returns EOF. A crash gives no such event — the
    /// subscriber detects it via EOF and follows the same reconnect path
    /// (ADR-0005).
    ApplicationExiting,
}

/// A window’s tile/float state, as carried on a [`Event::TileStateChanged`].
///
/// Serialized as `"tile"` / `"float"` (snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TileState {
    /// The window is tiled (managed by the scrolling tiling engine).
    Tile,
    /// The window is floating (free-floating above the tile layer).
    Float,
}

#[cfg(test)]
mod tests {
    use super::Event;
    use serde_json::json;

    /// Positive: `StateSnapshot` serializes with the flat `"type"` tag and the
    /// payload's keys as siblings — `{"type":"state_snapshot", <payload keys>}`.
    ///
    /// This is the wire contract: a subscriber reads one line, dispatches on
    /// `type`, and treats every other key as snapshot state. The
    /// `#[serde(flatten)]` on the payload is what spreads its keys into the
    /// event object rather than nesting them under a field.
    #[test]
    fn state_snapshot_serializes_with_flat_type_tag() {
        let event = Event::StateSnapshot {
            state: json!({
                "active_monitor": 0,
                "focused_window": null,
            }),
        };

        let wire = serde_json::to_string(&event).expect("serialize event");
        let parsed: serde_json::Value =
            serde_json::from_str(&wire).expect("wire is valid JSON");

        assert_eq!(parsed["type"], "state_snapshot", "wire: {wire}");
        assert_eq!(parsed["active_monitor"], 0, "payload keys are flattened in");
        assert!(
            parsed.get("focused_window").is_some(),
            "every payload key is present",
        );
        // The payload must NOT be nested under a `state` key — flatten spreads it.
        assert!(
            parsed.get("state").is_none(),
            "payload must be flattened, not nested under \"state\"",
        );
    }

    /// Positive: an empty-state snapshot still carries only the `type` tag.
    ///
    /// Guards the flatten path against the degenerate empty-object payload.
    #[test]
    fn state_snapshot_with_empty_payload_serializes_to_type_only() {
        let event = Event::StateSnapshot {
            state: json!({}),
        };
        let wire = serde_json::to_string(&event).expect("serialize empty snapshot");
        assert_eq!(wire, r#"{"type":"state_snapshot"}"#);
    }

    /// Positive: nested objects in the payload survive the flatten intact.
    ///
    /// The real snapshot payload contains a `monitors` array of objects;
    /// confirm flatten does not mangle nested structure.
    #[test]
    fn state_snapshot_preserves_nested_payload() {
        let event = Event::StateSnapshot {
            state: json!({
                "monitors": [
                    {"index": 0, "active_workspace": 1},
                ],
            }),
        };
        let wire = serde_json::to_string(&event).expect("serialize nested snapshot");
        let parsed: serde_json::Value = serde_json::from_str(&wire).expect("valid JSON");
        assert_eq!(parsed["type"], "state_snapshot");
        assert_eq!(parsed["monitors"][0]["active_workspace"], 1);
    }

    /// Positive: `WorkspaceChanged` serializes to the documented flat-tagged
    /// wire shape — `monitor` and `workspace` are siblings of `type`.
    #[test]
    fn workspace_changed_serializes_to_wire_shape() {
        let event = Event::WorkspaceChanged {
            monitor: 0,
            workspace: 2,
        };
        let wire = serde_json::to_string(&event).expect("serialize event");
        assert_eq!(
            wire,
            r#"{"type":"workspace_changed","monitor":0,"workspace":2}"#
        );
    }

    /// Positive: `WindowMovedToWorkspace` serializes to the documented
    /// flat-tagged wire shape — `window` is a nested object, `from`/`to` are
    /// siblings of `type`.
    #[test]
    fn window_moved_to_workspace_serializes_to_wire_shape() {
        let event = Event::WindowMovedToWorkspace {
            window: super::WindowDescriptor {
                hwnd: 123,
                title: "Notes".to_string(),
                exe: "notepad.exe".to_string(),
                class: "Notepad".to_string(),
            },
            from: 1,
            to: 4,
        };
        let wire = serde_json::to_string(&event).expect("serialize event");
        let parsed: serde_json::Value =
            serde_json::from_str(&wire).expect("wire is valid JSON");
        assert_eq!(parsed["type"], "window_moved_to_workspace");
        assert_eq!(parsed["from"], 1);
        assert_eq!(parsed["to"], 4);
        assert_eq!(parsed["window"]["hwnd"], 123);
        assert_eq!(parsed["window"]["title"], "Notes");
        assert_eq!(parsed["window"]["exe"], "notepad.exe");
        assert_eq!(parsed["window"]["class"], "Notepad");
    }

    /// Positive: `FocusChanged` serializes to the documented flat-tagged wire
    /// shape — `monitor` and `workspace` are siblings of `type`, and `window`
    /// is a nested `{ hwnd, title, exe, class }` object.
    #[test]
    fn focus_changed_serializes_to_wire_shape() {
        let event = Event::FocusChanged {
            monitor: 0,
            workspace: 1,
            window: super::WindowDescriptor {
                hwnd: 0x000a_0c20,
                title: "Terminal".into(),
                exe: "WindowsTerminal.exe".into(),
                class: "CascadiaTerminal".into(),
            },
        };
        let wire = serde_json::to_string(&event).expect("serialize event");
        let parsed: serde_json::Value =
            serde_json::from_str(&wire).expect("wire is valid JSON");
        assert_eq!(parsed["type"], "focus_changed", "wire: {wire}");
        assert_eq!(parsed["monitor"], 0);
        assert_eq!(parsed["workspace"], 1);
        assert_eq!(parsed["window"]["hwnd"], 0x000a_0c20);
        assert_eq!(parsed["window"]["title"], "Terminal");
        assert_eq!(parsed["window"]["exe"], "WindowsTerminal.exe");
        assert_eq!(parsed["window"]["class"], "CascadiaTerminal");
    }

    /// Positive: `ViewportScrolled` serializes to the documented flat-tagged
    /// wire shape — `monitor`, `workspace`, `offset`, and `columns` are
    /// siblings of `type`.
    #[test]
    fn viewport_scrolled_serializes_to_wire_shape() {
        let event = Event::ViewportScrolled {
            monitor: 0,
            workspace: 1,
            offset: 1280,
            columns: 4,
        };
        let wire = serde_json::to_string(&event).expect("serialize event");
        assert_eq!(
            wire,
            r#"{"type":"viewport_scrolled","monitor":0,"workspace":1,"offset":1280,"columns":4}"#
        );
    }

    /// Helper: build a representative window descriptor for wire-shape tests.
    fn sample_window() -> super::WindowDescriptor {
        super::WindowDescriptor {
            hwnd: 123,
            title: "Notes".to_string(),
            exe: "notes.exe".to_string(),
            class: "NotesCls".to_string(),
        }
    }

    /// Positive: `TileStateChanged` with the `float` state serializes to the
    /// documented wire shape — `window` nests its descriptor and `state` is
    /// the snake_case tag `"float"`.
    #[test]
    fn tile_state_changed_serializes_float() {
        let event = Event::TileStateChanged {
            window: sample_window(),
            state: super::TileState::Float,
        };
        let wire = serde_json::to_string(&event).expect("serialize event");
        assert_eq!(
            wire,
            r#"{"type":"tile_state_changed","window":{"hwnd":123,"title":"Notes","exe":"notes.exe","class":"NotesCls"},"state":"float"}"#
        );
    }

    /// Positive: `TileStateChanged` with the `tile` state serializes `state`
    /// as `"tile"`.
    #[test]
    fn tile_state_changed_serializes_tile() {
        let event = Event::TileStateChanged {
            window: sample_window(),
            state: super::TileState::Tile,
        };
        let wire = serde_json::to_string(&event).expect("serialize event");
        assert!(wire.contains(r#""type":"tile_state_changed""#));
        assert!(wire.contains(r#""state":"tile""#));
    }

    /// Positive: `ApplicationExiting` is a unit variant — it serializes to
    /// just the flat `type` tag, with no payload keys.
    #[test]
    fn application_exiting_serializes_to_type_only() {
        let wire = serde_json::to_string(&Event::ApplicationExiting).expect("serialize event");
        assert_eq!(wire, r#"{"type":"application_exiting"}"#);
    }
}
