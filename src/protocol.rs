use serde::{Deserialize, Serialize};

/// Unique identifier for a pane within the server.
///
/// This is defined here to avoid a circular dependency on `server::layout::PaneId`
/// while other modules are still being developed.
pub type PaneId = u64;

// ---------------------------------------------------------------------------
// Version handshake
// ---------------------------------------------------------------------------

/// The wire protocol version. Bump when a breaking change is made to the
/// framed message shapes exchanged between client and server.
pub const PROTOCOL_VERSION: u32 = 1;

/// First frame sent by a connecting client, announcing its protocol/build.
///
/// FROZEN WIRE SHAPE — never rename/remove/retype existing fields; only add
/// `#[serde(default)]` optional fields. This is the one message exchanged
/// before version-compatible messaging is established, so a version skew must
/// be detectable here rather than crashing mid-session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub remux_version: String,
}

/// First frame sent by the server in response to a `Hello`.
///
/// FROZEN WIRE SHAPE — never rename/remove/retype existing fields; only add
/// `#[serde(default)]` optional fields. This is the one message exchanged
/// before version-compatible messaging is established, so a version skew must
/// be detectable here rather than crashing mid-session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol_version: u32,
    pub remux_version: String,
}

// ---------------------------------------------------------------------------
// Client -> Server
// ---------------------------------------------------------------------------

/// Messages sent from a Remux client to the server over the Unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Attach to an existing session by name.
    Attach { session_name: String },
    /// Detach from the currently attached session.
    Detach,
    /// Send raw input bytes to the active pane's PTY.
    Input { data: Vec<u8> },
    /// Notify the server that the client terminal was resized.
    Resize { cols: u16, rows: u16 },
    /// Execute a command (typically triggered from Normal-mode keybindings).
    Command(RemuxCommand),
    /// Create a new session, optionally inside a folder.
    CreateSession {
        name: String,
        folder: Option<String>,
    },
    /// Request the list of active sessions.
    ListSessions,
    /// Kill (destroy) a session by name.
    KillSession { name: String },
    /// Notify the server that the client's input mode changed.
    ModeChanged { mode: String },
    /// A mouse click at the given screen coordinates.
    MouseClick { x: u16, y: u16 },
    /// A mouse drag selection from start to end screen coordinates.
    MouseDrag {
        start_x: u16,
        start_y: u16,
        end_x: u16,
        end_y: u16,
        /// `true` when the mouse button was released (final drag event).
        is_final: bool,
    },
    /// A mouse wheel event at the given full-screen 0-based coordinates.
    /// `up` is true for wheel-up, false for wheel-down. The server decides
    /// whether to forward it to the pane's application or scroll remux.
    MouseScroll { x: u16, y: u16, up: bool },
    /// Request the scrollback content for the active pane (for search).
    RequestScrollback,
    /// Send search match info to the server for status bar display.
    SearchInfo { current: usize, total: usize },
    /// Request the full session tree (folders, sessions, tabs, panes).
    ListSessionTree,
    /// Scroll the focused pane by delta lines (positive = up/back, negative = down/forward).
    /// The server owns the scroll offset and clamps it to valid range.
    ScrollDelta { delta: i32 },
    /// Reset scroll to live view (offset 0).
    ScrollReset,
    /// Request scrollback info (total line count) for the active pane.
    RequestScrollbackInfo,
}

// ---------------------------------------------------------------------------
// Server -> Client
// ---------------------------------------------------------------------------

