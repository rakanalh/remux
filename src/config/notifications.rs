//! `[notifications]` configuration: desktop notifications for the agents in
//! every server this client is connected to.
//!
//! Read by the CLIENT, which is what sends them, so for a remote agent it is
//! the local machine's config that applies and the notification appears on the
//! machine the client runs on. Applied live on a config reload.

use serde::Deserialize;

fn default_true() -> bool {
    true
}

/// `[notifications]`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct NotificationsConfig {
    /// The master switch. Off, the client sends nothing and does not keep the
    /// agent list subscribed for notifications.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// An agent started waiting on an approval, a question or a permission
    /// prompt.
    #[serde(default = "default_true")]
    pub needs_input: bool,
    /// An agent stopped working: quiet for a settle window after a run of work
    /// long enough to be a task rather than a flicker.
    #[serde(default = "default_true")]
    pub finished: bool,
    /// An agent left its pane: it quit, or the pane closed.
    #[serde(default = "default_true")]
    pub exited: bool,
    /// A program to run instead of the platform's notifier, as an argv. An
    /// argument that is exactly `{title}` or `{body}` is replaced by that text.
    /// No shell is involved, so the text can never be read as a command.
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            needs_input: true,
            finished: true,
            exited: true,
            command: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        notifications: NotificationsConfig,
    }

    fn parse(s: &str) -> NotificationsConfig {
        toml::from_str::<Wrapper>(s)
            .expect("config should parse")
            .notifications
    }

    #[test]
    fn an_absent_table_turns_every_event_on() {
        assert_eq!(parse(""), NotificationsConfig::default());
        let cfg = parse("");
        assert!(cfg.enabled && cfg.needs_input && cfg.finished && cfg.exited);
        assert_eq!(cfg.command, None);
    }

    #[test]
    fn a_partial_table_keeps_the_other_defaults() {
        let cfg = parse("[notifications]\nfinished = false\n");
        assert!(!cfg.finished);
        assert!(cfg.enabled && cfg.needs_input && cfg.exited);
    }

    #[test]
    fn the_master_switch_turns_off_alone() {
        let cfg = parse("[notifications]\nenabled = false\n");
        assert!(!cfg.enabled);
        assert!(cfg.needs_input, "the event toggles are independent of it");
    }

    #[test]
    fn a_custom_command_is_an_argv() {
        let cfg = parse(
            r#"
[notifications]
command = ["terminal-notifier", "-title", "{title}", "-message", "{body}"]
"#,
        );
        assert_eq!(
            cfg.command.as_deref(),
            Some(
                &[
                    "terminal-notifier".to_string(),
                    "-title".into(),
                    "{title}".into(),
                    "-message".into(),
                    "{body}".into()
                ][..]
            )
        );
    }
}
