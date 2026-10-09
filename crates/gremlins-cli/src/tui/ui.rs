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
/// Always produces four vertical constraints (top-to-bottom):
/// scrollback (absorbs all free space), widget, input bar, info bar.
/// Scrollback and widget heights can be zero.
///
/// The scrollback section uses `Constraint::Min(0)` as the first (topmost)
/// constraint so it absorbs all space not used by widget, input, and info.
pub fn render(
    frame: &mut Frame,
    app: &App,
    gremlin_count: &str,
    project_name: &str,
    term_w: u16,
    term_h: u16,
) {
    let available = term_h.saturating_sub(2); // input bar + info bar
    let has_widget = app.widget.as_ref().is_some_and(|w| !w.is_empty());
    let widget_h = if has_widget {
        app.widget
            .as_ref()
            .map_or(0, |w| w.height(term_w, available))
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0), // scrollback — absorbs all free space
            Constraint::Length(widget_h),
            Constraint::Length(1), // input bar
            Constraint::Length(1), // info bar
        ])
        .split(frame.area());

    // Render scrollback only in the area above widget+input+info.
    // The Min(0) constraint gives us the full free space; we render
    // the tail of scrollback_lines into it.
    if chunks[0].height > 0 && !app.scrollback_lines.is_empty() {
        render_scrollback(frame, chunks[0], app);
    }
    if has_widget {
        if let Some(widget) = &app.widget {
            widget.render(frame, chunks[1]);
        }
    }
    render_input_bar(frame, chunks[2], app);
    render_info_bar(frame, chunks[3], app, gremlin_count, project_name);
}

fn render_scrollback(frame: &mut Frame, area: Rect, app: &App) {
    // Compute wrapped height of all scrollback lines.
    let area_w = area.width;
    let scrollback_h = app.scrollback_height(area_w);

    // Clamp to available height, then build a sub-rect anchored to the bottom.
    let render_h = scrollback_h.min(area.height);
    let render_area = Rect {
        x: area.x,
        y: area.bottom().saturating_sub(render_h),
        width: area.width,
        height: render_h,
    };

    // Compute how many lines to skip so the tail is visible when content
    // exceeds the available area. When overflow falls inside a wrapped
    // line, also compute the char prefix to trim so the first rendered
    // line starts at the correct wrapped row.
    let (skip_lines, prefix_trim_chars) = if scrollback_h > area.height {
        let wrap_w = (area_w as usize).max(1);
        let overflow_rows = scrollback_h - area.height;
        let mut consumed: u16 = 0;
        let mut skip: usize = 0;
        let mut trim: usize = 0;
        for (line, _) in &app.scrollback_lines {
            let chars = line.chars().count();
            let rows = if chars == 0 {
                1
            } else {
                chars.div_ceil(wrap_w)
            } as u16;
            if consumed + rows > overflow_rows {
                // Part of this line extends above the viewport.
                trim = (overflow_rows - consumed) as usize * wrap_w;
                break;
            }
            consumed += rows;
            skip += 1;
        }
        (skip, trim)
    } else {
        (0, 0)
    };

    let lines: Vec<Line> = app
        .scrollback_lines
        .iter()
        .skip(skip_lines)
        .enumerate()
        .map(|(i, (s, style))| {
            let text = if i == 0 && prefix_trim_chars > 0 {
                s.chars().skip(prefix_trim_chars).collect::<String>()
            } else {
                s.clone()
            };
            Line::from(Span::styled(text, *style))
        })
        .collect();

    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, render_area);
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
