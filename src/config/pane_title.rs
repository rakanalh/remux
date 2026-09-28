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

/// Longest label kept, in characters: a window title, a process name, or a
/// name the user gave a pane. The text comes from programs and users, and one
/// of them must not make every name on every surface arbitrarily long.
pub const MAX_LABEL_CHARS: usize = 256;

/// `raw` without control or invisible format characters, cut to
/// [`MAX_LABEL_CHARS`].
///
/// Every name that ends up in a border cell or the session tree goes through
/// this: a window title, a process name (a program sets its own `comm`, or on
/// macOS its `argv[0]`) and a name the user typed. The client prints cells
/// verbatim, so an escape sequence left in a name would reach the user's outer
/// terminal and bypass remux's emulator, its OSC 52 policy included. A bidi
/// override would show a different name from the one stored.
pub fn sanitize_label(raw: &str) -> String {
    printable_chars(raw).take(MAX_LABEL_CHARS).collect()
}

/// `raw` without control or invisible format characters, at any length. For
/// text something else takes a part of before it is shown, such as an
/// `argv[0]` path whose basename names a process: cut to a label's length
/// first, a long path would lose the basename.
pub fn strip_unprintable(raw: &str) -> String {
    printable_chars(raw).collect()
}

fn printable_chars(raw: &str) -> impl Iterator<Item = char> + '_ {
    raw.chars()
        .filter(|&c| !c.is_control() && !is_hidden_format(c))
}

/// Whether `c` is in Unicode category Cf (format), except the zero-width
/// joiner and non-joiner. Those two shape visible text: the joiner builds
/// emoji sequences and the non-joiner is required by Persian and other
/// scripts, so dropping them would change what a title says.
fn is_hidden_format(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{600}'..='\u{605}'
            | '\u{61c}'
            | '\u{6dd}'
            | '\u{70f}'
            | '\u{890}'..='\u{891}'
            | '\u{8e2}'
            | '\u{180e}'
            | '\u{200b}'
            | '\u{200e}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

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
    segments(template, parts)
        .into_iter()
        .map(|s| s.text)
        .collect()
}

/// A piece of a rendered template: text written in the template itself, or
/// what a placeholder rendered to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    text: String,
    literal: bool,
}

/// `template` rendered piece by piece. An unknown placeholder is literal text.
fn segments(template: &str, parts: &NameParts<'_>) -> Vec<Segment> {
    let mut out = Vec::new();
    let literal = |text: &str| Segment {
        text: text.to_string(),
        literal: true,
    };
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push(literal(&rest[..open]));
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push(literal(&rest[open..]));
            return out;
        };
        let name = &after[..close];
        let value = match name {
            "title" => Some(parts.title.unwrap_or("").to_string()),
            "command" => Some(parts.command.to_string()),
            "session" => Some(parts.session.to_string()),
            "tab" => Some(parts.tab.to_string()),
            "cwd" => Some(parts.cwd.unwrap_or("").to_string()),
            "host" => Some(parts.host.to_string()),
            _ => None,
        };
        out.push(match value {
            Some(text) => Segment {
                text,
                literal: false,
            },
            None => literal(&rest[open..open + close + 2]),
        });
        rest = &after[close + 1..];
    }
    out.push(literal(rest));
    out
}

/// Characters that, with whitespace, make up the template text dropped from
/// either end of a rendered name. A placeholder that renders empty leaves the
/// separator beside it dangling (`"{command}: {title}"` with no title renders
/// `zsh:`), and there is no fallback syntax to avoid that.
const SEPARATORS: &[char] = &[':', '-', '|', '·', '/', ',', '–', '—', '•'];

/// The name a pane is shown by when the user has not renamed it.
///
/// With no template this is the title, else the command. From a template,
/// literal text left at either end that is only whitespace and [`SEPARATORS`]
/// is dropped, as is surrounding whitespace in the template text; what a
/// placeholder rendered is never changed, so a title of `/usr/local` or `-v`
/// is shown as it is. A result that is empty (`"{title}"` on a pane with no
/// title) falls back to the command, so that a pane is never unnamed.
pub fn display_name(template: Option<&str>, parts: &NameParts<'_>) -> String {
    sanitize_label(&unsanitized_display_name(template, parts))
}

