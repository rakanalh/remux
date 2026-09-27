//! `[appearance] pane_title`: the one template that names a pane on every
//! surface (borders, the Monocle strip, the session tree, View cells, the
//! agents panel and the agent switcher).
//!
//! The server renders it for what it composites and pushes; the client renders
//! it for View cells and agent rows, because only the client knows which
//! remote a pane came from (`{host}`).

/// The placeholders a template may use. Anything else in braces is left in
/// the output literally.
pub const PLACEHOLDERS: [&str; 6] = ["title", "command", "session", "tab", "cwd", "host"];

/// What a template is rendered from.
#[derive(Debug, Clone, Copy, Default)]
pub struct NameParts<'a> {
    /// The settled, normalised window title.
    pub title: Option<&'a str>,
    /// The name of the pane's foreground process.
    pub command: &'a str,
    pub session: &'a str,
    pub tab: usize,
    /// The basename of the pane's working directory.
    pub cwd: Option<&'a str>,
    /// The remote's name; empty for the local server.
    pub host: &'a str,
}

/// `template` with every known placeholder substituted.
///
/// There is no escaping. A `{` starts a placeholder that ends at the next `}`,
/// and one whose name is unknown is copied through unchanged, braces included.
/// So `{{title}}` reads as the unknown name `{title` followed by a literal `}`,
/// and renders as `{{title}}`, not as a braced title.
pub fn render(template: &str, parts: &NameParts<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        match name {
            "title" => out.push_str(parts.title.unwrap_or("")),
            "command" => out.push_str(parts.command),
            "session" => out.push_str(parts.session),
            "tab" => out.push_str(&parts.tab.to_string()),
            "cwd" => out.push_str(parts.cwd.unwrap_or("")),
            "host" => out.push_str(parts.host),
            _ => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The name a pane is shown by when the user has not renamed it.
///
/// With no template this is the title, else the command. A template that
/// renders to nothing (`"{title}"` on a pane with no title) falls back to the
/// command, so that a pane is never unnamed.
pub fn display_name(template: Option<&str>, parts: &NameParts<'_>) -> String {
    match template {
        None => parts
            .title
            .filter(|t| !t.is_empty())
            .unwrap_or(parts.command)
            .to_string(),
        Some(template) => {
            let rendered = render(template, parts);
            let trimmed = rendered.trim();
            if trimmed.is_empty() {
                parts.command.to_string()
            } else {
                trimmed.to_string()
            }
        }
    }
}

/// The placeholders in `template` that are not in [`PLACEHOLDERS`].
pub fn unknown_placeholders(template: &str) -> Vec<String> {
    let mut unknown = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break;
        };
        let name = &after[..close];
        if !PLACEHOLDERS.contains(&name) {
            unknown.push(name.to_string());
        }
        rest = &after[close + 1..];
    }
    unknown
}

/// The last path component of `path`, or `path` itself for `/`.
pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|b| !b.is_empty())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> NameParts<'static> {
        NameParts {
            title: Some("Fix the bug"),
            command: "claude",
            session: "main",
            tab: 2,
            cwd: Some("remux"),
            host: "mini",
        }
    }

    #[test]
    fn every_placeholder_renders() {
        assert_eq!(
            render(
                "{command}: {title} [{host}:{session}/{tab} {cwd}]",
                &parts()
            ),
            "claude: Fix the bug [mini:main/2 remux]"
        );
    }

    #[test]
    fn doubled_braces_are_not_an_escape() {
        assert_eq!(render("{{title}}", &parts()), "{{title}}");
        assert_eq!(unknown_placeholders("{{title}}"), vec!["{title"]);
    }

    #[test]
    fn unknown_and_unclosed_placeholders_stay_literal() {
        assert_eq!(render("{nope} {title} {", &parts()), "{nope} Fix the bug {");
        assert_eq!(
            unknown_placeholders("{nope} {title} {cwd} {x"),
            vec!["nope"]
        );
    }

    #[test]
    fn default_is_the_title_then_the_command() {
        assert_eq!(display_name(None, &parts()), "Fix the bug");
        let untitled = NameParts {
            title: None,
            ..parts()
        };
        assert_eq!(display_name(None, &untitled), "claude");
        let empty = NameParts {
            title: Some(""),
            ..parts()
        };
        assert_eq!(display_name(None, &empty), "claude");
    }

    #[test]
    fn a_template_is_trimmed_and_never_renders_blank() {
        let untitled = NameParts {
            title: None,
            ..parts()
        };
        assert_eq!(display_name(Some("  {title}  "), &parts()), "Fix the bug");
        assert_eq!(display_name(Some("{title}"), &untitled), "claude");
        assert_eq!(
            display_name(Some("{command}: {title}"), &untitled),
            "claude:"
        );
    }

    #[test]
    fn basename_takes_the_last_component() {
        assert_eq!(basename("/home/u/src/remux"), "remux");
        assert_eq!(basename("/home/u/src/remux/"), "remux");
        assert_eq!(basename("/"), "/");
    }
}