/// Messages sent from the Remux server to a connected client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Full screen render (sent on attach or after a major layout change).
    FullRender {
        cells: Vec<Vec<RenderCell>>,
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
        /// The focused pane's rectangle in the composited buffer.
        focused_pane_rect: Option<PaneRect>,
        /// Whether the focused pane has application cursor keys (DECCKM) active.
        #[serde(default)]
        application_cursor_keys: bool,
        /// Index in the combined scrollback+grid buffer of the first displayed line.
        #[serde(default)]
        viewport_top: usize,
    },
    /// Incremental render update (diff from previous frame).
    RenderDiff {
        changes: Vec<CellChange>,
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
        /// The focused pane's rectangle in the composited buffer.
        focused_pane_rect: Option<PaneRect>,
        /// Whether the focused pane has application cursor keys (DECCKM) active.
        #[serde(default)]
        application_cursor_keys: bool,
        /// Index in the combined scrollback+grid buffer of the first displayed line.
        #[serde(default)]
        viewport_top: usize,
    },
    /// Optimized scroll render: shift content within a pane rect and render
    /// only the new rows that appeared.
    ScrollRender {
        /// Pane content area to scroll within.
        pane_x: u16,
        pane_y: u16,
        pane_width: u16,
        pane_height: u16,
        /// Rows to scroll. Positive = content moves UP (new rows at top).
        /// Negative = content moves DOWN (new rows at bottom).
        delta: i16,
        /// The new rows to render. Length = abs(delta).
        new_rows: Vec<Vec<RenderCell>>,
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
        cursor_style: u8,
        focused_pane_rect: Option<PaneRect>,
        application_cursor_keys: bool,
        /// Index in the combined scrollback+grid buffer of the first displayed line.
        #[serde(default)]
        viewport_top: usize,
    },
    /// Response to a `ListSessions` request.
    SessionList { sessions: Vec<SessionListEntry> },
    /// An error response.
    Error { message: String },
    /// Asynchronous session event notification.
    Event(SessionEvent),
    /// Request the client to copy data to the system clipboard via OSC 52.
    CopyToClipboard { data: String },
    /// Response to a `RequestScrollback` request with the pane's text content.
    ScrollbackContent { lines: Vec<String> },
    /// Response to a `RequestScrollbackInfo` request with the total line count.
    ScrollbackInfo { total_lines: usize },
    /// Response to a `ListSessionTree` request with the full hierarchy.
    SessionTree {
        folders: Vec<FolderTreeEntry>,
        unfiled: Vec<SessionTreeEntry>,
    },
}

// ---------------------------------------------------------------------------
// Pane geometry (sent from server to client for scoped visual mode)
// ---------------------------------------------------------------------------

/// Rectangle describing a focused pane's position and size in the composited
/// screen buffer. Sent alongside render messages so the client can scope
/// visual-mode selection to the active pane.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

// ---------------------------------------------------------------------------
// Rendering primitives
// ---------------------------------------------------------------------------

/// Serde default for [`RenderCell::width`]. Older peers that omit the field
/// decode as normal (single-column) width.
fn default_cell_width() -> u8 {
    1
}

/// A single cell in the rendered terminal grid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderCell {
    pub c: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Display width in columns: `1` normal, `2` wide lead (CJK/emoji), `0`
    /// continuation cell placed after a wide lead. Defaults to `1` on the wire
    /// for back-compat with peers that predate the field.
    #[serde(default = "default_cell_width")]
    pub width: u8,
    /// Zero-width combining marks composed onto the base glyph `c`. Empty in the
    /// overwhelmingly common case; `skip_serializing_if` keeps empty cells at
    /// zero wire bytes so ASCII rendering is unchanged on the wire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub combining: Vec<char>,
}

impl Default for RenderCell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: CellColor::Default,
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
            width: 1,
            combining: Vec::new(),
        }
    }
}

/// A single changed cell for diff-based rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellChange {
    pub x: u16,
    pub y: u16,
    pub cell: RenderCell,
}

/// Terminal cell color representation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CellColor {
    /// Use the terminal's default foreground/background.
    Default,
    /// Standard 256-color palette index.
    Indexed(u8),
    /// True-color RGB value.
    Rgb(u8, u8, u8),
}

// ---------------------------------------------------------------------------
// Session tree entries (for session manager)
// ---------------------------------------------------------------------------

/// A folder containing sessions in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderTreeEntry {
    pub name: String,
    pub sessions: Vec<SessionTreeEntry>,
}

/// A session entry in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTreeEntry {
    pub name: String,
    pub tabs: Vec<TabTreeEntry>,
    pub client_count: usize,
    pub is_current: bool,
}

/// A tab entry in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabTreeEntry {
    pub id: u64,
    pub name: String,
    pub panes: Vec<PaneTreeEntry>,
}

/// A pane entry in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneTreeEntry {
    pub id: u64,
    pub name: String,
    pub is_focused: bool,
}

