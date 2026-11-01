//! The one edit remux makes to OpenCode's config files: an entry in a
//! top-level plugin array, `plugin` in 1.x's `tui.json` / `tui.jsonc` and
//! `plugins` in 2.x's `cli.json`.
//!
//! Through `jsonc-parser`'s CST, which keeps every comment, trailing comma and
//! piece of whitespace outside the array element it adds or removes. A
//! `serde_json` round trip refuses the comments outright, and the file is the
//! user's.

use anyhow::{Context, Result};
use jsonc_parser::cst::{CstArray, CstRootNode};
use jsonc_parser::ParseOptions;

pub const TUI_SCHEMA: &str = "https://opencode.ai/tui.json";
/// The plugin array of `tui.json(c)`.
pub const TUI_KEY: &str = "plugin";
/// The plugin array of `cli.json`.
pub const CLI_KEY: &str = "plugins";

fn parse(text: &str) -> Result<CstRootNode> {
    CstRootNode::parse(text, &ParseOptions::default())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("not valid JSON with comments")
}

/// The `key` array, created when `create` and it is absent; an error when
/// the file is not an object or `key` is not an array.
fn plugin_array(root: &CstRootNode, key: &str, create: bool) -> Result<Option<CstArray>> {
    let object = if create {
        root.object_value_or_create()
    } else {
        match root.value() {
            None => return Ok(None),
            Some(_) => root.object_value(),
        }
    }
    .context("the file is not a JSON object")?;
    if create {
        return object
            .array_value_or_create(key)
            .with_context(|| format!("its `{key}` key is not an array"))
            .map(Some);
    }
    match object.get(key) {
        None => Ok(None),
        Some(_) => object
            .array_value(key)
            .with_context(|| format!("its `{key}` key is not an array"))
            .map(Some),
    }
}

fn is_entry(node: &jsonc_parser::cst::CstNode, entry: &str) -> bool {
    node.as_string_lit()
        .and_then(|lit| lit.decoded_value().ok())
        .is_some_and(|value| value == entry)
}

/// Whether the `key` array lists `entry`.
pub fn has_plugin(text: &str, key: &str, entry: &str) -> Result<bool> {
    let root = parse(text)?;
    Ok(plugin_array(&root, key, false)?
        .is_some_and(|array| array.elements().iter().any(|node| is_entry(node, entry))))
}

/// `text` with `entry` appended to the `key` array, the array created when
/// absent; `None` when it is already listed, so nothing is written.
pub fn add_plugin(text: &str, key: &str, entry: &str) -> Result<Option<String>> {
    if has_plugin(text, key, entry)? {
        return Ok(None);
    }
    let root = parse(text)?;
    let array = plugin_array(&root, key, true)?.context("the file is not a JSON object")?;
    array.append(entry.into());
    Ok(Some(root.to_string()))
}

/// `text` without `entry` in the `key` array; `None` when it is not listed.
/// The array itself stays, emptied or not: it is the user's key.
pub fn remove_plugin(text: &str, key: &str, entry: &str) -> Result<Option<String>> {
    let root = parse(text)?;
    let Some(array) = plugin_array(&root, key, false)? else {
        return Ok(None);
    };
    let ours: Vec<_> = array
        .elements()
        .into_iter()
        .filter(|node| is_entry(node, entry))
        .collect();
    if ours.is_empty() {
        return Ok(None);
    }
    for node in ours {
        node.remove();
    }
    Ok(Some(root.to_string()))
}

