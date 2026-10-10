use ratatui::{
    layout::{Constraint, Direction, Layout},
    prelude::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::{App, Overlay};
use crate::tui::overlays::{
    render_debug_overlay, render_watch_overlay, render_watch_single_overlay,
};

/// Render the bottom-region TUI layout.
///
/// Always produces three vertical constraints (top-to-bottom):
/// transcript (absorbs all free space), input bar, info bar.
///
/// Widgets are rendered bottom-up. Widgets that don't fit scroll off the
/// top into the terminal's scrollback buffer.
pub fn render(frame: &mut Frame, app: &App, gremlin_count: &str, project_name: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // transcript — absorbs all free space
            Constraint::Length(1), // input bar
            Constraint::Length(1), // info bar
        ])
        .split(frame.area());

    render_transcript(frame, chunks[0], app);
    render_input_bar(frame, chunks[1], app);
    render_info_bar(frame, chunks[2], app, gremlin_count, project_name);
}

/// Render the transcript area, splitting it when an overlay is active.
fn render_transcript(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }

    match &app.overlay {
        Some(Overlay::Watch) => {
            // Reserve the upper portion for the watch table; keep rendering
            // the transcript below so recent widgets remain visible.
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(45), Constraint::Min(0)])
                .split(area);
            render_watch_overlay(frame, split[0], app);
            render_widgets_bottom_up(frame, split[1], app);
        }
        Some(Overlay::WatchSingle { .. }) => {
            render_watch_single_overlay(frame, area, app);
        }
        Some(Overlay::Debug { .. }) => {
            render_debug_overlay(frame, area, app);
        }
        None => render_widgets_bottom_up(frame, area, app),
    }
}

/// Render widgets bottom-up. Stop when the transcript area is full.
/// Widgets taller than the remaining space are still rendered — they get
/// a clipped area and use internal paragraph scrolling to show content.
///
/// Layout is two-pass: non-greedy widgets (finished turns, system messages)
/// are placed first at their natural heights, then the greedy widget (the
/// active turn) absorbs the leftover rows so it fills slack without erasing
/// history.
fn render_widgets_bottom_up(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }

    let width = area.width;

    // Pass 1: measure the total natural height of non-greedy widgets.
    let mut non_greedy_h = 0u16;
    for widget in app.transcript.iter().rev() {
        if widget.is_greedy() {
            break;
        }
        non_greedy_h = non_greedy_h.saturating_add(widget.height(width));
    }

    // The greedy widget gets everything not claimed by non-greedy widgets,
    // bounded by the transcript area.
    let greedy_h = area.height.saturating_sub(non_greedy_h);

    let mut y = area.bottom();
    for widget in app.transcript.iter().rev() {
        let h = widget.height(width);
        if h == 0 {
            continue;
        }
        if y <= area.y {
            // No space left at all.
            break;
        }
        // A greedy widget (the active turn) absorbs the leftover space
        // instead of its natural height. Non-greedy widgets use their
        // natural height, clipped to whatever space remains.
        let visible_h = if widget.is_greedy() {
            greedy_h.min(y - area.y)
        } else {
            h.min(y - area.y)
        };
        y = y.saturating_sub(visible_h);
        let widget_area = Rect {
            x: area.x,
            y,
            width,
            height: visible_h,
        };
        widget.render(frame, widget_area);
    }
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