/// [`display_name`] before [`sanitize_label`]. The parts are sanitised where
/// they are read, and this is the last line of defence for template text.
fn unsanitized_display_name(template: Option<&str>, parts: &NameParts<'_>) -> String {
    match template {
        None => parts
            .title
            .filter(|t| !t.is_empty())
            .unwrap_or(parts.command)
            .to_string(),
        Some(template) => {
            let name = trim_ends(segments(template, parts));
            if name.trim().is_empty() {
                parts.command.to_string()
            } else {
                name
            }
        }
    }
}

/// `segments` joined, without the dangling template text at either end: empty
/// values and all-separator literals are dropped from each end, and the
/// whitespace of a literal left at an end is trimmed.
fn trim_ends(mut segments: Vec<Segment>) -> String {
    let droppable = |s: &Segment| {
        if s.literal {
            s.text
                .chars()
                .all(|c| c.is_whitespace() || SEPARATORS.contains(&c))
        } else {
            s.text.is_empty()
        }
    };
    while segments.first().is_some_and(droppable) {
        segments.remove(0);
    }
    while segments.last().is_some_and(droppable) {
        segments.pop();
    }
    if let Some(first) = segments.first_mut().filter(|s| s.literal) {
        first.text = first.text.trim_start().to_string();
    }
    if let Some(last) = segments.last_mut().filter(|s| s.literal) {
        last.text = last.text.trim_end().to_string();
    }
    segments.into_iter().map(|s| s.text).collect()
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
    fn separators_left_dangling_by_an_empty_placeholder_are_trimmed() {
        let untitled = NameParts {
            title: None,
            host: "",
            ..parts()
        };
        assert_eq!(
            display_name(Some("{title} - {command}"), &untitled),
            "claude"
        );
        assert_eq!(
            display_name(Some("{host} | {command} · {title}"), &untitled),
            "claude"
        );
        assert_eq!(display_name(Some("{host}/{session}"), &untitled), "main");
        assert_eq!(
            display_name(Some("{command}: {title}"), &parts()),
            "claude: Fix the bug",
            "a separator between two values stays"
        );
        assert_eq!(
            display_name(Some("[{command}]"), &untitled),
            "[claude]",
            "brackets are not separators"
        );
    }

    #[test]
    fn a_value_is_never_trimmed() {
        let titled = |title: &'static str| NameParts {
            title: Some(title),
            ..parts()
        };
        assert_eq!(
            display_name(Some("{title}"), &titled("/usr/local")),
            "/usr/local"
        );
        assert_eq!(display_name(Some("{title}"), &titled("-v")), "-v");
        assert_eq!(display_name(Some("{title}"), &titled("/")), "/");
        assert_eq!(display_name(Some("{title} -"), &titled("C++ -")), "C++ -");
        let at_root = NameParts {
            cwd: Some("/"),
            ..parts()
        };
        assert_eq!(display_name(Some("{cwd}"), &at_root), "/");
        assert_eq!(display_name(Some("[{cwd}]"), &at_root), "[/]");
        let zsh = NameParts {
            title: None,
            command: "zsh",
            ..parts()
        };
        assert_eq!(display_name(Some("{command}: {title}"), &zsh), "zsh");
        assert_eq!(
            display_name(Some("{command} {title}"), &titled(" padded ")),
            "claude  padded ",
            "the value keeps its own spaces"
        );
    }

    #[test]
    fn a_name_never_carries_a_control_character() {
        let hostile = NameParts {
            title: Some("t\u{1b}]0;x\u{7}"),
            command: "c\u{9b}d",
            ..parts()
        };
        for template in [
            None,
            Some("{command}: {title}"),
            Some("\u{1b}]52;c;AA==\u{7}{title}"),
        ] {
            let name = display_name(template, &hostile);
            assert!(
                !name.chars().any(char::is_control),
                "{template:?} -> {name:?}"
            );
        }
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
            "claude"
        );
    }

    #[test]
    fn basename_takes_the_last_component() {
        assert_eq!(basename("/home/u/src/remux"), "remux");
        assert_eq!(basename("/home/u/src/remux/"), "remux");
        assert_eq!(basename("/"), "/");
    }
}
