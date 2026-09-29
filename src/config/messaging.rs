//! `[messaging]` configuration: how long mail lives and how much of it a
//! mailbox and a sender may hold, and how a message is typed into each kind
//! of agent (`[messaging.submit.<command>]`).
//!
//! The server reads this at startup, so an edit needs `remux restart`.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::server::messaging::Limits;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct MessagingConfig {
    /// A float so that a harness can expire mail in about a second.
    pub ttl_hours: f64,
    pub mailbox_max_messages: usize,
    pub mailbox_max_bytes: usize,
    pub max_body_bytes: usize,
    pub rate_per_10s: u32,
    /// `false` keeps mailboxes and names in memory only, so they do not
    /// survive `remux restart`.
    pub persist: bool,
    /// How to type into each agent, by its `[agents] commands` spelling.
    /// Merged field by field over [`shipped_rows`].
    pub submit: BTreeMap<String, SubmitRow>,
}

/// Whether a typed message is wrapped in bracketed paste.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Bracketed {
    /// As the pane's program asked (mode 2004), read when the text is typed.
    Auto,
    On,
    Off,
}

/// What a submit does while the agent is working.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WhileWorking {
    /// Type it now: the agent takes typed input as its next prompt.
    Queue,
    /// Leave it in the mailbox, for the agent's hook to deliver.
    Mailbox,
    /// Refuse a `Submit`; an `Auto` leaves it in the mailbox.
    Refuse,
}

/// One `[messaging.submit.<command>]` row. Every field is optional, so a
/// user's row changes only the fields it names and keeps the shipped value
/// of the rest.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SubmitRow {
    pub bracketed: Option<Bracketed>,
    /// The bytes that submit the typed text, `"\r"` unless set.
    pub enter: Option<String>,
    pub enter_delay_ms: Option<u64>,
    /// Report focus (`ESC[I`) before the text, for an agent that ignores
    /// input it believes is not for it.
    pub focus_in: Option<bool>,
    pub while_working: Option<WhileWorking>,
    /// Open the typed text with a line naming the message and its sender.
    pub envelope: Option<bool>,
}

impl SubmitRow {
    /// `self`, with each field `over` sets replaced by `over`'s.
    pub fn merged(&self, over: &SubmitRow) -> SubmitRow {
        SubmitRow {
            bracketed: over.bracketed.or(self.bracketed),
            enter: over.enter.clone().or_else(|| self.enter.clone()),
            enter_delay_ms: over.enter_delay_ms.or(self.enter_delay_ms),
            focus_in: over.focus_in.or(self.focus_in),
            while_working: over.while_working.or(self.while_working),
            envelope: over.envelope.or(self.envelope),
        }
    }
}

/// The rows that ship: Claude Code takes typed input while it works as its
/// next prompt, so `queue`; Copilot's CLI ignores input until it has focus,
/// so `focus_in`; Codex's behaviour while working is inferred rather than
/// verified, so it waits in the `mailbox`. Every other command gets the
/// generic default.
pub fn shipped_rows() -> BTreeMap<String, SubmitRow> {
    BTreeMap::from([
        (
            "claude".to_string(),
            SubmitRow {
                while_working: Some(WhileWorking::Queue),
                ..SubmitRow::default()
            },
        ),
        (
            "copilot".to_string(),
            SubmitRow {
                focus_in: Some(true),
                ..SubmitRow::default()
            },
        ),
        (
            "codex".to_string(),
            SubmitRow {
                while_working: Some(WhileWorking::Mailbox),
                ..SubmitRow::default()
            },
        ),
    ])
}

/// The shortest `enter_delay_ms`: with no delay the paste and its Enter can
/// arrive in one read, which an agent can take as part of the paste.
pub const MIN_ENTER_DELAY_MS: u64 = 50;

/// The longest `enter_delay_ms`. The reply to a send waits for the Enter, so
/// this must stay well inside the CLI's reply deadline (10 s,
/// `cli_common::REPLY_DEADLINE`): a CLI that gives up first leaves a message
/// that is typed after all, and a retry then delivers it twice.
pub const MAX_ENTER_DELAY_MS: u64 = 5000;

/// The most bytes an `enter` may be.
pub const MAX_ENTER_BYTES: usize = 4;

/// Whether `enter` can submit a typed message: one to four control bytes
/// (`"\r"`, `"\n"`, `"\r\n"`), so it can neither type text of its own nor
/// end a bracketed paste early.
pub fn valid_enter(enter: &str) -> bool {
    (1..=MAX_ENTER_BYTES).contains(&enter.len()) && enter.bytes().all(|b| b < 0x20 || b == 0x7f)
}

impl Default for MessagingConfig {
    fn default() -> Self {
        Self {
            ttl_hours: 24.0,
            mailbox_max_messages: 256,
            mailbox_max_bytes: 4 * 1024 * 1024,
            max_body_bytes: 64 * 1024,
            rate_per_10s: 20,
            persist: true,
            submit: BTreeMap::new(),
        }
    }
}

