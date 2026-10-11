use crate::tui::screen::LiveArea;

/// Editor stub — appends a placeholder message to the transcript.
///
/// Wired to `Ctrl+G` from day one so the keybinding is discoverable.
pub fn open_editor(live_area: &mut LiveArea) {
    live_area.push_transcript_line("> Ctrl+G".to_string());
    live_area.push_transcript_line("editor not yet implemented".to_string());
}
