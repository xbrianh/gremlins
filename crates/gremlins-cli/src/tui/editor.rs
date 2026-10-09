use ratatui::style::{Color, Style};

use crate::tui::app::App;

/// Editor stub — appends a placeholder message to the scrollback buffer.
///
/// Wired to `Ctrl+G` from day one so the keybinding is discoverable.
pub fn open_editor(app: &mut App) {
    let prompt_style = Style::default().fg(Color::Cyan);
    app.push_system(vec![
        ("> Ctrl+G".to_string(), prompt_style),
        ("editor not yet implemented".to_string(), Style::default()),
    ]);
}
