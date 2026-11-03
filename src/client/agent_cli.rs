//! `remux agent name | ls | status | wait`: name panes, list the ones worth
//! addressing, and wait for an agent's state. Exit codes and deadlines as
//! `remux msg` (see `cli_common`).
//!
//! Every field printed in the table is escaped: a name and a session are
//! labels someone else chose, and a command is a process name, which any
//! program can set to an escape sequence.

use std::time::Duration;

use crate::client::cli_common::{
    ask, escape, forward_deadline, json_line, pane_hint, parse_pane_arg, parse_wait,
    resolve_pane_arg, run_verb, unexpected, CliError, PaneArg, REPLY_DEADLINE,
};
use crate::client::hook_cli::{run_session, SessionInput};
use crate::protocol::{
    valid_name, Address, AgentState, ClientMessage, PaneStatusEntry, ServerMessage,
};

#[derive(clap::Subcommand, Debug)]
pub enum AgentCmd {
    /// Give this pane the name other panes send to, or clear it.
    #[command(group(
        clap::ArgGroup::new("which").required(true).args(["name", "clear"])
    ))]
    Name {
        /// `[a-z][a-z0-9_-]{0,31}`
        name: Option<String>,
        /// Remove the pane's name
        #[arg(long)]
        clear: bool,
        /// Another pane, by its name: only from outside every pane
        #[arg(long, value_name = "NAME", value_parser = parse_pane_arg)]
        pane: Option<PaneArg>,
    },
    /// List every agent, named pane and pane with mail.
    Ls {
        /// Print the list as JSON. The table is for people; use --json in
        /// scripts, because a name or command can contain spaces that read
        /// as columns. Its `tab_index` counts from 0, as the server sends
        /// it; the table's TAB counts from 1, as the status bar does
        #[arg(long)]
        json: bool,
    },
    /// Show one pane, or every pane worth listing.
    Status {
        /// `%N`, a name, or `host:` either
        target: Option<String>,
        /// Print the list as JSON. The table is for people; use --json in
        /// scripts, because a name or command can contain spaces that read
        /// as columns. Its `tab_index` counts from 0, as the server sends
        /// it; the table's TAB counts from 1, as the status bar does
        #[arg(long)]
        json: bool,
    },
    /// Wait until a pane's agent is in a state. Exits 0 when it is, and 0 when
    /// the time runs out, saying so and naming the state it is in. The state
    /// is the one a typed message is checked against, so a numbered selector
    /// that no prompt pattern names counts as blocked here, while `agent
    /// status` can still show that pane idle.
    Wait {
        /// `%N` or a name
        target: String,
        /// The state to wait for
        #[arg(long, value_enum)]
        state: StateArg,
        /// How long to wait: `30`, `30s`, `2m`, at most 1h; 10m by default
        #[arg(long, value_parser = parse_wait, value_name = "DUR")]
        timeout: Option<Duration>,
        /// Print the answer as JSON
        #[arg(long)]
        json: bool,
    },
    /// Report the session this pane's agent is running, so that a restore
    /// resumes it. Run by an agent's hook or plugin, never by hand: a report
    /// typed at the prompt is refused. Always exits 0 and prints nothing on
    /// stdout; outside a pane it does nothing at all.
    #[command(group(
        clap::ArgGroup::new("reference").required(true).args(["hook", "id", "path"])
    ))]
    Session {
        /// claude, codex, opencode, pi or omp
        agent: String,
        /// Read the agent's hook payload from standard input
        #[arg(long)]
        hook: bool,
        /// The session's id
        #[arg(long)]
        id: Option<String>,
        /// The session file's absolute path
        #[arg(long)]
        path: Option<String>,
        /// How the session started: startup, resume, clear, compact, fork,
        /// new or select. Read from the payload with --hook
        #[arg(long)]
        source: Option<String>,
        /// Another pane, by its name: only from outside every pane
        #[arg(long, value_name = "NAME", value_parser = parse_pane_arg)]
        pane: Option<PaneArg>,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum StateArg {
    Idle,
    Working,
    /// Waiting on its user: a question or a permission prompt
    #[value(alias = "needs-input")]
    Blocked,
}

impl From<StateArg> for AgentState {
    fn from(arg: StateArg) -> Self {
        match arg {
            StateArg::Idle => AgentState::Idle,
            StateArg::Working => AgentState::Working,
            StateArg::Blocked => AgentState::NeedsInput,
        }
    }
}

/// Run one `remux agent` verb, and the exit code for it.
pub async fn run_agent(cmd: AgentCmd) -> u8 {
    // An agent's hook runs this, and a hook must never fail its agent, so no
    // outcome maps to a non-zero exit.
    if matches!(cmd, AgentCmd::Session { .. }) {
        let _ = run(cmd).await;
        return 0;
    }
    let json = matches!(
        cmd,
        AgentCmd::Ls { json: true }
            | AgentCmd::Status { json: true, .. }
            | AgentCmd::Wait { json: true, .. }
    );
    run_verb(run(cmd), json).await
}

async fn run(cmd: AgentCmd) -> Result<(), CliError> {
    match cmd {
        AgentCmd::Name { name, clear, pane } => {
            let name = if clear { None } else { name };
            if let Some(bad) = name.as_deref().filter(|n| !valid_name(n)) {
                return Err(CliError::usage(
                    "bad_name",
                    format!(
                        "\"{}\" is not a name: {}",
                        escape(bad),
                        crate::protocol::bad_name_reason(bad)
                    ),
                ));
            }
            log::debug!(
                "cmd: agent name pane={pane:?} {}",
                if name.is_some() { "set" } else { "clear" }
            );
            let pane = resolve_pane_arg(pane).await?;
            let answer = ask(
                ClientMessage::AgentName {
                    name,
                    pane,
                    pane_hint: pane_hint(),
                },
                REPLY_DEADLINE,
            )
            .await?;
            match answer {
                ServerMessage::AgentNamed {
                    pane,
                    name,
                    session,
                } => {
                    log::debug!("cmd: agent name answered AgentNamed(%{pane})");
                    println!("{}", named_line(name.as_deref(), session.as_deref()));
                    Ok(())
                }
                other => Err(unexpected(&other)),
            }
        }
        AgentCmd::Session {
            agent,
            hook: _,
            id,
            path,
            source,
            pane,
        } => {
            let input = match (id, path) {
                (Some(id), _) => SessionInput::Id(id),
                (None, Some(path)) => SessionInput::Path(path),
                (None, None) => SessionInput::Hook,
            };
            // Like the rest of `agent session`, a name that resolves to no
            // pane is reported nowhere: it prints nothing and exits 0.
            match resolve_pane_arg(pane).await {
                Ok(pane) => run_session(agent, input, source, pane).await,
                Err(e) => log::debug!("cmd: agent session --pane not resolved: {}", e.stderr()),
            }
            Ok(())
        }
        AgentCmd::Ls { json } => status(None, json).await,
        AgentCmd::Status { target, json } => {
            let target = target
                .map(|t| t.parse::<Address>())
                .transpose()
                .map_err(|why| CliError::usage("bad_address", why))?;
            status(target, json).await
        }
        AgentCmd::Wait {
            target,
            state,
            timeout,
            json,
        } => {
            let target: Address = target
                .parse()
                .map_err(|why: String| CliError::usage("bad_address", why))?;
            let spelled = target.to_string();
            let want: AgentState = state.into();
            let timeout = wait_for(timeout);
            log::debug!(
                "cmd: agent wait target={:?} state={want:?} timeout={}",
                target.to_string(),
                timeout.as_millis()
            );
            let deadline = timeout + REPLY_DEADLINE + forward_deadline(&target);
            let answer = ask(
                ClientMessage::AgentWait {
                    target,
                    state: want,
                    timeout_ms: Some(timeout.as_millis() as u64),
                    req: None,
                },
                deadline,
            )
            .await?;
            match answer {
                ServerMessage::AgentWaited {
                    target,
                    state,
                    timed_out,
                    ..
                } => {
                    log::debug!("cmd: agent wait answered AgentWaited(timed_out={timed_out})");
                    if json {
                        println!(
                            "{}",
                            json_line(&serde_json::json!({
                                "target": target, "state": state, "timed_out": timed_out
                            }))
                        );
                    } else {
                        println!(
                            "{}",
                            waited_line(
                                &target_label(
                                    target.host.as_deref(),
                                    target.name.as_deref(),
                                    &spelled
                                ),
                                want,
                                state,
                                timed_out
                            )
                        );
                    }
                    Ok(())
                }
                other => Err(unexpected(&other)),
            }
        }
    }
}

/// The time `agent wait` asks for: `timeout`, or the server's default, at
/// most an hour, as the server caps it.
fn wait_for(timeout: Option<Duration>) -> Duration {
    use crate::server::daemon::{DEFAULT_SUBMIT_TIMEOUT_MS, MAX_WAIT_MS};
    timeout
        .unwrap_or(Duration::from_millis(DEFAULT_SUBMIT_TIMEOUT_MS))
        .min(Duration::from_millis(MAX_WAIT_MS))
}

/// A state as `agent wait --state` names it, so the answer uses the flag's
/// words.
fn wait_word(state: Option<AgentState>) -> &'static str {
    match state {
        Some(AgentState::NeedsInput) => "blocked",
        other => state_word(other),
    }
}