impl MessagingConfig {
    /// A negative or NaN `ttl_hours` saturates to 0, which expires mail at
    /// once rather than keeping it for ever.
    pub fn ttl_ms(&self) -> u64 {
        (self.ttl_hours * 3_600_000.0) as u64
    }

    /// The bounds the store enforces, with every value [`Self::problems`]
    /// reports replaced. A zero cap would refuse every send, which reads as
    /// broken messaging rather than a setting, so it falls back to the default.
    pub fn limits(&self) -> Limits {
        let default = Self::default();
        let or_default = |value: usize, fallback: usize| if value == 0 { fallback } else { value };
        let max_bytes = or_default(self.mailbox_max_bytes, default.mailbox_max_bytes);
        Limits {
            max_messages: or_default(self.mailbox_max_messages, default.mailbox_max_messages),
            max_bytes,
            max_body: self.max_body_bytes.min(max_bytes),
            rate_per_10s: if self.rate_per_10s == 0 {
                default.rate_per_10s
            } else {
                self.rate_per_10s
            },
            ttl_ms: self.ttl_ms(),
        }
    }

    /// One line per value that [`Self::limits`] does not use as written, for
    /// the config loader to warn with.
    pub fn problems(&self) -> Vec<String> {
        let default = Self::default();
        let mut out = Vec::new();
        for (key, value, fallback) in [
            (
                "mailbox_max_messages",
                self.mailbox_max_messages,
                default.mailbox_max_messages,
            ),
            (
                "mailbox_max_bytes",
                self.mailbox_max_bytes,
                default.mailbox_max_bytes,
            ),
            (
                "rate_per_10s",
                self.rate_per_10s as usize,
                default.rate_per_10s as usize,
            ),
        ] {
            if value == 0 {
                out.push(format!(
                    "[messaging] {key} = 0 would refuse every send; using {fallback}"
                ));
            }
        }
        let max_bytes = self.limits().max_bytes;
        if self.max_body_bytes > max_bytes {
            out.push(format!(
                "[messaging] max_body_bytes = {} is larger than a whole mailbox; using {max_bytes}",
                self.max_body_bytes
            ));
        }
        for (command, row) in &self.submit {
            if let Some(enter) = row.enter.as_deref().filter(|e| !valid_enter(e)) {
                out.push(format!(
                    "[messaging.submit.{command}] enter = {enter:?} is not 1 to {MAX_ENTER_BYTES} control bytes; using \"\\r\""
                ));
            }
            if let Some(ms) = row.enter_delay_ms {
                if ms > MAX_ENTER_DELAY_MS {
                    out.push(format!(
                        "[messaging.submit.{command}] enter_delay_ms = {ms} is capped at {MAX_ENTER_DELAY_MS}"
                    ));
                } else if ms < MIN_ENTER_DELAY_MS {
                    out.push(format!(
                        "[messaging.submit.{command}] enter_delay_ms = {ms} is raised to {MIN_ENTER_DELAY_MS}"
                    ));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn parse(s: &str) -> MessagingConfig {
        toml::from_str::<Config>(s).unwrap().messaging
    }

    #[test]
    fn messaging_defaults_are_the_spec_values() {
        let m = parse("");
        assert_eq!(m, MessagingConfig::default());
        assert_eq!(m.ttl_hours, 24.0);
        assert_eq!(m.mailbox_max_messages, 256);
        assert_eq!(m.mailbox_max_bytes, 4 * 1024 * 1024);
        assert_eq!(m.max_body_bytes, 64 * 1024);
        assert_eq!(m.rate_per_10s, 20);
        assert!(m.persist);
        assert_eq!(
            m.limits(),
            Limits {
                max_messages: 256,
                max_bytes: 4 * 1024 * 1024,
                max_body: 64 * 1024,
                rate_per_10s: 20,
                ttl_ms: 24 * 3_600_000,
            }
        );
    }

    #[test]
    fn ttl_hours_accepts_a_fraction() {
        assert_eq!(parse("[messaging]\nttl_hours = 0.0005").ttl_ms(), 1_800);
        assert_eq!(parse("[messaging]\nttl_hours = 2").ttl_ms(), 7_200_000);
    }

    /// The commented block in `config.sample.toml`, uncommented, is the
    /// defaults, so the sample cannot drift from the code.
    #[test]
    fn the_sample_config_block_is_the_defaults() {
        let sample = include_str!("../../config.sample.toml");
        let block: String = sample
            .lines()
            .skip_while(|l| *l != "# [messaging]")
            .take_while(|l| l.starts_with("# "))
            .map(|l| format!("{}\n", &l[2..]))
            .collect();
        assert!(block.starts_with("[messaging]\n"), "no [messaging] block");
        assert_eq!(block.lines().count(), 7, "the block is not all six keys");
        assert_eq!(parse(&block), MessagingConfig::default());
    }

    #[test]
    fn the_sample_submit_rows_are_the_shipped_rows() {
        let sample = include_str!("../../config.sample.toml");
        let block: String = sample
            .lines()
            .skip_while(|l| *l != "# [messaging.submit.claude]")
            .take_while(|l| l.starts_with('#'))
            .map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or("")))
            .collect();
        assert!(!block.is_empty(), "no [messaging.submit.claude] block");
        assert_eq!(parse(&block).submit, shipped_rows());
    }

    #[test]
    fn a_submit_row_parses_every_field() {
        let m = parse(
            "[messaging.submit.aider]\nbracketed = \"off\"\nenter = \"\\n\"\nenter_delay_ms = 50\nfocus_in = true\nwhile_working = \"refuse\"\nenvelope = false",
        );
        assert_eq!(
            m.submit["aider"],
            SubmitRow {
                bracketed: Some(Bracketed::Off),
                enter: Some("\n".into()),
                enter_delay_ms: Some(50),
                focus_in: Some(true),
                while_working: Some(WhileWorking::Refuse),
                envelope: Some(false),
            }
        );
        assert!(m.problems().is_empty(), "{:?}", m.problems());
    }

    #[test]
    fn a_bad_enter_and_a_delay_out_of_range_are_reported() {
        let m = parse(
            "[messaging.submit.a]\nenter = \"\"\n[messaging.submit.b]\nenter = \"\\u001b[201~\"\n[messaging.submit.c]\nenter_delay_ms = 9000\n[messaging.submit.d]\nenter = \"ok\"\n[messaging.submit.e]\nenter = \"\\r\\r\\r\\r\\r\"\nenter_delay_ms = 0",
        );
        let problems = m.problems();
        assert_eq!(problems.len(), 6, "{problems:?}");
        assert!(problems[0].starts_with("[messaging.submit.a] enter"));
        assert!(problems[1].starts_with("[messaging.submit.b] enter"));
        assert!(problems[2].contains("is capped at 5000"));
        assert!(problems[3].starts_with("[messaging.submit.d] enter"));
        assert!(problems[4].starts_with("[messaging.submit.e] enter "));
        assert!(problems[5].contains("is raised to 50"));
    }

    #[test]
    fn a_row_for_a_command_that_is_no_agent_is_reported() {
        let cfg: Config = toml::from_str(
            "[agents]\ncommands = [\"claude\"]\n[messaging.submit.claude]\nfocus_in = true\n[messaging.submit.clade]\nfocus_in = true",
        )
        .unwrap();
        assert_eq!(
            cfg.submit_rows_without_an_agent(),
            ["[messaging.submit.clade] names no command in [agents] commands, so it is never used"]
        );
    }

    #[test]
    fn an_enter_is_one_to_four_control_bytes() {
        for ok in ["\r", "\n", "\r\n", "\u{1b}\r", "\r\r\r\r"] {
            assert!(valid_enter(ok), "{ok:?}");
        }
        for bad in ["", "x", "\ry", "\u{1b}[201~", "\r\r\r\r\r"] {
            assert!(!valid_enter(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_zero_cap_falls_back_to_the_default_and_is_reported() {
        let m =
            parse("[messaging]\nmailbox_max_messages = 0\nmailbox_max_bytes = 0\nrate_per_10s = 0");
        assert_eq!(m.limits(), MessagingConfig::default().limits());
        let problems = m.problems();
        assert_eq!(problems.len(), 3, "{problems:?}");
        for key in ["mailbox_max_messages", "mailbox_max_bytes", "rate_per_10s"] {
            assert!(
                problems.iter().any(|p| p.contains(key)),
                "{key} not reported"
            );
        }
        assert!(MessagingConfig::default().problems().is_empty());
    }

    #[test]
    fn the_body_cap_is_clamped_to_the_mailbox_cap_and_reported() {
        let m = parse("[messaging]\nmailbox_max_bytes = 1000\nmax_body_bytes = 5000");
        assert_eq!(m.limits().max_body, 1000);
        assert_eq!(m.problems().len(), 1);
        assert!(m.problems()[0].contains("max_body_bytes"));

        let fits = parse("[messaging]\nmailbox_max_bytes = 1000\nmax_body_bytes = 1000");
        assert_eq!(fits.limits().max_body, 1000);
        assert!(fits.problems().is_empty());
    }

    #[test]
    fn a_partial_table_keeps_the_other_defaults() {
        let m = parse("[messaging]\npersist = false\nrate_per_10s = 3");
        assert!(!m.persist);
        assert_eq!(m.rate_per_10s, 3);
        assert_eq!(m.mailbox_max_messages, 256);
    }
}
