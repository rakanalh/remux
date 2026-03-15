## MODIFIED Requirements

### Requirement: Client-server protocol
The system SHALL use a JSON-serialized message protocol over the Unix socket for client-server communication. Messages SHALL include: input events, render updates, session commands, resize notifications, and mouse events.

#### Scenario: Client sends input
- **WHEN** the user presses a key
- **THEN** the client sends an Input message to the server

#### Scenario: Server sends render update
- **WHEN** a pane's content changes
- **THEN** the server sends a Render message to all attached clients with the diff

#### Scenario: Client sends mouse click
- **WHEN** the user clicks the mouse
- **THEN** the client sends a MouseClick message to the server with the click coordinates (x, y)

#### Scenario: Client sends mouse drag
- **WHEN** the user drags the mouse to select text
- **THEN** the client sends a MouseDrag message to the server with start and end coordinates

#### Scenario: Server sends clipboard data
- **WHEN** the server resolves a text selection from a mouse drag
- **THEN** the server sends a CopyToClipboard message to the client with the selected text, and the client writes it to the system clipboard via OSC 52
