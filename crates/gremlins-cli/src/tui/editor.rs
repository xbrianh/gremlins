//! External editor integration for the chat prompt.
//!
//! `Ctrl+G` temporarily returns the terminal to cooked mode, opens the
//! user's preferred editor on a draft file seeded with the current input
//! bar contents, then restores the TUI's raw mode and returns whatever the
//! user wrote (or `None` if they aborted without adding anything).

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use crossterm::{
    cursor, execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use tokio::sync::{mpsc, Notify};

/// Coordination handle for suspending and resuming the background crossterm
/// input reader while an external editor owns the terminal.
///
/// The event loop owns the reader task; `open_editor` receives this handle
/// so it can park the reader (and confirm the park) before handing the
/// terminal over, and wake it again once raw mode is restored.
pub struct SuspendHandle {
    suspend_tx: mpsc::UnboundedSender<()>,
    parked: Arc<Notify>,
    dead: Arc<Notify>,
    resume_tx: mpsc::UnboundedSender<()>,
}

/// The reader-task side of a [`SuspendHandle`], returned by [`SuspendHandle::new`].
pub struct ReaderChannels {
    pub suspend_rx: mpsc::UnboundedReceiver<()>,
    pub resume_rx: mpsc::UnboundedReceiver<()>,
    pub parked: Arc<Notify>,
    pub dead: Arc<Notify>,
}

impl SuspendHandle {
    /// Create a suspend handle and the matching reader-side channels.
    pub fn new() -> (Self, ReaderChannels) {
        let (suspend_tx, suspend_rx) = mpsc::unbounded_channel();
        let (resume_tx, resume_rx) = mpsc::unbounded_channel();
        let parked = Arc::new(Notify::new());
        let dead = Arc::new(Notify::new());
        (
            SuspendHandle {
                suspend_tx,
                parked: Arc::clone(&parked),
                dead: Arc::clone(&dead),
                resume_tx,
            },
            ReaderChannels {
                suspend_rx,
                resume_rx,
                parked,
                dead,
            },
        )
    }

    /// Signal the reader to park and wait until it acknowledges.
    ///
    /// Returns an error if the reader is already gone, or exits before it
    /// can acknowledge the park, so Ctrl+G never hangs on a dead reader.
    pub async fn suspend(&self) -> io::Result<()> {
        self.suspend_tx
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "input reader has stopped"))?;

        tokio::select! {
            _ = self.parked.notified() => Ok(()),
            _ = self.dead.notified() => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "input reader stopped before parking",
            )),
        }
    }

    /// Wake the parked reader so it resumes polling crossterm.
    pub fn resume(&self) {
        let _ = self.resume_tx.send(());
    }
}

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

/// Resumes the suspended input reader when dropped.
///
/// `open_editor` creates this guard after parking the reader and returns
/// it to the event loop as part of [`EditorOutcome`]. The event loop keeps
/// it alive while draining the crossterm channel, then drops it to wake the
/// reader only after the drain is complete.
pub struct ResumeGuard<'a>(&'a SuspendHandle);

impl Drop for ResumeGuard<'_> {
    fn drop(&mut self) {
        self.0.resume();
    }
}

/// Result of an external-editor session.
///
/// The reader remains parked until the caller drops [`EditorOutcome::resume`]
/// via [`EditorOutcome::into_parts`], so the event loop can drain the
/// crossterm channel while it is guaranteed not to change.
pub struct EditorOutcome<'a> {
    result: io::Result<Option<String>>,
    resume: ResumeGuard<'a>,
}

impl<'a> EditorOutcome<'a> {
    /// Split the editor result from the reader-resume guard.
    pub fn into_parts(self) -> (io::Result<Option<String>>, ResumeGuard<'a>) {
        (self.result, self.resume)
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
///
/// The background input reader is suspended (and confirmed parked) before
/// the terminal is handed over, so the editor is the sole reader while it
/// runs, and resumed after raw mode is restored.
pub async fn open_editor<'a>(
    input: &str,
    suspend: &'a SuspendHandle,
) -> io::Result<EditorOutcome<'a>> {
    // Park the background reader and wait for the acknowledgment so the
    // editor never races it for keystrokes. A failed park is returned to
    // the event loop before any terminal state is touched.
    suspend.suspend().await?;

    // The reader is now parked. Keep it parked until the caller has drained
    // the crossterm channel; the `ResumeGuard` is returned to the caller for
    // that purpose.
    let resume = ResumeGuard(suspend);

    // Declare the terminal guard after `ResumeGuard` and before any fallible
    // work, so on every path the raw-mode restoration guard drops first and
    // the (returned) resume guard is only dropped later by the caller.
    let _terminal = TerminalRestoreGuard;

    let result = run_editor_process(input);

    Ok(EditorOutcome { result, resume })
}

/// Run the external editor and read back the composed prompt.
///
/// The terminal is handed over to the editor by the caller after parking the
/// input reader, so this function assumes it owns the terminal until it
/// returns. The caller's [`TerminalRestoreGuard`] still restores raw mode on
/// every return path.
fn run_editor_process(input: &str) -> io::Result<Option<String>> {
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
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("editor resolved to empty command: {editor}"),
        ));
    }
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
    // The reader is parked, so no other task competes for stdin.
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
