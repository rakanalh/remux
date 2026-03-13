//! Editor integration for scrollback viewing.
//!
//! This module provides functionality to open the scrollback buffer content
//! in the user's preferred text editor as a read-only file.

use anyhow::Result;
use std::io::Write;

/// Open the scrollback content in the user's `$EDITOR` as a read-only file.
///
/// The content is written to a temporary file which is automatically cleaned
/// up when this function returns. For vim/nvim, the `-R` flag is used to
/// open in readonly mode.
///
/// The caller is responsible for restoring terminal state (exiting raw mode,
/// leaving the alternate screen) before calling this function, and
/// re-entering raw mode / alternate screen after it returns.
pub fn open_in_editor(content: &str) -> Result<()> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    let mut tmp = tempfile::NamedTempFile::new()?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    let path = tmp.path().to_path_buf();

    let mut cmd = std::process::Command::new(&editor);

    // For vim/nvim, use -R for readonly mode.
    if editor.contains("vim") || editor.contains("nvim") {
        cmd.arg("-R");
    }
    cmd.arg(&path);

    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("editor exited with status: {}", status);
    }

    Ok(())
}

/// Copy text to the system clipboard using the OSC 52 escape sequence.
///
/// OSC 52 is supported by most modern terminal emulators and works over SSH,
/// making it the most portable clipboard mechanism available.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    print!("\x1b]52;c;{}\x07", encoded);
    std::io::stdout().flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_copy_to_clipboard_does_not_panic() {
        // We cannot test actual clipboard integration in a headless environment,
        // but we can verify the function does not panic on an empty string.
        // In CI, stdout may not be a terminal, so we just check it doesn't crash.
        // The actual OSC 52 output goes to stdout which we cannot capture here.
        let _ = copy_to_clipboard("test");
    }

    #[test]
    fn test_base64_encoding() {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode("hello world");
        assert_eq!(encoded, "aGVsbG8gd29ybGQ=");
    }
}
