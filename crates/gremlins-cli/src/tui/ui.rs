use ratatui::{
    layout::{Constraint, Direction, Layout},
    prelude::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::{App, STREAMING_HEIGHT};

/// Render the bottom-region TUI layout.
///
/// Ratatui draws three regions:
/// 1. Streaming area — live reasoning + visible text deltas.
/// 2. Input bar   — prompt + current input.
/// 3. Info bar    — single-line status.
///
/// The transcript (command output, help text, subprocess results) is inserted
/// above the inline viewport via `terminal.insert_before()` and becomes normal
/// terminal scrollback — it is never ratatui-rendered.
pub fn render(frame: &mut Frame, app: &App, gremlin_count: &str, project_name: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(STREAMING_HEIGHT),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_streaming(frame, chunks[0], app);
    render_input_bar(frame, chunks[1], app);
    render_info_bar(frame, chunks[2], app, gremlin_count, project_name);
}

fn render_streaming(frame: &mut Frame, area: Rect, app: &App) {
    let line_count = app.stream_text.lines().count();
    let scroll = line_count.saturating_sub(area.height as usize) as u16;
    let paragraph = Paragraph::new(app.stream_text.as_str()).scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_info_bar(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    gremlin_count: &str,
    project_name: &str,
) {
    let mut spans = vec![
        Span::styled("gremlins tui", Style::default().fg(Color::Cyan)),
        Span::raw(" · "),
        Span::styled(
            format!("{gremlin_count} gremlins"),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(" · "),
        Span::styled(
            format!("project: {project_name}"),
            Style::default().fg(Color::Magenta),
        ),
    ];

    if let Some(ref id) = app.following_log {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            format!("following: {id}"),
            Style::default().fg(Color::Red),
        ));
    }

    spans.push(Span::raw("  |  "));
    spans.push(Span::styled("Ctrl+G", Style::default().fg(Color::Green)));
    spans.push(Span::raw(" editor  "));
    spans.push(Span::styled("/help", Style::default().fg(Color::Green)));

    let paragraph = Paragraph::new(Line::from(spans));
    frame.render_widget(paragraph, area);
}

fn render_input_bar(frame: &mut Frame, area: Rect, app: &App) {
    let prompt = Span::styled("> ", Style::default().fg(Color::Cyan));

    // Block cursor: render the last character with inverted style.
    let text = if app.input.is_empty() {
        Line::from(vec![
            prompt,
            Span::styled(" ", Style::default().bg(Color::White)),
        ])
    } else {
        let last_char_idx = app
            .input
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        let before_last = &app.input[..last_char_idx];
        let last_char = &app.input[last_char_idx..];
        Line::from(vec![
            prompt,
            Span::raw(before_last),
            Span::styled(
                last_char,
                Style::default().bg(Color::White).fg(Color::Black),
            ),
        ])
    };

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, area);
}
