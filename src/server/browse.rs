//! Directory listing and editor resolution for the `files` sidebar panel.
//!
//! Both halves live on the SERVER, and for the same reason: the panel follows
//! the focused pane's directory, that pane is routinely on a remote, and both
//! the files and the editor that opens them are on the remote's filesystem. A
//! client-side `read_dir` would list the client's own machine and look entirely
//! plausible; a client-side `$EDITOR` would name a binary that may not be
//! installed where the file is.
//!
//! Kept out of `daemon.rs` because neither half needs a lock, a client, or a
//! session -- they are pure functions of the filesystem and the environment,
//! and testable as such.

use std::path::Path;

use crate::protocol::DirEntry;

/// The most entries one listing will carry.
///
/// A directory with a hundred thousand files is real (a Maildir, a build cache,
/// `node_modules`). Serialising all of it would put megabytes of JSON on a
/// socket to paint twenty rows. The cap is reported, never silent --
/// [`ServerMessage::DirectoryListing`]'s `truncated`.
///
/// [`ServerMessage::DirectoryListing`]: crate::protocol::ServerMessage::DirectoryListing
pub const MAX_ENTRIES: usize = 5_000;

/// The editor used when neither config nor the environment names one.
///
/// POSIX requires `vi`, which is the only thing that makes it a safe last
/// resort: it is the editor most likely to exist on a machine whose owner never
/// set `$EDITOR`.
pub const FALLBACK_EDITOR: &str = "vi";

/// What one directory holds: its entries, why it could not be read, and whether
/// the listing is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<DirEntry>,
    pub error: Option<String>,
    pub truncated: bool,
}

impl Listing {
    /// A listing that could not be produced, carrying the reason.
    fn failed(msg: String) -> Self {
        Self {
            entries: Vec::new(),
            error: Some(msg),
            truncated: false,
        }
    }
}

/// This server's home directory, as an absolute path string.
///
/// Sent with every listing so the `files` panel can render `~`. It is resolved
/// HERE rather than on the client for the same reason the listing is: the
/// directory being shown belongs to this machine, and the client's own `$HOME`
/// describes a different one whenever the server is a remote.
///
/// `None` when it cannot be resolved (no `$HOME` and no passwd entry), which the
/// panel reads as "show the full path" -- the behaviour it had before `~`.
///
/// A non-UTF-8 home is also `None`: the wire is JSON, and a lossy conversion
/// would be a path that does not exist. The panel loses the shortening, not
/// correctness.
pub fn home_dir() -> Option<String> {
    dirs::home_dir().and_then(|h| h.to_str().map(str::to_string))
}

/// List `path`, sorted directories-first then by name.
///
/// Errors are RETURNED, not logged and swallowed. "Permission denied" is an
/// ordinary answer for a directory, and a panel that renders an empty list for
/// it cannot be told apart from one rendering an empty directory -- the exact
/// ambiguity `detection_supported` was added to remove on the agents panel.
///
/// Hidden files are INCLUDED. The client filters them, which is what lets the
/// `.` toggle be instant instead of a round trip.
///
/// `path` must be ABSOLUTE, and that is ENFORCED here rather than asserted in a
/// doc comment. A relative path is not merely unsupported, it is meaningless
/// across the socket: `read_dir` would resolve it against the SERVER PROCESS's
/// working directory -- whatever directory the daemon happened to be started in,
/// months ago -- and answer with a real, plausible listing of somewhere the
/// client never asked about. Refusing is the only reading that cannot be
/// mistaken for an answer, and it arrives as an ordinary `error` the panel
/// already knows how to show.
pub fn list_directory(path: &Path) -> Listing {
    if !path.is_absolute() {
        return Listing::failed(format!("not an absolute path: {}", path.to_string_lossy()));
    }
    let dir = match std::fs::read_dir(path) {
        Ok(d) => d,
        Err(e) => return Listing::failed(describe_io_error(&e)),
    };
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in dir {
        // One unreadable entry does not fail the directory: `read_dir` yields a
        // per-entry Result, and a file that vanished between the open and the
        // read is normal in a live directory. Skipping it shows the other
        // nine hundred.
        let Ok(item) = item else { continue };
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let name = item.file_name().to_string_lossy().into_owned();
        // `symlink_metadata` describes the LINK, `metadata` its target. Both are
        // wanted: `is_symlink` marks the row, and `is_dir` decides whether Enter
        // descends -- so a symlink to a directory is both, and descends, which
        // is what `cd` through one does. A broken symlink follows nowhere, so
        // `metadata` fails and it is neither.
        let link_meta = item.path().symlink_metadata().ok();
        let is_symlink = link_meta
            .as_ref()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let target_meta = item.path().metadata().ok();
        let is_dir = target_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = target_meta.as_ref().map(|m| m.len()).unwrap_or(0);
        entries.push(DirEntry {
            name,
            is_dir,
            is_symlink,
            size,
        });
    }
    sort_entries(&mut entries);
    Listing {
        entries,
        error: None,
        truncated,
    }
}

