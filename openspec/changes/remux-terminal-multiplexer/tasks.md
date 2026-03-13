## 1. Project Scaffolding

- [x] 1.1 Initialize Cargo project with `cargo init`, set up Cargo.toml with all dependencies (crossterm, vte, nix, libc, tokio, interprocess, crossbeam, cassowary, serde, serde_json, toml, clap, signal-hook, daemonize, unicode-width, anyhow, thiserror, dirs, log)
- [x] 1.2 Create module directory structure: src/{server/, client/, config/, protocol.rs, main.rs} with mod declarations
- [x] 1.3 Set up clap CLI argument parsing for subcommands: (default) start/attach, new, attach, ls, kill

## 2. PTY Management

- [x] 2.1 Implement PTY allocation using nix (openpty, set window size, configure terminal attributes)
- [x] 2.2 Implement child process spawning (fork, setsid, dup2 slave fd to stdin/stdout/stderr, exec shell)
- [x] 2.3 Implement async PTY reader as a tokio task that reads master fd and sends output to a channel
- [x] 2.4 Implement PTY writer that forwards input bytes to the master fd
- [x] 2.5 Implement PTY resize (TIOCSWINSZ ioctl + SIGWINCH to child process group)
- [x] 2.6 Implement child process exit detection (SIGCHLD handling or waitpid polling)

## 3. VTE Parsing and Screen Buffer

- [x] 3.1 Implement per-pane Screen struct (cell grid with character + attributes, cursor position, scrollback ring buffer)
- [x] 3.2 Integrate vte::Parser — implement vte::Perform trait to update Screen state on escape sequences
- [x] 3.3 Handle basic VT100 operations: cursor movement, erase line/screen, insert/delete lines, scroll regions
- [x] 3.4 Handle text attributes: bold, italic, underline, foreground/background colors (16, 256, truecolor)
- [x] 3.5 Implement scrollback buffer (ring buffer of lines, configurable max size)

## 4. Layout Engine

- [x] 4.1 Define LayoutNode enum (Split { direction, ratio, first, second } | Stack { panes, active })
- [x] 4.2 Implement split operations (split_vertical, split_horizontal) that replace a leaf with a split node
- [x] 4.3 Implement cassowary constraint setup from the split tree (ratios → constraints, min pane size = 2x2)
- [x] 4.4 Implement dimension computation: walk the tree, assign Rect(x, y, width, height) to each leaf
- [x] 4.5 Implement resize operations (adjust split ratio by increment, recompute constraints)
- [x] 4.6 Implement directional focus navigation (find nearest pane stack in direction based on spatial position)
- [x] 4.7 Implement pane close with tree simplification (remove empty stack, replace split with sibling)
- [x] 4.8 Implement pane stack operations (add pane, cycle next/prev, close pane within stack)

## 5. Session Model

- [x] 5.1 Define data structures: Server { folders, sessions }, Folder { name, session_ids }, Session { name, folder, tabs, active_tab }, Tab { name, layout: LayoutNode }
- [x] 5.2 Implement session CRUD (create with optional folder, rename, delete with cleanup)
- [x] 5.3 Implement folder CRUD (create, rename, delete if empty, list)
- [x] 5.4 Implement tab CRUD (create with default pane, close with pane cleanup, rename, reorder)
- [x] 5.5 Implement move session between folders (or to/from top-level)
- [x] 5.6 Implement unique session name validation

## 6. Client-Server Architecture

- [x] 6.1 Define protocol messages (serde-serializable enums for client→server and server→client messages)
- [x] 6.2 Implement server daemon startup with daemonize (fork, create socket dir, bind Unix socket)
- [x] 6.3 Implement server socket listener (accept connections, spawn tokio task per client)
- [x] 6.4 Implement client connection (check for existing server via socket, connect or start new server)
- [x] 6.5 Implement server event loop (select across: client messages, PTY outputs, timers)
- [x] 6.6 Implement client attach/detach (attach to session, detach leaves session running)
- [x] 6.7 Implement multi-client support (broadcast render updates to all clients attached to same session)

## 7. Terminal Rendering

