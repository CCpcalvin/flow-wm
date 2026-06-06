//! Daemon-side command dispatcher.
//!
//! Maps [`SocketMessage`] commands to [`SocketResponse`] results.
//! The base [`dispatch`] function is platform-independent. The
//! [`dispatch_with_registry`] function (Windows-only) handles commands that
//! require access to the [`WindowRegistry`](crate::registry::WindowRegistry).

use super::message::{SocketMessage, SocketResponse};

/// Dispatch a single command and return the response.
///
/// This function will grow as more commands are implemented.
/// Unknown or unimplemented commands return an error response.
pub fn dispatch(msg: &SocketMessage) -> SocketResponse {
    match msg {
        SocketMessage::Stop => SocketResponse::Ok,

        SocketMessage::ReloadConfig => unimplemented_command("reload_config"),
        SocketMessage::CheckConfig => unimplemented_command("check_config"),
        SocketMessage::FocusLeft => unimplemented_command("focus_left"),
        SocketMessage::FocusRight => unimplemented_command("focus_right"),
        SocketMessage::FocusUp => unimplemented_command("focus_up"),
        SocketMessage::FocusDown => unimplemented_command("focus_down"),
        SocketMessage::SwapLeft => unimplemented_command("swap_left"),
        SocketMessage::SwapRight => unimplemented_command("swap_right"),
        SocketMessage::SwapUp => unimplemented_command("swap_up"),
        SocketMessage::SwapDown => unimplemented_command("swap_down"),
        SocketMessage::SwapWithOffscreen { .. } => unimplemented_command("swap_with_offscreen"),
        SocketMessage::ScrollLeft => unimplemented_command("scroll_left"),
        SocketMessage::ScrollRight => unimplemented_command("scroll_right"),
        SocketMessage::ExpandColumn => unimplemented_command("expand_column"),
        SocketMessage::ShrinkColumn => unimplemented_command("shrink_column"),
        SocketMessage::SetColumnWidth { .. } => unimplemented_command("set_column_width"),
        SocketMessage::ToggleFloat => unimplemented_command("toggle_float"),
        SocketMessage::ToggleMonocle => unimplemented_command("toggle_monocle"),
        SocketMessage::PlaceAbove => unimplemented_command("place_above"),
        SocketMessage::Promote => unimplemented_command("promote"),
        SocketMessage::CloseWindow => unimplemented_command("close_window"),
        SocketMessage::QueryWindowsAll => unimplemented_command("query_windows_all"),
        SocketMessage::QueryLayoutVirtual => unimplemented_command("query_layout_virtual"),
        SocketMessage::QueryLayoutActual => unimplemented_command("query_layout_actual"),
        SocketMessage::QueryState => unimplemented_command("query_state"),
        SocketMessage::SetConfigValue { .. } => unimplemented_command("set_config_value"),
        SocketMessage::ForgetApp { .. } => unimplemented_command("forget_app"),
        SocketMessage::ForgetAllApps => unimplemented_command("forget_all_apps"),
    }
}

/// Dispatch a command that requires access to the window registry.
///
/// Handles `QueryWindowsAll` by serialising the registry state to JSON,
/// and falls through to [`dispatch`] for all other commands.
///
/// This function is Windows-only because the registry depends on Win32 APIs.
#[cfg(target_os = "windows")]
pub fn dispatch_with_registry(
    msg: &SocketMessage,
    registry: &std::sync::Arc<std::sync::Mutex<crate::registry::WindowRegistry>>,
) -> SocketResponse {
    match msg {
        SocketMessage::QueryWindowsAll => match registry.lock() {
            Ok(reg) => {
                let payload = reg.to_json_value();
                SocketResponse::Data { payload }
            }
            Err(e) => SocketResponse::Error {
                message: format!("registry lock failed: {e}"),
            },
        },
        _ => dispatch(msg),
    }
}