fn waited_line(
    target: &str,
    want: AgentState,
    state: Option<AgentState>,
    timed_out: bool,
) -> String {
    if timed_out {
        format!(
            "{target} is not {} (timed out; now {})",
            wait_word(Some(want)),
            wait_word(state)
        )
    } else {
        format!("{target} is {}", wait_word(state))
    }
}

/// An agent as an address names it: `alpha` here, `mini:alpha` on another
/// server, `-` for one with no name (`mini:-`). Never a pane id.
pub(crate) fn name_cell(host: Option<&str>, name: Option<&str>) -> String {
    let name = name.map_or_else(|| "-".to_string(), escape);
    match host {
        Some(host) => format!("{}:{name}", escape(host)),
        None => name,
    }
}

/// How an answer names the agent a command was about: by its name, or, when
/// it has none, as the caller spelled it, which is the caller's own word and
/// so may be a `%N` it typed.
pub(crate) fn target_label(host: Option<&str>, name: Option<&str>, spelled: &str) -> String {
    match name {
        Some(_) => name_cell(host, name),
        None => escape(spelled),
    }
}

async fn status(target: Option<Address>, json: bool) -> Result<(), CliError> {
    log::debug!(
        "cmd: agent status target={:?}",
        target.as_ref().map(ToString::to_string)
    );
    let deadline = REPLY_DEADLINE + target.as_ref().map_or(Duration::ZERO, forward_deadline);
    match ask(ClientMessage::AgentStatus { target, req: None }, deadline).await? {
        ServerMessage::AgentStatusList { panes, .. } => {
            log::debug!(
                "cmd: agent status answered AgentStatusList({} pane(s))",
                panes.len()
            );
            if json {
                println!("{}", json_line(&serde_json::json!({ "panes": panes })));
            } else {
                print!("{}", render_table(&panes));
            }
            Ok(())
        }
        other => Err(unexpected(&other)),
    }
}

