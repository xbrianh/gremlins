use ratatui::{
    layout::{Constraint, Direction, Layout},
    prelude::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::App;

/// Render the bottom-region TUI layout.
///
/// Ratatui draws up to five regions when a turn is active:
/// 1. Prompt area      — user's message, static.
/// 2. Streaming area   — live reasoning + tool results, fixed height, scrolls.
/// 3. Response area    — accumulated model response, fixed height, scrolls.
/// 4. Input bar        — prompt + current input.
/// 5. Info bar         — single-line status.
///
/// When no turn is active sections 1-3 collapse to zero height
/// so that only the input and info bars occupy the viewport.
///
/// The transcript (command output, help text, subprocess results) is inserted
/// above the inline viewport via `terminal.insert_before()` and becomes normal
/// terminal scrollback — it is never ratatui-rendered.
pub fn render(frame: &mut Frame, app: &App, gremlin_count: &str, project_name: &str) {
    let (prompt_h, streaming_h, response_h) = if app.streaming_active {
        let resp_h = if app.response_area_open {
            Constraint::Min(0)
        } else {
            Constraint::Length(0)
        };
        (
            Constraint::Length(app.prompt_lines),
            Constraint::Length(app.streaming_rows),
            resp_h,
        )
    } else {
        (
            Constraint::Length(0),
            Constraint::Length(0),
            Constraint::Length(0),
        )
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            prompt_h,
            streaming_h,
            response_h,
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_prompt(frame, chunks[0], app);
    render_streaming(frame, chunks[1], app);
    render_response(frame, chunks[2], app);
    render_input_bar(frame, chunks[3], app);
    render_info_bar(frame, chunks[4], app, gremlin_count, project_name);
}

fn render_prompt(frame: &mut Frame, area: Rect, app: &App) {
    let prompt_style = Style::default().fg(Color::Cyan);
    let text = format!("> {}", app.prompt);
    let paragraph = Paragraph::new(Line::from(Span::styled(text, prompt_style)));
    frame.render_widget(paragraph, area);
}

fn render_streaming(frame: &mut Frame, area: Rect, app: &App) {
    let reason_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    let mut lines: Vec<Line> = Vec::new();
    for line in app.reasoning_stream.lines() {
        lines.push(Line::from(Span::styled(format!("  {line}"), reason_style)));
    }

    let line_count = lines.len().max(1);
    let scroll = line_count.saturating_sub(area.height as usize) as u16;
    let paragraph = Paragraph::new(lines).scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_response(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    for line in app.response_stream.lines() {
        lines.push(Line::from(Span::styled(
            line,
            Style::default().fg(Color::White),
        )));
    }

    let line_count = lines.len().max(1);
    let scroll = line_count.saturating_sub(area.height as usize) as u16;
    let paragraph = Paragraph::new(lines).scroll((scroll, 0));
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

    // Block cursor rendered as an inverted space *after* the input text,
    // showing where the next character will be inserted.
    let text = Line::from(vec![
        prompt,
        Span::raw(app.input.as_str()),
        Span::styled(" ", Style::default().bg(Color::White)),
    ]);

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, area);
}