/// Return a standard "not yet implemented" error response.
fn unimplemented_command(name: &str) -> SocketResponse {
    SocketResponse::Error {
        message: format!("command '{name}' is not yet implemented"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Direction;

    // Positive: Stop command returns Ok
    #[test]
    fn dispatch_stop_returns_ok() {
        let response = dispatch(&SocketMessage::Stop);
        assert_eq!(response, SocketResponse::Ok);
    }

    // Positive: unimplemented commands return Error with message
    #[test]
    fn dispatch_reload_config_returns_error() {
        let response = dispatch(&SocketMessage::ReloadConfig);
        match response {
            SocketResponse::Error { message } => {
                assert!(message.contains("reload_config"));
                assert!(message.contains("not yet implemented"));
            }
            _ => panic!("expected Error response, got: {response:?}"),
        }
    }

    // Positive: all focus commands return unimplemented error
    #[test]
    fn dispatch_focus_commands_return_error() {
        let commands = [
            SocketMessage::FocusLeft,
            SocketMessage::FocusRight,
            SocketMessage::FocusUp,
            SocketMessage::FocusDown,
        ];
        for cmd in &commands {
            let response = dispatch(cmd);
            assert!(
                matches!(response, SocketResponse::Error { .. }),
                "expected Error for {cmd:?}"
            );
        }
    }

    // Positive: all swap commands return unimplemented error
    #[test]
    fn dispatch_swap_commands_return_error() {
        let commands = vec![
            SocketMessage::SwapLeft,
            SocketMessage::SwapRight,
            SocketMessage::SwapUp,
            SocketMessage::SwapDown,
            SocketMessage::SwapWithOffscreen {
                direction: Direction::Left,
            },
        ];
        for cmd in &commands {
            let response = dispatch(cmd);
            assert!(
                matches!(response, SocketResponse::Error { .. }),
                "expected Error for {cmd:?}"
            );
        }
    }

    // Positive: all scroll commands return unimplemented error
    #[test]
    fn dispatch_scroll_commands_return_error() {
        let commands = [SocketMessage::ScrollLeft, SocketMessage::ScrollRight];
        for cmd in &commands {
            let response = dispatch(cmd);
            assert!(
                matches!(response, SocketResponse::Error { .. }),
                "expected Error for {cmd:?}"
            );
        }
    }

    // Positive: all column resize commands return unimplemented error
    #[test]
    fn dispatch_column_commands_return_error() {
        let commands = vec![
            SocketMessage::ExpandColumn,
            SocketMessage::ShrinkColumn,
            SocketMessage::SetColumnWidth { eighths: 4 },
        ];
        for cmd in &commands {
            let response = dispatch(cmd);
            assert!(
                matches!(response, SocketResponse::Error { .. }),
                "expected Error for {cmd:?}"
            );
        }
    }

    // Positive: all window state commands return unimplemented error
    #[test]
    fn dispatch_window_state_commands_return_error() {
        let commands = vec![
            SocketMessage::ToggleFloat,
            SocketMessage::ToggleMonocle,
            SocketMessage::PlaceAbove,
            SocketMessage::Promote,
            SocketMessage::CloseWindow,
        ];
        for cmd in &commands {
            let response = dispatch(cmd);
            assert!(
                matches!(response, SocketResponse::Error { .. }),
                "expected Error for {cmd:?}"
            );
        }
    }

    // Positive: all query commands return unimplemented error
    #[test]
    fn dispatch_query_commands_return_error() {
        let commands = vec![
            SocketMessage::QueryWindowsAll,
            SocketMessage::QueryLayoutVirtual,
            SocketMessage::QueryLayoutActual,
            SocketMessage::QueryState,
        ];
        for cmd in &commands {
            let response = dispatch(cmd);
            assert!(
                matches!(response, SocketResponse::Error { .. }),
                "expected Error for {cmd:?}"
            );
        }
    }

    // Positive: all config mutation commands return unimplemented error
    #[test]
    fn dispatch_config_commands_return_error() {
        let commands = vec![
            SocketMessage::SetConfigValue {
                key: "inner_gap".into(),
                value: serde_json::json!(10),
            },
            SocketMessage::ForgetApp {
                exe: "firefox.exe".into(),
            },
            SocketMessage::ForgetAllApps,
        ];
        for cmd in &commands {
            let response = dispatch(cmd);
            assert!(
                matches!(response, SocketResponse::Error { .. }),
                "expected Error for {cmd:?}"
            );
        }
    }

    // Positive: CheckConfig returns error with correct command name
    #[test]
    fn dispatch_check_config_returns_error() {
        let response = dispatch(&SocketMessage::CheckConfig);
        match response {
            SocketResponse::Error { message } => {
                assert!(message.contains("check_config"));
            }
            _ => panic!("expected Error response"),
        }
    }

    // Positive: every unimplemented response includes the command name
    #[test]
    fn unimplemented_includes_command_name() {
        let all_unimplemented = [
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
            SocketMessage::SwapWithOffscreen {
                direction: Direction::Down,
            },
            SocketMessage::ScrollLeft,
            SocketMessage::ScrollRight,
            SocketMessage::ExpandColumn,
            SocketMessage::ShrinkColumn,
            SocketMessage::SetColumnWidth { eighths: 8 },
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
                key: "k".into(),
                value: serde_json::json!(null),
            },
            SocketMessage::ForgetApp {
                exe: "a.exe".into(),
            },
            SocketMessage::ForgetAllApps,
        ];
        for cmd in &all_unimplemented {
            match dispatch(cmd) {
                SocketResponse::Error { message } => {
                    assert!(
                        message.contains("not yet implemented"),
                        "expected 'not yet implemented' in: {message}"
                    );
                }
                other => panic!("expected Error for {:?}, got: {other:?}", cmd),
            }
        }
    }

    // Negative: Stop does NOT return Error
    #[test]
    fn dispatch_stop_does_not_return_error() {
        let response = dispatch(&SocketMessage::Stop);
        assert!(!matches!(response, SocketResponse::Error { .. }));
    }

    // Negative: unimplemented does not return Ok
    #[test]
    fn dispatch_unimplemented_does_not_return_ok() {
        let response = dispatch(&SocketMessage::FocusLeft);
        assert!(!matches!(response, SocketResponse::Ok));
    }

    // Negative: unimplemented does not return Data
    #[test]
    fn dispatch_unimplemented_does_not_return_data() {
        let response = dispatch(&SocketMessage::ReloadConfig);
        assert!(!matches!(response, SocketResponse::Data { .. }));
    }
}