// ---------------------------------------------------------------------------
// Session metadata
// ---------------------------------------------------------------------------

/// Entry returned in a session list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub name: String,
    pub folder: Option<String>,
    pub tab_count: usize,
    pub client_count: usize,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// All commands that can be executed within Remux, either from keybindings or
/// the command line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RemuxCommand {
    // -- Tab commands -------------------------------------------------------
    TabNew,
    TabClose,
    TabRename(String),
    TabGoto(usize),
    TabNext,
    TabPrev,
    TabMove(usize),

    // -- Pane commands ------------------------------------------------------
    PaneNew,
    PaneClose,
    PaneSplitVertical,
    PaneSplitHorizontal,
    PaneFocusLeft,
    PaneFocusRight,
    PaneFocusUp,
    PaneFocusDown,
    PaneStackAdd,
    PaneStackNext,
    PaneStackPrev,
    PaneMoveLeft,
    PaneMoveRight,
    PaneMoveUp,
    PaneMoveDown,
    PaneRename(String),
    PaneToggleZoom,

    // -- Resize commands ----------------------------------------------------
    ResizeLeft(u16),
    ResizeRight(u16),
    ResizeUp(u16),
    ResizeDown(u16),

    // -- Session commands ---------------------------------------------------
    SessionNew {
        name: String,
        folder: Option<String>,
    },
    SessionDetach,
    SessionRename(String),
    SessionList,

    // -- Folder commands ----------------------------------------------------
    FolderNew(String),
    FolderDelete(String),
    FolderList,
    FolderMoveSession {
        session: String,
        folder: Option<String>,
    },

    // -- Buffer commands ----------------------------------------------------
    BufferEditInEditor,

    // -- Layout commands ------------------------------------------------------
    ToggleStyle,
    LayoutNext,
    SetMaster,

    // -- System / mode commands ---------------------------------------------
    SessionSave,
    EnterNormal,
    EnterCommandMode,
    EnterVisualMode,
    /// Send raw key bytes to the active pane's PTY (used for leader-leader normal mode).
    SendKey(Vec<u8>),
    /// Enter search mode (client-side mode transition).
    EnterSearchMode,
    /// Open the session manager (client-side mode transition).
    OpenSessionManager,
    /// Connect to a remote (client-side command). The argument is either an
    /// SSH destination (`user@host` or an `~/.ssh/config` Host alias) or the
    /// name of a remote already declared in `[remotes]`. Opens the session
    /// manager and makes the remote and its sessions visible.
    RemoteConnect(String),
    /// Open folder selection popup to move current session (client-side only).
    SessionMoveToFolder,
    /// Switch to a specific tab in a specific session.
    SessionSwitchTab {
        session: String,
        tab_index: usize,
    },
    /// Switch to a specific pane in a specific session and tab.
    SessionSwitchPane {
        session: String,
        tab_index: usize,
        pane_id: u64,
    },
    /// Close a tab by index in a specific session.
    TabCloseByIndex {
        session: String,
        tab_index: usize,
    },
}

// ---------------------------------------------------------------------------
// Command name enumeration
// ---------------------------------------------------------------------------

/// Return the list of all recognised command names (PascalCase strings that
/// [`crate::config::keybindings::parse_command`] accepts). Commands that
/// take parameters include a hint suffix after a space.
pub fn command_names() -> Vec<(&'static str, Option<&'static str>)> {
    vec![
        ("TabNew", None),
        ("TabClose", None),
        ("TabRename", Some("<name>")),
        ("TabGoto", Some("<index>")),
        ("TabNext", None),
        ("TabPrev", None),
        ("TabMove", Some("<index>")),
        ("PaneNew", None),
        ("PaneClose", None),
        ("PaneSplitVertical", None),
        ("PaneSplitHorizontal", None),
        ("PaneFocusLeft", None),
        ("PaneFocusRight", None),
        ("PaneFocusUp", None),
        ("PaneFocusDown", None),
        ("PaneStackAdd", None),
        ("PaneStackNext", None),
        ("PaneStackPrev", None),
        ("PaneMoveLeft", None),
        ("PaneMoveRight", None),
        ("PaneMoveUp", None),
        ("PaneMoveDown", None),
        ("PaneRename", Some("<name>")),
        ("PaneToggleZoom", None),
        ("ResizeLeft", Some("<amount>")),
        ("ResizeRight", Some("<amount>")),
        ("ResizeUp", Some("<amount>")),
        ("ResizeDown", Some("<amount>")),
        ("SessionNew", Some("<name> [folder]")),
        ("SessionDetach", None),
        ("SessionRename", Some("<name>")),
        ("SessionList", None),
        ("SessionSave", None),
        ("FolderNew", Some("<name>")),
        ("FolderDelete", Some("<name>")),
        ("FolderList", None),
        ("FolderMoveSession", Some("<session> [folder]")),
        ("BufferEditInEditor", None),
        ("OpenSessionManager", None),
        ("RemoteConnect", Some("<user@host|alias>")),
        ("SessionMoveToFolder", None),
        ("ToggleStyle", None),
        ("LayoutNext", None),
        ("SetMaster", None),
        ("EnterNormal", None),
        ("EnterCommandMode", None),
        ("EnterVisualMode", None),
    ]
}

