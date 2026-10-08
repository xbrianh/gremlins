use ratatui::{
    layout::{Constraint, Direction, Layout},
    prelude::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::App;
use crate::tui::widgets::DynamicWidget;

/// Render the bottom-region TUI layout.
///
/// Two branches:
/// - **Active widget:** prompt + widget + input-bar + info-bar.
/// - **Idle:** input-bar + info-bar only.
///
/// The transcript (command output, help text, subprocess results) is inserted
/// above the inline viewport via `terminal.insert_before()` and becomes normal
/// terminal scrollback — it is never ratatui-rendered.
pub fn render(frame: &mut Frame, app: &App, gremlin_count: &str, project_name: &str) {
    let has_widget = app.widget.as_ref().is_some_and(|w| !w.is_empty());
    let has_prompt = !app.prompt.is_empty();

    if has_widget {
        let prompt_h = if has_prompt {
            Constraint::Length(app.prompt_lines())
        } else {
            Constraint::Length(0)
        };
        let widget_h = Constraint::Length(app.widget.as_ref().map_or(0, |w| w.height()));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                prompt_h,
                widget_h,
                Constraint::Length(1), // input bar
                Constraint::Length(1), // info bar
            ])
            .split(frame.area());

        if has_prompt {
            render_prompt(frame, chunks[0], app);
        }
        if let Some(widget) = &app.widget {
            widget.render(frame, chunks[1]);
        }
        render_input_bar(frame, chunks[2], app);
        render_info_bar(frame, chunks[3], app, gremlin_count, project_name);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // input bar
                Constraint::Length(1), // info bar
            ])
            .split(frame.area());

        render_input_bar(frame, chunks[0], app);
        render_info_bar(frame, chunks[1], app, gremlin_count, project_name);
    }
}

fn render_prompt(frame: &mut Frame, area: Rect, app: &App) {
    let prompt_style = Style::default().fg(Color::Cyan);
    let text = format!("> {}", app.prompt);
    let paragraph = Paragraph::new(Line::from(Span::styled(text, prompt_style)));
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
