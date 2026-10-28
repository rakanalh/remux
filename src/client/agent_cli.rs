//! `remux agent name | ls | status | wait`: name panes, list the ones worth
//! addressing, and wait for an agent's state. Exit codes and deadlines as
//! `remux msg` (see `cli_common`).
//!
//! Every field printed in the table is escaped: a name and a session are
//! labels someone else chose, and a command is a process name, which any
//! program can set to an escape sequence.

use std::time::Duration;

use crate::client::cli_common::{
    ask, escape, forward_deadline, json_line, pane_hint, parse_pane, parse_wait, run_verb,
    unexpected, CliError, REPLY_DEADLINE,
};
use crate::protocol::{
    valid_name, Address, AgentState, ClientMessage, PaneId, PaneStatusEntry, ServerMessage,
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
        /// Another pane, `%N`: only from outside every pane
        #[arg(long, value_parser = parse_pane)]
        pane: Option<PaneId>,
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
                ServerMessage::AgentNamed { pane, name } => {
                    log::debug!("cmd: agent name answered AgentNamed(%{pane})");
                    println!("{}", named_line(pane, name.as_deref()));
                    Ok(())
                }
                other => Err(unexpected(&other)),
            }
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
                                target.host.as_deref(),
                                target.pane_id,
                                target.name.as_deref(),
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
    host: Option<&str>,
    pane: PaneId,
    name: Option<&str>,
    want: AgentState,
    state: Option<AgentState>,
    timed_out: bool,
) -> String {
    let pane = pane_cell(host, pane);
    let named = name
        .map(|n| format!(" ({})", escape(n)))
        .unwrap_or_default();
    if timed_out {
        format!(
            "{pane}{named} is not {} (timed out; now {})",
            wait_word(Some(want)),
            wait_word(state)
        )
    } else {
        format!("{pane}{named} is {}", wait_word(state))
    }
}

/// A pane as an address names it: `%7` here, `mini:%7` on another server.
fn pane_cell(host: Option<&str>, pane: PaneId) -> String {
    match host {
        Some(host) => format!("{}:%{pane}", escape(host)),
        None => format!("%{pane}"),
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

fn named_line(pane: PaneId, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("pane %{pane} is now \"{}\"", escape(name)),
        None => format!("pane %{pane} unnamed"),
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
    let header = [
        "PANE", "SESSION", "TAB", "NAME", "COMMAND", "STATE", "UNREAD",
    ]
    .map(String::from);
    let mut rows = vec![header.to_vec()];
    for p in panes {
        rows.push(vec![
            pane_cell(p.host.as_deref(), p.pane_id),
            escape(&p.session),
            p.tab_index.saturating_add(1).to_string(),
            p.name.as_deref().map_or_else(|| "-".to_string(), escape),
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
    use crate::protocol::is_invisible;

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
            "PANE  SESSION  TAB  NAME   COMMAND  STATE        UNREAD\n\
             %3    main     1    -      -        -            0\n\
             %12   main     1    alpha  claude   needs-input  3\n"
        );
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
    fn a_remote_entry_shows_its_host_in_the_pane_column() {
        let mut remote = entry(7, Some("beta"));
        remote.host = Some("mini".into());
        let table = render_table(&[entry(3, None), remote]);
        assert_eq!(
            table,
            "PANE     SESSION  TAB  NAME  COMMAND  STATE  UNREAD\n\
             %3       main     1    -     -        -      0\n\
             mini:%7  main     1    beta  -        -      0\n"
        );
    }

    #[test]
    fn the_waited_line_names_the_host() {
        assert_eq!(
            waited_line(
                Some("mini"),
                7,
                Some("beta"),
                AgentState::NeedsInput,
                Some(AgentState::NeedsInput),
                false
            ),
            "mini:%7 (beta) is blocked"
        );
        assert_eq!(
            waited_line(
                Some("m\u{1b}"),
                7,
                None,
                AgentState::Idle,
                Some(AgentState::Idle),
                false
            ),
            "m\\x1b:%7 is idle",
            "the host is the far side's word too, so it is escaped"
        );
    }

    #[test]
    fn the_waited_line_says_whether_the_state_came() {
        assert_eq!(
            waited_line(
                None,
                3,
                Some("lead"),
                AgentState::Idle,
                Some(AgentState::Idle),
                false
            ),
            "%3 (lead) is idle"
        );
        assert_eq!(
            waited_line(
                None,
                3,
                None,
                AgentState::Idle,
                Some(AgentState::Working),
                true
            ),
            "%3 is not idle (timed out; now working)"
        );
        assert_eq!(
            waited_line(
                None,
                3,
                Some("a\u{1b}"),
                AgentState::NeedsInput,
                Some(AgentState::NeedsInput),
                false
            ),
            "%3 (a\\x1b) is blocked"
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
    fn the_named_line_escapes_the_name() {
        assert_eq!(named_line(3, Some("alpha")), "pane %3 is now \"alpha\"");
        assert_eq!(named_line(3, None), "pane %3 unnamed");
        assert_eq!(named_line(3, Some("a\u{1b}")), "pane %3 is now \"a\\x1b\"");
    }
}