// ---------------------------------------------------------------------------
// Session events
// ---------------------------------------------------------------------------

/// Asynchronous events that the server pushes to connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    SessionCreated(String),
    SessionDeleted(String),
    PaneExited { pane_id: PaneId, exit_code: i32 },
}

// ---------------------------------------------------------------------------
// Wire format helpers -- length-prefixed JSON over Unix sockets
//
// Frame layout: [4 bytes big-endian payload length][JSON payload]
// ---------------------------------------------------------------------------

/// Serialize a message into a length-prefixed JSON frame.
pub fn encode_message<T: Serialize>(msg: &T) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)?;
    log::trace!("protocol: encode_message bytes={}", json.len());
    let len = (json.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Read the payload length from a 4-byte big-endian header.
pub fn decode_message_length(header: &[u8; 4]) -> usize {
    let len = u32::from_be_bytes(*header) as usize;
    log::trace!("protocol: decode_message_length bytes={}", len);
    len
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_client_message() {
        let msg = ClientMessage::Attach {
            session_name: "main".into(),
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::Attach { session_name } => assert_eq!(session_name, "main"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_server_message() {
        let msg = ServerMessage::Error {
            message: "not found".into(),
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ServerMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ServerMessage::Error { message } => assert_eq!(message, "not found"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn render_cell_width_defaults_to_one_when_absent() {
        // A peer that predates the `width` field omits it on the wire; it must
        // decode as normal (single-column) width via the serde default.
        let json = r#"{"c":"a","fg":"Default","bg":"Default","bold":false,"italic":false,"underline":false}"#;
        let cell: RenderCell = serde_json::from_str(json).unwrap();
        assert_eq!(cell.width, 1);

        // A present width field is preserved through a round trip.
        let wide = RenderCell {
            c: '中',
            width: 2,
            ..RenderCell::default()
        };
        let encoded = serde_json::to_string(&wide).unwrap();
        let decoded: RenderCell = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.width, 2);
    }

    #[test]
    fn round_trip_command() {
        let msg = ClientMessage::Command(RemuxCommand::TabNew);
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::Command(RemuxCommand::TabNew) => {}
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn encode_length_is_correct() {
        let msg = ClientMessage::Detach;
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        assert_eq!(len, encoded.len() - 4);
    }

    #[test]
    fn render_cell_default() {
        let cell = RenderCell::default();
        assert_eq!(cell.c, ' ');
        assert_eq!(cell.fg, CellColor::Default);
        assert!(!cell.bold);
        assert!(cell.combining.is_empty());
    }

    #[test]
    fn render_cell_empty_combining_is_not_serialized() {
        // The overwhelmingly common case (no combining marks) must add zero wire
        // bytes: `skip_serializing_if` omits the field from the JSON entirely.
        let cell = RenderCell::default();
        let json = serde_json::to_string(&cell).unwrap();
        assert!(
            !json.contains("combining"),
            "empty combining must be skipped, got: {json}"
        );

        // Absent on the wire decodes back to an empty vec via serde default.
        let decoded: RenderCell = serde_json::from_str(&json).unwrap();
        assert!(decoded.combining.is_empty());
    }

    #[test]
    fn render_cell_combining_round_trips() {
        // A decomposed `é` (base 'e' + U+0301) round-trips with its marks.
        let cell = RenderCell {
            c: 'e',
            combining: vec!['\u{301}'],
            ..RenderCell::default()
        };
        let json = serde_json::to_string(&cell).unwrap();
        assert!(json.contains("combining"));
        let decoded: RenderCell = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.c, 'e');
        assert_eq!(decoded.combining, vec!['\u{301}']);
    }

    #[test]
    fn round_trip_mouse_click() {
        let msg = ClientMessage::MouseClick { x: 42, y: 10 };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::MouseClick { x, y } => {
                assert_eq!(x, 42);
                assert_eq!(y, 10);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_mouse_drag() {
        let msg = ClientMessage::MouseDrag {
            start_x: 5,
            start_y: 3,
            end_x: 20,
            end_y: 7,
            is_final: false,
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::MouseDrag {
                start_x,
                start_y,
                end_x,
                end_y,
                is_final,
            } => {
                assert_eq!(start_x, 5);
                assert_eq!(start_y, 3);
                assert_eq!(end_x, 20);
                assert_eq!(end_y, 7);
                assert!(!is_final);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_list_session_tree() {
        let msg = ClientMessage::ListSessionTree;
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        assert!(matches!(decoded, ClientMessage::ListSessionTree));
    }

    #[test]
    fn round_trip_session_tree() {
        let msg = ServerMessage::SessionTree {
            folders: vec![FolderTreeEntry {
                name: "work".to_string(),
                sessions: vec![SessionTreeEntry {
                    name: "proj".to_string(),
                    tabs: vec![TabTreeEntry {
                        id: 1,
                        name: "Tab 1".to_string(),
                        panes: vec![PaneTreeEntry {
                            id: 10,
                            name: "zsh".to_string(),
                            is_focused: true,
                        }],
                    }],
                    client_count: 1,
                    is_current: true,
                }],
            }],
            unfiled: vec![SessionTreeEntry {
                name: "scratch".to_string(),
                tabs: vec![],
                client_count: 0,
                is_current: false,
            }],
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ServerMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ServerMessage::SessionTree { folders, unfiled } => {
                assert_eq!(folders.len(), 1);
                assert_eq!(folders[0].name, "work");
                assert_eq!(folders[0].sessions[0].name, "proj");
                assert!(folders[0].sessions[0].is_current);
                assert_eq!(unfiled.len(), 1);
                assert_eq!(unfiled[0].name, "scratch");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_session_switch_tab() {
        let msg = ClientMessage::Command(RemuxCommand::SessionSwitchTab {
            session: "main".to_string(),
            tab_index: 2,
        });
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::Command(RemuxCommand::SessionSwitchTab { session, tab_index }) => {
                assert_eq!(session, "main");
                assert_eq!(tab_index, 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_session_switch_pane() {
        let msg = ClientMessage::Command(RemuxCommand::SessionSwitchPane {
            session: "dev".to_string(),
            tab_index: 0,
            pane_id: 42,
        });
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ClientMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ClientMessage::Command(RemuxCommand::SessionSwitchPane {
                session,
                tab_index,
                pane_id,
            }) => {
                assert_eq!(session, "dev");
                assert_eq!(tab_index, 0);
                assert_eq!(pane_id, 42);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_hello_welcome() {
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            remux_version: "1.2.3".into(),
        };
        let encoded = encode_message(&hello).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: Hello = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert_eq!(decoded.remux_version, "1.2.3");

        let welcome = Welcome {
            protocol_version: PROTOCOL_VERSION,
            remux_version: "1.2.3".into(),
        };
        let encoded = encode_message(&welcome).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: Welcome = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn round_trip_copy_to_clipboard() {
        let msg = ServerMessage::CopyToClipboard {
            data: "hello world".to_string(),
        };
        let encoded = encode_message(&msg).unwrap();
        let len = decode_message_length(encoded[..4].try_into().unwrap());
        let decoded: ServerMessage = serde_json::from_slice(&encoded[4..4 + len]).unwrap();
        match decoded {
            ServerMessage::CopyToClipboard { data } => {
                assert_eq!(data, "hello world");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
