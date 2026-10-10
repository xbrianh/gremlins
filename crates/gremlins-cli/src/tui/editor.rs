use crate::tui::app::App;

/// Editor stub — appends a placeholder message to the transcript.
///
/// Wired to `Ctrl+G` from day one so the keybinding is discoverable.
pub fn open_editor(app: &mut App) {
    app.push_transcript_line("> Ctrl+G".to_string());
    app.push_transcript_line("editor not yet implemented".to_string());
}
