use crate::tui::app::App;

/// Editor stub — appends a placeholder message to the output buffer.
///
/// Wired to `Ctrl+G` from day one so the keybinding is discoverable.
pub fn open_editor(app: &mut App) {
    app.push_line("> Ctrl+G");
    app.push_line("editor not yet implemented");
}
