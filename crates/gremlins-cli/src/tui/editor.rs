use crate::tui::app::App;

/// Editor stub — appends a placeholder message to the scrollback buffer.
///
/// Wired to `Ctrl+G` from day one so the keybinding is discoverable.
pub fn open_editor(app: &mut App) {
    app.scrollback_lines.push("> Ctrl+G".to_string());
    app.scrollback_lines
        .push("editor not yet implemented".to_string());
}
