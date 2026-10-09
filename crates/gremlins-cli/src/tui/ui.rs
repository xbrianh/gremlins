use ratatui::{
    layout::{Constraint, Direction, Layout},
    prelude::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::tui::app::App;
use crate::tui::widgets::DynamicWidget;

/// Render the bottom-region TUI layout.
///
/// Always produces four vertical constraints (bottom-to-top):
/// scrollback, dynamic transcript, input bar, info bar.
/// Scrollback and transcript heights can be zero.
///
/// The transcript (command output, help text, subprocess results) is inserted
/// above the inline viewport via `terminal.insert_before()` and becomes normal
/// terminal scrollback — it is never ratatui-rendered.
pub fn render(frame: &mut Frame, app: &App, gremlin_count: &str, project_name: &str) {
    let has_widget = app.widget.as_ref().is_some_and(|w| !w.is_empty());
    let area_w = frame.area().width;
    let scrollback_h = app.scrollback_height(area_w);
    let widget_h = if has_widget {
        app.widget.as_ref().map_or(0, |w| w.height())
    } else {
        0
    };

    // Clamp scrollback to available area so input + info bars are never starved.
    let max_scrollback = frame.area().height.saturating_sub(widget_h + 2);
    let clamped_scrollback_h = scrollback_h.min(max_scrollback);
    // When clamped, skip the oldest lines so the tail is visible.
    let skip_lines = if clamped_scrollback_h < scrollback_h {
        let mut h: u16 = 0;
        let wrap_w = (area_w as usize).max(1);
        let mut skip: usize = 0;
        for line in &app.scrollback_lines {
            let chars = line.chars().count();
            let rows = if chars == 0 {
                1
            } else {
                chars.div_ceil(wrap_w)
            } as u16;
            if h + rows > scrollback_h - clamped_scrollback_h {
                break;
            }
            h += rows;
            skip += 1;
        }
        skip
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(clamped_scrollback_h),
            Constraint::Length(widget_h),
            Constraint::Length(1), // input bar
            Constraint::Length(1), // info bar
        ])
        .split(frame.area());

    if clamped_scrollback_h > 0 {
        render_scrollback(frame, chunks[0], app, skip_lines);
    }
    if has_widget {
        if let Some(widget) = &app.widget {
            widget.render(frame, chunks[1]);
        }
    }
    render_input_bar(frame, chunks[2], app);
    render_info_bar(frame, chunks[3], app, gremlin_count, project_name);
}

fn render_scrollback(frame: &mut Frame, area: Rect, app: &App, skip_lines: usize) {
    let prompt_style = Style::default().fg(Color::Cyan);
    let default_style = Style::default();

    let lines: Vec<Line> = app
        .scrollback_lines
        .iter()
        .skip(skip_lines)
        .map(|s| {
            if s.starts_with("> ") {
                Line::from(Span::styled(s.as_str(), prompt_style))
            } else {
                Line::from(Span::styled(s.as_str(), default_style))
            }
        })
        .collect();

    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
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
