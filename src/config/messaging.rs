//! `[messaging]` configuration: how long mail lives and how much of it a
//! mailbox and a sender may hold.
//!
//! The server reads this at startup, so an edit needs `remux restart`.

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
    /// Type an envelope line naming the message and its sender before a
    /// message typed into an agent's pane.
    pub envelope: bool,
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
            envelope: true,
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
        assert_eq!(block.lines().count(), 8, "the block is not all seven keys");
        assert_eq!(parse(&block), MessagingConfig::default());
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
