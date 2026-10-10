use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::tui::app::App;

/// Render the TUI using the LiveArea region model.
///
/// Content is laid out bottom-up from the input bar:
///   info bar → input bar → response → tools → reasoning → prompt →
///   transcript.
///
/// Ephemeral regions (reasoning, tools) are bounded to 5 lines each
/// and are rendered between the prompt and response. They are excluded
/// from the scrollable line count so they never reach terminal
/// scrollback.
///
/// Returns `(scrollable_lines, total_lines)` where `scrollable_lines`
/// is the count of lines that may be committed to scrollback
/// (transcript + prompt + response) and `total_lines` is all rendered
/// lines including ephemeral regions.
pub fn render_livearea(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    gremlin_count: &str,
    project_name: &str,
) -> (u16, u16) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1), // input bar
            Constraint::Length(1), // info bar
        ])
        .split(area);

    let content_area = chunks[0];
    render_input_bar(buf, chunks[1], app);
    render_info_bar(buf, chunks[2], app, gremlin_count, project_name);

    if content_area.height == 0 {
        return (0, 0);
    }

    log::debug!(
        "render transcript_lines={} prompt={:?} reasoning={} tools={} response_len={} content_h={} active={}",
        app.transcript_lines.len(),
        app.current_prompt.as_deref(),
        app.reasoning.len(),
        app.tools.len(),
        app.response_text.len(),
        content_area.height,
        app.active_request,
    );
    if !app.response_text.is_empty() {
        log::debug!("render response_text={:?}", app.response_text);
    }

    // Styles.
    let prompt_marker = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let prompt_text = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let reason_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);
    let tool_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    // ── Build content lines top-to-bottom ─────────────────────
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut scrollable: u16 = 0;

    // 1. Transcript (visible portion).
    let tx_start = app.transcript_committed;
    for line in &app.transcript_lines[tx_start..] {
        lines.push(line.clone());
        scrollable += 1;
    }

    // 2. Current prompt.
    if let Some(ref prompt) = app.current_prompt {
        if !lines.is_empty() {
            lines.push(Line::from(""));
            scrollable += 1;
        }
        let prompt_line = Line::from(vec![
            Span::styled("> ", prompt_marker),
            Span::styled(prompt.clone(), prompt_text),
        ]);
        lines.push(prompt_line);
        scrollable += 1;
    }

    // 3. Reasoning (ephemeral, bounded last 5).
    if !app.reasoning.is_empty() || !app.reasoning_partial.is_empty() {
        lines.push(Line::from(""));
        for line in app.reasoning.iter().rev().take(5).rev() {
            lines.push(line.clone());
        }
        if !app.reasoning_partial.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", app.reasoning_partial),
                reason_style,
            )));
        }
    }

    // 4. Tools (ephemeral, bounded last 5).
    if !app.tools.is_empty() {
        lines.push(Line::from(""));
        for name in app.tools.iter().rev().take(5).rev() {
            lines.push(Line::from(Span::styled(
                format!("  {name}"),
                tool_style,
            )));
        }
    }

    // 5. Response text (scrollable).
    if !app.response_text.is_empty() {
        lines.push(Line::from(""));
        for line_str in app.response_text.lines() {
            lines.push(Line::from(Span::styled(
                line_str.to_string(),
                Style::default(),
            )));
            scrollable += 1;
        }
        // Preserve trailing newline (undisplayed partial line).
        if app.response_text.ends_with('\n') {
            // no-op — the final newline doesn't produce a line.
        }
    }

    // ── Render tail-pinned ────────────────────────────────────
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let line_count = paragraph.line_count(content_area.width);
    let scroll = line_count
        .saturating_sub(content_area.height as usize)
        .min(u16::MAX as usize) as u16;
    paragraph.scroll((scroll, 0)).render(content_area, buf);

    (scrollable, line_count.min(u16::MAX as usize) as u16)
}

fn render_info_bar(
    buf: &mut Buffer,
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
    paragraph.render(area, buf);
}

fn render_input_bar(buf: &mut Buffer, area: Rect, app: &App) {
    let prompt = Span::styled("> ", Style::default().fg(Color::Cyan));

    // Block cursor rendered as an inverted space *after* the input text,
    // showing where the next character will be inserted.
    let text = Line::from(vec![
        prompt,
        Span::raw(app.input.as_str()),
        Span::styled(" ", Style::default().bg(Color::White)),
    ]);

    let paragraph = Paragraph::new(text);
    paragraph.render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::screen::{LiveArea, TestBackend};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn dump_buffer(buf: &Buffer) -> String {
        let area = buf.area;
        let mut rows: Vec<String> = Vec::new();
        for y in 0..area.height {
            let row: String = (0..area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>();
            rows.push(row.trim_end().to_string());
        }
        while rows.last().is_some_and(|r| r.is_empty()) {
            rows.pop();
        }
        rows.join("\n")
    }

    #[test]
    fn chat_response_visible_after_done() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let backend = TestBackend::new(cols, rows);
        let mut live_area = LiveArea::with_backend(backend, cols, rows);
        let area = Rect::new(0, 0, cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        app.push_transcript_line("gremlins tui — type /help for commands, Ctrl+C to exit".to_string());
        app.push_transcript_line("[run started] chat-0000 (chat) stage=starting".to_string());
        app.submit_prompt("hi".to_string());
        app.push_reasoning_text("thinking...");
        app.flush_reasoning();
        app.push_reasoning_text("The user just said hi.");
        app.flush_reasoning();
        app.push_tool("Done");
        app.replace_response("Greeted the user; no task was requested yet.");
        app.current_response = "Greeted the user; no task was requested yet.".to_string();
        app.push_transcript_line("[run completed] chat-0000 (exit 0)".to_string());

        live_area
            .draw(|buf| render_livearea(buf, area, &app, "0", "gremlins"))
            .unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("Greeted the user"),
            "Response text should be visible.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("> hi"),
            "Prompt should be visible.\nBuffer:\n{visible}"
        );
    }

    #[test]
    fn multi_frame_streaming_response_visible_after_done() {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let backend = TestBackend::new(cols, rows);
        let mut live_area = LiveArea::with_backend(backend, cols, rows);
        let area = Rect::new(0, 0, cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();

        let frame = |live_area: &mut LiveArea<TestBackend>, app: &App| {
            live_area
                .draw(|buf| render_livearea(buf, area, app, "0", "gremlins"))
                .unwrap();
        };

        app.push_transcript_line("gremlins tui — type /help for commands, Ctrl+C to exit".to_string());
        app.push_transcript_line("[run started] chat-0000 (chat) stage=starting".to_string());
        frame(&mut live_area, &app);

        app.submit_prompt("hi".to_string());
        app.push_reasoning_text("thinking...");
        app.flush_reasoning();
        frame(&mut live_area, &app);

        app.push_reasoning_text("The user just said hi.");
        app.flush_reasoning();
        frame(&mut live_area, &app);

        app.push_reasoning_text("I should respond briefly.");
        app.flush_reasoning();
        app.push_tool("Done");
        frame(&mut live_area, &app);

        app.replace_response("Greeted the user; no task was requested yet.");
        app.current_response = "Greeted the user; no task was requested yet.".to_string();
        frame(&mut live_area, &app);

        app.push_transcript_line("[run completed] chat-0000 (exit 0)".to_string());
        frame(&mut live_area, &app);

        let visible = dump_buffer(live_area.backend().buffer());
        assert!(
            visible.contains("Greeted the user"),
            "Response should be visible after Done.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains("> hi"),
            "Prompt should be visible.\nBuffer:\n{visible}"
        );
    }
}