/// Directories first, then by name, case-insensitively.
///
/// Case-insensitive because the panel is read by a human scanning a column, and
/// a byte-order sort files every capitalised name above every lowercase one --
/// `README` above `bin`, `src` below `Makefile`. The exact name breaks ties so
/// the order is total and stable across refreshes (`aB` and `Ab` must not swap
/// on every push, which would move the user's selection under them).
fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// A short human reason for a failed listing.
///
/// `io::Error`'s own `Display` appends the OS code (`Permission denied (os
/// error 13)`), which is noise in a panel that is often twenty columns wide.
fn describe_io_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "not found".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        std::io::ErrorKind::NotADirectory => "not a directory".to_string(),
        _ => e.kind().to_string(),
    }
}

/// The argv for opening `path`, given the panel's configured `command`.
///
/// Resolution order: the panel's `command`, else the server's `$EDITOR`, else
/// [`FALLBACK_EDITOR`]. All three are resolved HERE, on the machine holding the
/// file, which is the whole point of doing this server-side.
///
/// The editor string is split on whitespace so `$EDITOR="nvim -R"` and
/// `emacsclient -t` work. That is the documented limitation: an editor whose
/// own path or flags contain spaces cannot be expressed this way. The FILE path
/// is appended as its own element and is never split, so a file called
/// `my notes.txt` opens correctly -- which is the case that actually occurs.
///
/// An editor string that is entirely whitespace falls back rather than
/// producing an empty argv: `EDITOR=""` is a real thing to find in an
/// environment, and exec'ing nothing would leave a pane that dies instantly for
/// no stated reason.
pub fn editor_argv(command: Option<&str>, path: &str) -> Vec<String> {
    let configured = command.map(str::to_string);
    let from_env = || std::env::var("EDITOR").ok();
    let mut argv: Vec<String> = configured
        .or_else(from_env)
        .into_iter()
        .flat_map(|s| {
            s.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .collect();
    if argv.is_empty() {
        argv.push(FALLBACK_EDITOR.to_string());
    }
    argv.push(path.to_string());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory tree, removed when the guard drops.
    struct Tmp(std::path::PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "remux-browse-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(l: &Listing) -> Vec<String> {
        l.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// A relative path is refused, not resolved.
    ///
    /// Probed over the wire before this check existed, `"."` and `"src"`
    /// answered with a genuine listing of the SERVER PROCESS's working
    /// directory -- a real answer about somewhere nobody asked about, which is
    /// worse than an error because it looks right.
    #[test]
    fn a_relative_path_is_refused_rather_than_resolved_against_the_daemons_cwd() {
        for relative in [".", "src", "../etc", ""] {
            let l = list_directory(Path::new(relative));
            assert!(
                l.entries.is_empty(),
                "a relative path must not produce a listing (got {:?} for {relative:?})",
                names(&l)
            );
            let err = l.error.unwrap_or_default();
            assert!(
                err.contains("absolute"),
                "the refusal must say WHY (got {err:?} for {relative:?})"
            );
        }
        // And the enforcement is not simply "everything fails": an absolute path
        // to a real directory still lists.
        let tmp = Tmp::new("absolute");
        std::fs::write(tmp.path().join("a.txt"), b"x").expect("write");
        let ok = list_directory(tmp.path());
        assert_eq!(names(&ok), vec!["a.txt".to_string()]);
        assert_eq!(ok.error, None);
    }

    #[test]
    fn directories_sort_above_files_and_names_ignore_case() {
        let tmp = Tmp::new("sort");
        for f in ["Zebra.txt", "apple.txt", "Beta.txt"] {
            std::fs::write(tmp.path().join(f), b"x").unwrap();
        }
        for d in ["src", "Docs"] {
            std::fs::create_dir(tmp.path().join(d)).unwrap();
        }
        let l = list_directory(tmp.path());
        assert_eq!(l.error, None);
        assert_eq!(
            names(&l),
            vec!["Docs", "src", "apple.txt", "Beta.txt", "Zebra.txt"],
            "directories first, then case-insensitive by name"
        );
    }

    #[test]
    fn hidden_files_are_listed_because_the_client_is_what_hides_them() {
        let tmp = Tmp::new("hidden");
        std::fs::write(tmp.path().join(".secret"), b"x").unwrap();
        std::fs::write(tmp.path().join("plain"), b"x").unwrap();
        assert_eq!(names(&list_directory(tmp.path())), vec![".secret", "plain"]);
    }

    #[test]
    fn a_missing_directory_reports_why_rather_than_looking_empty() {
        let l = list_directory(Path::new("/nonexistent-remux-browse-path"));
        assert!(l.entries.is_empty());
        assert_eq!(l.error.as_deref(), Some("not found"));
    }

    #[test]
    fn listing_a_file_is_an_error_not_an_empty_directory() {
        let tmp = Tmp::new("notadir");
        let f = tmp.path().join("file");
        std::fs::write(&f, b"x").unwrap();
        let l = list_directory(&f);
        assert!(l.error.is_some(), "got {l:?}");
        assert!(l.entries.is_empty());
    }

    #[test]
    fn a_symlink_to_a_directory_is_marked_as_both_and_can_be_descended() {
        let tmp = Tmp::new("symlink");
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("gone"), tmp.path().join("broken")).unwrap();
        let l = list_directory(tmp.path());
        let link = l.entries.iter().find(|e| e.name == "link").unwrap();
        assert!(link.is_dir, "a symlinked directory must be descendable");
        assert!(link.is_symlink);
        let broken = l.entries.iter().find(|e| e.name == "broken").unwrap();
        assert!(!broken.is_dir, "a broken symlink follows nowhere");
        assert!(broken.is_symlink);
    }

    #[test]
    fn a_directory_over_the_cap_is_truncated_and_says_so() {
        let tmp = Tmp::new("cap");
        for i in 0..(MAX_ENTRIES + 10) {
            std::fs::write(tmp.path().join(format!("f{i:06}")), b"").unwrap();
        }
        let l = list_directory(tmp.path());
        assert!(l.truncated, "the cap must be reported, never silent");
        assert_eq!(l.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn a_directory_under_the_cap_is_not_flagged_truncated() {
        let tmp = Tmp::new("uncap");
        std::fs::write(tmp.path().join("only"), b"").unwrap();
        assert!(!list_directory(tmp.path()).truncated);
    }

    #[test]
    fn the_configured_command_wins_and_the_path_is_its_own_argument() {
        assert_eq!(
            editor_argv(Some("hx"), "/tmp/my notes.txt"),
            vec!["hx", "/tmp/my notes.txt"],
            "a path with a space must stay ONE argument"
        );
    }

    #[test]
    fn an_editor_string_with_flags_is_split_into_argv() {
        assert_eq!(
            editor_argv(Some("emacsclient -t"), "/a/b"),
            vec!["emacsclient", "-t", "/a/b"]
        );
    }

    #[test]
    fn with_no_command_and_no_editor_it_falls_back_to_vi() {
        // The environment of the test process is what stands in for the
        // server's here, so state it rather than depending on the runner's.
        temp_env(None, || {
            assert_eq!(editor_argv(None, "/a/b"), vec![FALLBACK_EDITOR, "/a/b"]);
        });
    }

    #[test]
    fn the_servers_editor_is_used_when_the_panel_configures_none() {
        temp_env(Some("nvim -R"), || {
            assert_eq!(editor_argv(None, "/a/b"), vec!["nvim", "-R", "/a/b"]);
        });
    }

    #[test]
    fn an_empty_editor_variable_falls_back_rather_than_exec_ing_nothing() {
        temp_env(Some("   "), || {
            assert_eq!(editor_argv(None, "/a/b"), vec![FALLBACK_EDITOR, "/a/b"]);
        });
    }

    /// `$EDITOR` is process-global, so these three cases are serialised behind
    /// one mutex rather than racing each other under the test harness's threads.
    fn temp_env(value: Option<&str>, f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("EDITOR").ok();
        match value {
            Some(v) => std::env::set_var("EDITOR", v),
            None => std::env::remove_var("EDITOR"),
        }
        f();
        match saved {
            Some(v) => std::env::set_var("EDITOR", v),
            None => std::env::remove_var("EDITOR"),
        }
    }
}