fn named_line(name: Option<&str>, session: Option<&str>) -> String {
    let session = session
        .map(|s| format!(" ({})", escape(s)))
        .unwrap_or_default();
    match name {
        Some(name) => format!("named {}{session}", escape(name)),
        None => format!("name cleared{session}"),
    }
}

fn state_word(state: Option<AgentState>) -> &'static str {
    match state {
        Some(AgentState::NeedsInput) => "needs-input",
        Some(AgentState::Working) => "working",
        Some(AgentState::Idle) => "idle",
        None => "-",
    }
}

/// A column-aligned table, header first. Fields are escaped before they are
/// measured, so the padding matches what the terminal shows.
fn render_table(panes: &[PaneStatusEntry]) -> String {
    let header = ["NAME", "SESSION", "TAB", "COMMAND", "STATE", "UNREAD"].map(String::from);
    let mut rows = vec![header.to_vec()];
    for p in panes {
        rows.push(vec![
            name_cell(p.host.as_deref(), p.name.as_deref()),
            escape(&p.session),
            p.tab_index.saturating_add(1).to_string(),
            p.command.as_deref().map_or_else(|| "-".to_string(), escape),
            state_word(p.state).to_string(),
            p.unread.to_string(),
        ]);
    }
    let widths: Vec<usize> = (0..header.len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, &w)| format!("{cell:<w$}"))
                .collect();
            format!("{}\n", cells.join("  ").trim_end())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{is_invisible, PaneId};

    fn entry(pane_id: PaneId, name: Option<&str>) -> PaneStatusEntry {
        PaneStatusEntry {
            pane_id,
            session: "main".into(),
            tab_index: 0,
            command: None,
            state: None,
            name: name.map(String::from),
            unread: 0,
            host: None,
        }
    }

    #[test]
    fn the_table_is_aligned_under_its_header() {
        let mut claude = entry(12, Some("alpha"));
        claude.command = Some("claude".into());
        claude.state = Some(AgentState::NeedsInput);
        claude.unread = 3;
        let table = render_table(&[entry(3, None), claude]);
        assert_eq!(
            table,
            "NAME   SESSION  TAB  COMMAND  STATE        UNREAD\n\
             -      main     1    -        -            0\n\
             alpha  main     1    claude   needs-input  3\n"
        );
        assert!(!table.contains('%'), "no pane id: {table}");
    }

    #[test]
    fn a_hostile_entry_shows_every_field_escaped() {
        let hostile = PaneStatusEntry {
            pane_id: 4,
            session: "s\u{1b}[2J\nx".into(),
            tab_index: 1,
            command: Some("c\u{9b}31m\u{202e}".into()),
            state: Some(AgentState::Working),
            name: Some("n\u{1b}]52;c;aGk=\u{7}\u{e0041}".into()),
            unread: 1,
            host: None,
        };
        let table = render_table(&[hostile]);
        assert!(
            !table
                .chars()
                .any(|c| (c.is_control() && c != '\n') || is_invisible(c)),
            "{table:?}"
        );
        assert_eq!(table.lines().count(), 2, "{table}");
        assert!(table.contains("s\\x1b[2J\\x0ax"), "{table}");
        assert!(table.contains("c\\u{9b}31m\\u{202e}"), "{table}");
        assert!(table.contains("n\\x1b]52;c;aGk=\\x07\\u{e0041}"), "{table}");
    }

    #[test]
    fn a_hostile_tab_index_is_shown_not_overflowed() {
        let mut far = entry(7, None);
        far.tab_index = usize::MAX;
        let table = render_table(&[far]);
        assert!(table.contains(&usize::MAX.to_string()), "{table}");
    }

    #[test]
    fn a_remote_entry_shows_its_host_in_the_name_column() {
        let mut remote = entry(7, Some("beta"));
        remote.host = Some("mini".into());
        let mut nameless = entry(8, None);
        nameless.host = Some("mini".into());
        let table = render_table(&[entry(3, None), remote, nameless]);
        assert_eq!(
            table,
            "NAME       SESSION  TAB  COMMAND  STATE  UNREAD\n\
             -          main     1    -        -      0\n\
             mini:beta  main     1    -        -      0\n\
             mini:-     main     1    -        -      0\n"
        );
    }

    #[test]
    fn the_waited_line_names_the_host() {
        let beta = target_label(Some("mini"), Some("beta"), "mini:%7");
        assert_eq!(
            waited_line(
                &beta,
                AgentState::NeedsInput,
                Some(AgentState::NeedsInput),
                false
            ),
            "mini:beta is blocked"
        );
        let far = target_label(Some("m\u{1b}"), Some("x"), "m:x");
        assert_eq!(
            waited_line(&far, AgentState::Idle, Some(AgentState::Idle), false),
            "m\\x1b:x is idle",
            "the host is the far side's word too, so it is escaped"
        );
    }

    #[test]
    fn a_target_with_no_name_is_shown_as_the_caller_spelled_it() {
        assert_eq!(target_label(None, None, "%3"), "%3");
        assert_eq!(target_label(Some("mini"), None, "mini:%3"), "mini:%3");
        assert_eq!(target_label(None, Some("lead"), "%3"), "lead");
    }

    #[test]
    fn the_waited_line_says_whether_the_state_came() {
        let lead = target_label(None, Some("lead"), "lead");
        assert_eq!(
            waited_line(&lead, AgentState::Idle, Some(AgentState::Idle), false),
            "lead is idle"
        );
        assert_eq!(
            waited_line(&lead, AgentState::Idle, Some(AgentState::Working), true),
            "lead is not idle (timed out; now working)"
        );
        let odd = target_label(None, Some("a\u{1b}"), "x");
        assert_eq!(
            waited_line(
                &odd,
                AgentState::NeedsInput,
                Some(AgentState::NeedsInput),
                false
            ),
            "a\\x1b is blocked"
        );
    }

    #[test]
    fn the_wait_asks_for_the_default_and_never_past_the_cap() {
        use crate::server::daemon::{DEFAULT_SUBMIT_TIMEOUT_MS, MAX_WAIT_MS};
        assert_eq!(
            wait_for(None),
            Duration::from_millis(DEFAULT_SUBMIT_TIMEOUT_MS)
        );
        assert_eq!(
            wait_for(Some(Duration::from_secs(5))),
            Duration::from_secs(5)
        );
        assert_eq!(
            wait_for(Some(Duration::from_secs(u32::MAX as u64))),
            Duration::from_millis(MAX_WAIT_MS)
        );
    }

    #[derive(clap::Parser, Debug)]
    struct Parsed {
        #[command(subcommand)]
        cmd: AgentCmd,
    }

    #[test]
    fn a_wait_needs_a_state_and_takes_blocked_or_needs_input() {
        use clap::Parser;
        let parse = |args: &[&str]| Parsed::try_parse_from(["agent"].iter().chain(args));
        assert!(parse(&["wait", "%1"]).is_err(), "no state");
        assert!(parse(&["wait", "%1", "--state", "done"]).is_err());
        for word in ["idle", "working", "blocked", "needs-input"] {
            assert!(parse(&["wait", "%1", "--state", word]).is_ok(), "{word}");
        }
        assert!(parse(&["wait", "%1", "--state", "idle", "--timeout", "abc"]).is_err());
    }

    #[test]
    fn the_named_line_names_the_session_and_escapes_both() {
        assert_eq!(
            named_line(Some("alpha"), Some("Remux")),
            "named alpha (Remux)"
        );
        assert_eq!(named_line(None, Some("Remux")), "name cleared (Remux)");
        assert_eq!(
            named_line(Some("a\u{1b}"), Some("s\u{1b}")),
            "named a\\x1b (s\\x1b)"
        );
        assert_eq!(
            named_line(Some("alpha"), None),
            "named alpha",
            "an older server"
        );
        assert_eq!(named_line(None, None), "name cleared");
    }
}