/// A new `tui.json` holding the schema and `entry` alone.
pub fn new_tui_json(entry: &str) -> String {
    let value = serde_json::json!({ "$schema": TUI_SCHEMA, "plugin": [entry] });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTRY: &str = "/home/u/.config/opencode/plugins/remux-tui-session/tui.js";

    const COMMENTED: &str = r#"{
  // my TUI config
  "$schema": "https://opencode.ai/tui.json",
  "theme": "tokyonight", /* keep */
  "plugin": [
    "./plugins/mine.js", // the one I wrote
  ],
}
"#;

    #[test]
    fn add_appends_one_entry_and_keeps_every_comment() {
        let after = add_plugin(COMMENTED, TUI_KEY, ENTRY).unwrap().unwrap();
        for kept in [
            "// my TUI config",
            "/* keep */",
            "// the one I wrote",
            "\"./plugins/mine.js\"",
        ] {
            assert!(after.contains(kept), "{kept} lost: {after}");
        }
        assert_eq!(after.matches(ENTRY).count(), 1, "{after}");
        assert!(after.ends_with("}\n"), "trailing newline: {after:?}");
        assert!(has_plugin(&after, TUI_KEY, ENTRY).unwrap());
        let mine = after.find("mine.js").unwrap();
        let ours = after.find(ENTRY).unwrap();
        assert!(mine < ours, "appended after the user's entry: {after}");
    }

    #[test]
    fn add_is_no_change_when_listed() {
        let once = add_plugin(COMMENTED, TUI_KEY, ENTRY).unwrap().unwrap();
        assert_eq!(add_plugin(&once, TUI_KEY, ENTRY).unwrap(), None);
    }

    #[test]
    fn remove_takes_back_exactly_what_add_put() {
        let once = add_plugin(COMMENTED, TUI_KEY, ENTRY).unwrap().unwrap();
        let back = remove_plugin(&once, TUI_KEY, ENTRY).unwrap().unwrap();
        assert!(!back.contains(ENTRY), "{back}");
        for kept in [
            "// my TUI config",
            "/* keep */",
            "// the one I wrote",
            "\"./plugins/mine.js\"",
        ] {
            assert!(back.contains(kept), "{kept} lost: {back}");
        }
        assert_eq!(remove_plugin(COMMENTED, TUI_KEY, ENTRY).unwrap(), None);
    }

    #[test]
    fn a_file_without_the_array_gains_it() {
        let text = "{\n  // just a theme\n  \"theme\": \"x\"\n}\n";
        let after = add_plugin(text, TUI_KEY, ENTRY).unwrap().unwrap();
        assert!(after.contains("// just a theme"), "{after}");
        assert!(has_plugin(&after, TUI_KEY, ENTRY).unwrap(), "{after}");
        assert_eq!(remove_plugin(text, TUI_KEY, ENTRY).unwrap(), None);
    }

    #[test]
    fn the_new_file_holds_the_schema_and_the_entry() {
        let text = new_tui_json(ENTRY);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "$schema": TUI_SCHEMA, "plugin": [ENTRY] })
        );
        assert!(has_plugin(&text, TUI_KEY, ENTRY).unwrap());
        assert_eq!(add_plugin(&text, TUI_KEY, ENTRY).unwrap(), None);
    }

    #[test]
    fn an_entry_with_options_is_not_ours_and_bad_shapes_are_refused() {
        let text = format!("{{ \"plugin\": [[\"{ENTRY}\", {{}}]] }}");
        assert!(!has_plugin(&text, TUI_KEY, ENTRY).unwrap());
        assert!(add_plugin("[1, 2]", TUI_KEY, ENTRY).is_err());
        assert!(add_plugin("{ \"plugin\": 3 }", TUI_KEY, ENTRY).is_err());
        assert!(add_plugin("{ \"plugin\": [", TUI_KEY, ENTRY).is_err());
    }

    const CLI: &str = r#"{
  // OpenCode 2 settings
  "$schema": "https://opencode.ai/v2/cli.json",
  "plugins": [
    "./plugins/mine",
    { "package": "@acme/x", "options": { "path": "/home/u/.config/opencode/plugins/remux-tui-session/tui.js" } },
    "-opencode.tips",
    "/home/u/.config/opencode/plugins/remux-tui-session/tui.js",
  ],
}
"#;

    #[test]
    fn the_cli_json_plugins_array_loses_only_our_string() {
        assert!(has_plugin(CLI, CLI_KEY, ENTRY).unwrap());
        assert!(!has_plugin(CLI, TUI_KEY, ENTRY).unwrap());
        let after = remove_plugin(CLI, CLI_KEY, ENTRY).unwrap().unwrap();
        let expected = CLI.replace(&format!("    \"{ENTRY}\",\n"), "");
        assert_ne!(expected, CLI);
        assert_eq!(after, expected);
        assert_eq!(remove_plugin(&after, CLI_KEY, ENTRY).unwrap(), None);
    }

    #[test]
    fn the_other_key_is_never_read_as_ours() {
        let tui = add_plugin("{}\n", TUI_KEY, ENTRY).unwrap().unwrap();
        assert_eq!(remove_plugin(&tui, CLI_KEY, ENTRY).unwrap(), None);
        assert!(add_plugin("{ \"plugins\": 3 }", CLI_KEY, ENTRY).is_err());
    }
}
