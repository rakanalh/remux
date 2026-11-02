//! The one edit remux makes to Codex's `config.toml`: `[features] hooks =
//! true`, without which an older Codex never runs `hooks.json`.
//!
//! Through `toml_edit`, which keeps comments, key order and layout: a `toml`
//! round trip drops every comment and reorders the tables, and the file is the
//! user's.

use anyhow::{Context, Result};
use toml_edit::{value, DocumentMut, Item, Table};

/// `text` with `[features] hooks = true`, or `None` when it already says so,
/// so that an install that changes nothing writes nothing. The `[features]`
/// table is created only when it is absent.
pub fn ensure_features_hooks(text: &str) -> Result<Option<String>> {
    let mut doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    if features_hooks(&doc) == Some(true) {
        return Ok(None);
    }
    let features = doc
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()));
    let Some(table) = features.as_table_like_mut() else {
        anyhow::bail!("config.toml has a `features` key that is not a table");
    };
    table.insert("hooks", value(true));
    Ok(Some(doc.to_string()))
}

/// What `[features] hooks` says, if it is a boolean.
pub fn features_hooks(doc: &DocumentMut) -> Option<bool> {
    doc.get("features")?.get("hooks")?.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED: &str = "\
# my codex config
model = \"o3\" # the good one

[features]
# keep this one on
web_search = true
other = false

[profiles.work]
model = \"gpt\"
";

    #[test]
    fn adds_hooks_to_an_existing_features_table_keeping_every_other_byte() {
        let after = ensure_features_hooks(COMMENTED).unwrap().unwrap();
        assert_eq!(
            after,
            COMMENTED.replace("other = false\n", "other = false\nhooks = true\n")
        );
    }

    #[test]
    fn creates_the_features_table_once_when_it_is_absent() {
        let text = "# top\nmodel = \"o3\"\n\n[profiles.work]\nmodel = \"gpt\"\n";
        let after = ensure_features_hooks(text).unwrap().unwrap();
        assert!(after.starts_with(text), "{after}");
        assert_eq!(after.matches("[features]").count(), 1, "{after}");
        assert!(after.ends_with("[features]\nhooks = true\n"), "{after:?}");
        assert_eq!(ensure_features_hooks(&after).unwrap(), None);
    }

    #[test]
    fn an_empty_file_gains_just_the_table() {
        assert_eq!(
            ensure_features_hooks("").unwrap().as_deref(),
            Some("[features]\nhooks = true\n")
        );
    }

    #[test]
    fn already_true_is_no_change_at_all() {
        let text = COMMENTED.replace("other = false\n", "other = false\nhooks = true # mine\n");
        assert_eq!(ensure_features_hooks(&text).unwrap(), None);
    }

    #[test]
    fn false_is_turned_on_in_place() {
        let text = "[features]\nhooks = false\nweb_search = true\n";
        assert_eq!(
            ensure_features_hooks(text).unwrap().as_deref(),
            Some("[features]\nhooks = true\nweb_search = true\n")
        );
    }

    #[test]
    fn an_inline_features_table_is_edited_where_it_is() {
        let text = "features = { web_search = true }\n";
        let after = ensure_features_hooks(text).unwrap().unwrap();
        assert!(after.contains("web_search = true"), "{after}");
        assert_eq!(after.matches("features").count(), 1, "{after}");
        let doc: DocumentMut = after.parse().unwrap();
        assert_eq!(features_hooks(&doc), Some(true));
    }

    #[test]
    fn invalid_toml_and_a_scalar_features_key_are_refused() {
        assert!(ensure_features_hooks("[features\n").is_err());
        assert!(ensure_features_hooks("features = 1\n").is_err());
    }
}
