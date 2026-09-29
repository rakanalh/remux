//! `remux agent name | ls | status`: name panes and list the ones worth
//! addressing. Exit codes and deadlines as `remux msg` (see `cli_common`).
//!
//! Every field printed in the table is escaped: a name and a session are
//! labels someone else chose, and a command is a process name, which any
//! program can set to an escape sequence.

use crate::client::cli_common::{
    ask, escape, json_line, pane_hint, parse_pane, run_verb, unexpected, CliError, REPLY_DEADLINE,
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
}

/// Run one `remux agent` verb, and the exit code for it.
pub async fn run_agent(cmd: AgentCmd) -> u8 {
    let json = matches!(
        cmd,
        AgentCmd::Ls { json: true } | AgentCmd::Status { json: true, .. }
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
                        "\"{}\" is not a name: use [a-z][a-z0-9_-]{{0,31}}",
                        escape(bad)
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
    }
}

async fn status(target: Option<Address>, json: bool) -> Result<(), CliError> {
    log::debug!(
        "cmd: agent status target={:?}",
        target.as_ref().map(ToString::to_string)
    );
    match ask(ClientMessage::AgentStatus { target }, REPLY_DEADLINE).await? {
        ServerMessage::AgentStatusList { panes } => {
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
            format!("%{}", p.pane_id),
            escape(&p.session),
            (p.tab_index + 1).to_string(),
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
    fn the_named_line_escapes_the_name() {
        assert_eq!(named_line(3, Some("alpha")), "pane %3 is now \"alpha\"");
        assert_eq!(named_line(3, None), "pane %3 unnamed");
        assert_eq!(named_line(3, Some("a\u{1b}")), "pane %3 is now \"a\\x1b\"");
    }
}
