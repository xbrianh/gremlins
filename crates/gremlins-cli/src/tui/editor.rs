//! External editor integration for the chat prompt.
//!
//! `Ctrl+G` temporarily returns the terminal to cooked mode, opens the
//! user's preferred editor on a draft file seeded with the current input
//! bar contents, then restores the TUI's raw mode and returns whatever the
//! user wrote (or `None` if they aborted without adding anything).

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use crossterm::{
    cursor, execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};

/// Re-enters raw mode and hides the cursor when dropped.
///
/// The guard is created *before* the terminal is put into cooked mode, so
/// even if a later step fails the terminal is always restored.
struct TerminalRestoreGuard;

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = enable_raw_mode();
        let _ = execute!(io::stdout(), cursor::Hide);
        let _ = io::stdout().flush();
    }
}

/// Removes the editor draft file on drop, covering every return path.
struct DraftGuard(PathBuf);

impl Drop for DraftGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Open an external editor so the user can compose a chat prompt.
///
/// Returns `Ok(None)` when the editor is aborted without producing new text
/// (empty file or text identical to the seed). The draft file is always
/// cleaned up, and the terminal is always returned to raw mode.
pub fn open_editor(input: &str) -> io::Result<Option<String>> {
    // Construct this first so terminal restoration is guaranteed even if
    // disabling raw mode or spawning the editor fails.
    let _terminal = TerminalRestoreGuard;

    // Cooked mode + visible cursor are what an interactive editor expects.
    disable_raw_mode()?;
    execute!(io::stdout(), cursor::Show)?;
    io::stdout().flush()?;

    // Resolve the editor command: $EDITOR, then $VISUAL, then vi.
    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("VISUAL").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "vi".to_string());
    let mut parts = shlex::split(&editor).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("failed to parse editor command: {editor}"),
        )
    })?;
    let program = parts.remove(0);
    let args = parts;

    // Create the draft under $state_root/tui/ and seed it with the current
    // input bar contents.
    let draft_dir = gremlins::config::state_root().join("tui");
    std::fs::create_dir_all(&draft_dir)?;
    let draft_path = draft_dir.join("editor-draft.txt");
    let _draft = DraftGuard(draft_path.clone());
    std::fs::write(&draft_path, input)?;

    // The editor owns the terminal while it runs; block until it exits.
    // The tokio runtime keeps servicing background tasks, but no new TUI
    // frames are drawn while the terminal belongs to the editor.
    let _status = Command::new(&program)
        .args(&args)
        .arg(&draft_path)
        .status()?;

    let composed = std::fs::read_to_string(&draft_path)?;

    // vi and friends leave a trailing newline on save; the seed itself
    // never contains one. Strip it before comparing so a saved-but-clear
    // buffer does not become a prompt.
    let composed = composed.trim_end_matches(['\r', '\n']);
    let seed = input.trim_end_matches(['\r', '\n']);

    if composed.trim().is_empty() || composed == seed {
        Ok(None)
    } else {
        Ok(Some(composed.to_string()))
    }
}
