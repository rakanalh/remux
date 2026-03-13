## ADDED Requirements

### Requirement: PTY allocation and child process spawning
The system SHALL allocate a new PTY pair (master/slave) for each pane and spawn a shell process attached to the slave end. The default shell SHALL be read from the SHELL environment variable, falling back to /bin/sh.

#### Scenario: New pane spawns shell
- **WHEN** a new pane is created
- **THEN** a PTY pair is allocated, a child process is spawned with the slave PTY as its controlling terminal, and the pane is associated with the master PTY fd

#### Scenario: Custom shell
- **WHEN** a pane is created with a specific command
- **THEN** that command is executed instead of the default shell

### Requirement: PTY I/O multiplexing
The system SHALL read output from each pane's master PTY asynchronously via tokio tasks and feed it into the pane's VT parser. Input from the client SHALL be written to the active pane's master PTY.

#### Scenario: Child process produces output
- **WHEN** a child process writes to stdout/stderr
- **THEN** the output is read from the master PTY and fed into the pane's screen buffer via the VTE parser

#### Scenario: User types in Insert mode
- **WHEN** the user presses a key in Insert mode
- **THEN** the raw key bytes are written to the active pane's master PTY

### Requirement: PTY resize
The system SHALL send SIGWINCH and update the PTY window size when a pane's dimensions change (terminal resize, split adjustment).

#### Scenario: Terminal window resized
- **WHEN** the terminal emulator window is resized
- **THEN** all visible panes have their PTY window size updated and receive SIGWINCH

### Requirement: Child process lifecycle
The system SHALL detect when a child process exits and mark the pane as dead. Dead panes SHALL display the exit status and be closeable by the user.

#### Scenario: Shell exits
- **WHEN** a child process exits with status 0
- **THEN** the pane displays "[Process exited: 0]" and can be closed by the user
