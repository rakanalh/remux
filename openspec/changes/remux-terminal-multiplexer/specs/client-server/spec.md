## ADDED Requirements

### Requirement: Server daemon
The system SHALL run a server process as a daemon that persists after the client detaches. The server SHALL manage all sessions, PTYs, and state. The server SHALL listen on a Unix socket at /tmp/remux-$UID/remux.sock.

#### Scenario: Start server
- **WHEN** the user runs `remux` and no server is running
- **THEN** a server daemon is started and a client attaches to it

#### Scenario: Server already running
- **WHEN** the user runs `remux` and a server is already running
- **THEN** the client attaches to the existing server without starting a new one

### Requirement: Client attach/detach
The system SHALL allow multiple clients to attach to the same server. Each client SHALL be able to attach to a specific session. Detaching SHALL leave the session running on the server.

#### Scenario: Detach
- **WHEN** the user detaches from a session
- **THEN** the client process exits, the terminal is restored, and all session processes continue running on the server

#### Scenario: Multiple clients
- **WHEN** two clients attach to the same session
- **THEN** both see the same content and input from either client is processed

### Requirement: Client-server protocol
The system SHALL use a JSON-serialized message protocol over the Unix socket for client-server communication. Messages SHALL include: input events, render updates, session commands, and resize notifications.

#### Scenario: Client sends input
- **WHEN** the user presses a key
- **THEN** the client sends an Input message to the server

#### Scenario: Server sends render update
- **WHEN** a pane's content changes
- **THEN** the server sends a Render message to all attached clients with the diff

### Requirement: CLI commands
The system SHALL provide the following CLI commands: `remux` (start/attach), `remux new -s <name> [-f <folder>]` (create session), `remux attach <name>` (attach to session), `remux ls` (list sessions), `remux kill <name>` (kill session).

#### Scenario: List sessions
- **WHEN** the user runs `remux ls`
- **THEN** all sessions are listed with their folder, tab count, and attached client count

#### Scenario: Create session in folder
- **WHEN** the user runs `remux new -s backend -f Work`
- **THEN** a session named "backend" is created in the "Work" folder
