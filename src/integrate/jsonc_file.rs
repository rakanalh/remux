//! The one edit remux makes to OpenCode's `tui.json` / `tui.jsonc`: an entry
//! in its top-level `plugin` array.
//!
//! Through `jsonc-parser`'s CST, which keeps every comment, trailing comma and
//! piece of whitespace outside the array element it adds or removes. A
//! `serde_json` round trip refuses the comments outright, and the file is the
//! user's.

use anyhow::{Context, Result};
use jsonc_parser::cst::{CstArray, CstRootNode};
use jsonc_parser::ParseOptions;

pub const TUI_SCHEMA: &str = "https://opencode.ai/tui.json";

fn parse(text: &str) -> Result<CstRootNode> {
    CstRootNode::parse(text, &ParseOptions::default())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("not valid JSON with comments")
}

/// The `plugin` array, created when `create` and it is absent; an error when
/// the file is not an object or `plugin` is not an array.
fn plugin_array(root: &CstRootNode, create: bool) -> Result<Option<CstArray>> {
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
            .array_value_or_create("plugin")
            .context("its `plugin` key is not an array")
            .map(Some);
    }
    match object.get("plugin") {
        None => Ok(None),
        Some(_) => object
            .array_value("plugin")
            .context("its `plugin` key is not an array")
            .map(Some),
    }
}

fn is_entry(node: &jsonc_parser::cst::CstNode, entry: &str) -> bool {
    node.as_string_lit()
        .and_then(|lit| lit.decoded_value().ok())
        .is_some_and(|value| value == entry)
}

/// Whether the `plugin` array lists `entry`.
pub fn has_plugin(text: &str, entry: &str) -> Result<bool> {
    let root = parse(text)?;
    Ok(plugin_array(&root, false)?
        .is_some_and(|array| array.elements().iter().any(|node| is_entry(node, entry))))
}

/// `text` with `entry` appended to the `plugin` array, the array created when
/// absent; `None` when it is already listed, so nothing is written.
pub fn add_plugin(text: &str, entry: &str) -> Result<Option<String>> {
    if has_plugin(text, entry)? {
        return Ok(None);
    }
    let root = parse(text)?;
    let array = plugin_array(&root, true)?.context("the file is not a JSON object")?;
    array.append(entry.into());
    Ok(Some(root.to_string()))
}

/// `text` without `entry` in the `plugin` array; `None` when it is not
/// listed. The array itself stays, emptied or not: it is the user's key.
pub fn remove_plugin(text: &str, entry: &str) -> Result<Option<String>> {
    let root = parse(text)?;
    let Some(array) = plugin_array(&root, false)? else {
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

    const ENTRY: &str = "/home/u/.config/opencode/remux-tui-session.js";

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
        let after = add_plugin(COMMENTED, ENTRY).unwrap().unwrap();
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
        assert!(has_plugin(&after, ENTRY).unwrap());
        let mine = after.find("mine.js").unwrap();
        let ours = after.find(ENTRY).unwrap();
        assert!(mine < ours, "appended after the user's entry: {after}");
    }

    #[test]
    fn add_is_no_change_when_listed() {
        let once = add_plugin(COMMENTED, ENTRY).unwrap().unwrap();
        assert_eq!(add_plugin(&once, ENTRY).unwrap(), None);
    }

    #[test]
    fn remove_takes_back_exactly_what_add_put() {
        let once = add_plugin(COMMENTED, ENTRY).unwrap().unwrap();
        let back = remove_plugin(&once, ENTRY).unwrap().unwrap();
        assert!(!back.contains(ENTRY), "{back}");
        for kept in [
            "// my TUI config",
            "/* keep */",
            "// the one I wrote",
            "\"./plugins/mine.js\"",
        ] {
            assert!(back.contains(kept), "{kept} lost: {back}");
        }
        assert_eq!(remove_plugin(COMMENTED, ENTRY).unwrap(), None);
    }

    #[test]
    fn a_file_without_the_array_gains_it() {
        let text = "{\n  // just a theme\n  \"theme\": \"x\"\n}\n";
        let after = add_plugin(text, ENTRY).unwrap().unwrap();
        assert!(after.contains("// just a theme"), "{after}");
        assert!(has_plugin(&after, ENTRY).unwrap(), "{after}");
        assert_eq!(remove_plugin(text, ENTRY).unwrap(), None);
    }

    #[test]
    fn the_new_file_holds_the_schema_and_the_entry() {
        let text = new_tui_json(ENTRY);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "$schema": TUI_SCHEMA, "plugin": [ENTRY] })
        );
        assert!(has_plugin(&text, ENTRY).unwrap());
        assert_eq!(add_plugin(&text, ENTRY).unwrap(), None);
    }

    #[test]
    fn an_entry_with_options_is_not_ours_and_bad_shapes_are_refused() {
        let text = format!("{{ \"plugin\": [[\"{ENTRY}\", {{}}]] }}");
        assert!(!has_plugin(&text, ENTRY).unwrap());
        assert!(add_plugin("[1, 2]", ENTRY).is_err());
        assert!(add_plugin("{ \"plugin\": 3 }", ENTRY).is_err());
        assert!(add_plugin("{ \"plugin\": [", ENTRY).is_err());
    }
}
