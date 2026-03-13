use serde::{Deserialize, Serialize};

/// Unique identifier for a pane within the server.
///
/// This is defined here to avoid a circular dependency on `server::layout::PaneId`
/// while other modules are still being developed.
pub type PaneId = u64;

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
    },
    /// Incremental render update (diff from previous frame).
    RenderDiff {
        changes: Vec<CellChange>,
        cursor_x: u16,
        cursor_y: u16,
        cursor_visible: bool,
    },
    /// Response to a `ListSessions` request.
    SessionList { sessions: Vec<SessionListEntry> },
    /// An error response.
    Error { message: String },
    /// Asynchronous session event notification.
    Event(SessionEvent),
}

// ---------------------------------------------------------------------------
// Rendering primitives
// ---------------------------------------------------------------------------

/// A single cell in the rendered terminal grid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderCell {
    pub c: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
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
    BufferSearch,

    // -- System / mode commands ---------------------------------------------
    SessionSave,
    EnterInsertMode,
    EnterNormalMode,
    EnterVisualMode,
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
    let len = (json.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Read the payload length from a 4-byte big-endian header.
pub fn decode_message_length(header: &[u8; 4]) -> usize {
    u32::from_be_bytes(*header) as usize
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
    }
}
