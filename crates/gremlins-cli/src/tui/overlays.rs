use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table},
    Frame,
};

use crate::tui::app::{App, Overlay};

const WATCH_COLUMNS: [&str; 4] = ["ID", "STATUS", "STAGE", "PROJECT"];

/// Render the gremlins watch table in the upper portion of the transcript.
pub fn render_watch_overlay(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }

    // Header + rows + summary footer share the allocated area.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    render_watch_title(frame, chunks[0], app);
    render_watch_table(frame, chunks[1], app);
    render_watch_footer(frame, chunks[2], app);
}

fn render_watch_title(frame: &mut Frame, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(
            "gremlins watch",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} tracked", app.active_runs.len()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  Esc to dismiss"),
    ]);
    frame.render_widget(Paragraph::new(title), area);
}

fn render_watch_table(frame: &mut Frame, area: Rect, app: &App) {
    if app.active_runs.is_empty() {
        let paragraph = Paragraph::new(Span::styled(
            "(no gremlins)",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(paragraph, area);
        return;
    }

    let rows = build_watch_rows(app);
    let header = Row::new(
        WATCH_COLUMNS
            .iter()
            .map(|h| Cell::from(Span::styled(*h, Style::default().fg(Color::Yellow)))),
    )
    .style(Style::default().fg(Color::Yellow));

    let widths = [
        Constraint::Length(20),
        Constraint::Length(10),
        Constraint::Length(16),
        Constraint::Min(16),
    ];

    let table = Table::new(rows, widths).header(header).column_spacing(2);
    frame.render_widget(table, area);
}

fn build_watch_rows(app: &App) -> Vec<Row<'static>> {
    let mut runs: Vec<_> = app.active_runs.iter().collect();
    runs.sort_by(|a, b| a.0.cmp(b.0));

    runs.into_iter()
        .map(|(id, run)| {
            Row::new(vec![
                Cell::from(Span::styled(id.clone(), Style::default().fg(Color::Cyan))),
                Cell::from(Span::styled(run.status.clone(), status_style(&run.status))),
                Cell::from(Span::styled(run.stage.clone(), Style::default())),
                Cell::from(Span::styled(run.project.clone(), Style::default())),
            ])
        })
        .collect()
}

fn status_style(status: &str) -> Style {
    match status {
        "running" => Style::default().fg(Color::Green),
        "failed" => Style::default().fg(Color::Red),
        "stopped" => Style::default().fg(Color::Yellow),
        "done" => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

fn render_watch_footer(frame: &mut Frame, area: Rect, app: &App) {
    let counts = status_counts(app);
    let text = format!(
        "{} total · {} running · {} done · {} failed · {} stopped",
        counts.get("total").copied().unwrap_or(0),
        counts.get("running").copied().unwrap_or(0),
        counts.get("done").copied().unwrap_or(0),
        counts.get("failed").copied().unwrap_or(0),
        counts.get("stopped").copied().unwrap_or(0),
    );
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

/// Return the trailing window of `lines` that fits in `height` rows.
fn tail_text(lines: &[String], height: u16, empty: &str) -> String {
    if lines.is_empty() {
        return empty.to_string();
    }

    let start = lines.len().saturating_sub(height as usize);
    lines[start..].join("\n")
}

fn status_counts(app: &App) -> HashMap<&'static str, usize> {
    let mut counts = HashMap::new();
    counts.insert("total", app.active_runs.len());
    for run in app.active_runs.values() {
        *counts
            .entry(match run.status.as_str() {
                "running" => "running",
                "done" => "done",
                "failed" => "failed",
                "stopped" => "stopped",
                _ => "other",
            })
            .or_insert(0) += 1;
    }
    counts
}

/// Render the single-gremlin log viewer (full transcript occlusion).
pub fn render_watch_single_overlay(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }

    let (id, log_lines) = match &app.overlay {
        Some(Overlay::WatchSingle { id, log_lines }) => (id, log_lines),
        _ => return,
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    let stage = app
        .active_runs
        .get(id)
        .map(|r| r.stage.as_str())
        .unwrap_or("");

    let title = Line::from(vec![
        Span::styled(
            format!("gremlin {id}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("stage: {stage}"),
            Style::default().fg(Color::Magenta),
        ),
        Span::raw("  Esc to dismiss"),
    ]);
    frame.render_widget(Paragraph::new(title), chunks[0]);

    // Render only the trailing `height` lines so the view stays anchored to
    // the newest output without allocating a join of the full buffer.
    let body = tail_text(log_lines, chunks[1].height, "(no log lines yet)");
    frame.render_widget(Paragraph::new(body), chunks[1]);
}

/// Render the interactive debug session (full transcript occlusion).
pub fn render_debug_overlay(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }

    let (id, input, history) = match &app.overlay {
        Some(Overlay::Debug { id, input, history }) => (id, input, history),
        _ => return,
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let title = Line::from(vec![
        Span::styled(
            format!("debug {id}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  Esc to dismiss · Ctrl+D or /exit to leave"),
    ]);
    frame.render_widget(Paragraph::new(title), chunks[0]);

    // Keep the scrollback anchored to the latest command output without
    // joining the entire history buffer on every frame.
    let body = tail_text(
        history,
        chunks[1].height,
        "(debug session not yet available)",
    );
    frame.render_widget(Paragraph::new(body), chunks[1]);

    let prompt = Line::from(vec![
        Span::styled("debug> ", Style::default().fg(Color::Cyan)),
        Span::raw(input.as_str()),
        Span::styled(" ", Style::default().bg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(prompt), chunks[2]);
}