- [x] 7.1 Implement client terminal setup (crossterm raw mode, alternate screen, enable mouse/keyboard enhancement)
- [x] 7.2 Implement front/back buffer diff engine (compare cell grids, emit only changed cells via crossterm)
- [x] 7.3 Implement frame renderer for zellij-style (draw borders around each pane, stack tab headers in top border)
- [x] 7.4 Implement minimal/tmux-style renderer (no pane borders, single status bar at bottom)
- [x] 7.5 Implement status bar rendering (mode indicator, folder/session path, tab list, active tab highlight)
- [x] 7.6 Implement compositing (walk layout tree, blit each active pane's screen buffer into the back buffer at its Rect)
- [x] 7.7 Implement terminal resize handling (SIGWINCH → recompute layout → resize all PTYs → re-render)

## 8. Modal Input System

- [x] 8.1 Implement mode state machine (Insert/Normal/Visual with transitions)
- [x] 8.2 Implement Insert mode input handling (pass all keys to PTY except mode-switch key)
- [x] 8.3 Implement Normal mode input routing (feed keys into keybinding tree, dispatch commands)
- [x] 8.4 Implement Visual mode (enter scrollback view, vim motions for navigation, text selection)
- [x] 8.5 Implement clipboard integration for yank (system clipboard via OSC 52 or xclip/wl-copy)

## 9. Keybinding Tree and Which-Key

- [x] 9.1 Define keybinding tree data structure (KeyNode enum: Group { label, children } | Command { action })
- [x] 9.2 Implement TOML config parser for keybinding definitions (nested tables → KeyNode tree)
- [x] 9.3 Implement default keybinding tree (t=Tab, p=Pane, s=Session, f=Folder, b=Buffer, r=Resize groups)
- [x] 9.4 Implement config merge (user config overrides/extends defaults)
- [x] 9.5 Implement keybinding tree traversal (track current node, advance on keypress, dispatch on leaf)
- [x] 9.6 Implement which-key popup renderer (floating box, two-column layout, key + label pairs)
- [x] 9.7 Implement configurable timeout (show popup after delay, cancel on Escape)
- [x] 9.8 Implement command dispatcher (parse action strings like "tab:new", "pane:split_vertical" → execute)

## 10. Scrollback and Editor Integration

- [x] 10.1 Implement Visual mode scrollback navigation (j/k line, Ctrl-d/Ctrl-u half-page, gg/G top/bottom)
- [x] 10.2 Implement Visual mode text selection (v character-wise, V line-wise, highlight selected region)
- [x] 10.3 Implement scrollback search ('/' prompt, highlight matches, n/N navigation)
- [x] 10.4 Implement "edit in $EDITOR" (dump scrollback + visible to temp file, spawn $EDITOR readonly, cleanup)

## 11. Session Persistence

- [x] 11.1 Define serializable state structure (folders, sessions, tabs, layouts, cwds → JSON)
- [x] 11.2 Implement state serialization (walk server state, extract layout tree + cwd per pane)
- [x] 11.3 Implement atomic state save (write to temp file, rename to state.json)
- [x] 11.4 Implement auto-save timer (configurable interval, default 30s, tokio::time::interval)
- [x] 11.5 Implement state deserialization and session resurrection on server start
- [x] 11.6 Implement manual save command ("session:save")

## 12. Configuration

- [x] 12.1 Define Config struct with all configurable fields (serde Deserialize, defaults via Default trait)
- [x] 12.2 Implement TOML config loading from ~/.config/remux/config.toml with fallback to defaults
- [x] 12.3 Implement config file watching (notify crate or polling) with hot reload
- [x] 12.4 Wire config values into all subsystems (keybindings, appearance, behavior, timeouts)

## 13. Integration and Polish

- [x] 13.1 Wire all subsystems together in the server event loop (PTY → screen → render → client)
- [x] 13.2 Wire CLI commands to server operations (new/attach/ls/kill send messages to server)
- [x] 13.3 Implement graceful shutdown (kill all child processes, save state, remove socket)
- [ ] 13.4 Test with common terminal programs (vim, htop, less, man, top) and fix rendering issues
- [ ] 13.5 Test session resurrection (kill server, restart, verify layout + cwds restored)
- [ ] 13.6 Test multi-client attach (two terminals attached to same session)
