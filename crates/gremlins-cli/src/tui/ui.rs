use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::tui::app::App;
use crate::tui::buffer::wrapped_height;

/// Render the TUI using the LiveArea region model.
///
/// Content is laid out bottom-up from the input bar:
///   info bar → input bar → tools → reasoning → scrollable (transcript +
///   prompt + response).
///
/// All scrollable content lives in the FIFO ([`ScrollableBuffer`]). When its
/// wrapped height exceeds available rows, the FIFO head is extracted and
/// returned as overflow lines to be committed to terminal scrollback via
/// [`LiveArea::commit_scrollback`].
///
/// Ephemeral regions (reasoning, tools) are bounded to 5 visible rows each
/// and never reach scrollback.
///
/// Returns overflow lines (empty if nothing overflowed). The caller must
/// commit them via `LiveArea::commit_scrollback`.
pub fn render_livearea(
    buf: &mut Buffer,
    area: Rect,
    app: &mut App,
    gremlin_count: &str,
    project_name: &str,
) -> Vec<Line<'static>> {
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
        return Vec::new();
    }

    let width = content_area.width;

    log::debug!(
        "render scrollable_vis={} prompt={:?} reasoning={} tools={} response_len={} content_h={} active={}",
        app.scrollable.visible_lines().len(),
        app.current_prompt.as_deref(),
        app.reasoning.visible_lines(width).len(),
        app.tools.visible_lines(width).len(),
        app.response_text.len(),
        content_area.height,
        app.active_request,
    );

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

    // ── Current-turn scrollable lines (prompt + response) ─────
    let prompt_lines: Vec<Line<'static>> = match &app.current_prompt {
        Some(prompt) => vec![Line::from(vec![
            Span::styled("> ", prompt_marker),
            Span::styled(prompt.clone(), prompt_text),
        ])],
        None => Vec::new(),
    };
    let prompt_height = wrapped_height(&prompt_lines, width) as u16;

    let response_lines: Vec<Line<'static>> = if app.response_text.is_empty() {
        Vec::new()
    } else {
        app.response_text
            .lines()
            .map(|s| Line::from(Span::styled(s.to_string(), Style::default())))
            .collect()
    };
    let response_height = wrapped_height(&response_lines, width) as u16;

    // ── Ephemeral lines (reasoning + partial, tools), bounded ──
    let reasoning_extra: Vec<Line<'static>> = if app.reasoning_partial.is_empty() {
        Vec::new()
    } else {
        vec![Line::from(Span::styled(
            format!("  {}", app.reasoning_partial),
            reason_style,
        ))]
    };
    let reasoning_lines = app
        .reasoning
        .visible_lines_with_extra(&reasoning_extra, width);
    let reasoning_height = wrapped_height(&reasoning_lines, width) as u16;

    let tools_lines = app.tools.visible_lines(width);
    let tools_height = wrapped_height(&tools_lines, width) as u16;

    // ── Scrollable budget ─────────────────────────────────────
    let ephemeral = reasoning_height.saturating_add(tools_height);
    let available = content_area.height.saturating_sub(ephemeral);

    // Blank separators.
    let sep_tp_est = if !app.scrollable.is_empty() && !prompt_lines.is_empty() {
        1u16
    } else {
        0
    };
    let sep_pr = if !prompt_lines.is_empty() && !response_lines.is_empty() {
        1u16
    } else {
        0
    };

    // Fixed (non-transcript) scrollable height.
    let fixed = prompt_height
        .saturating_add(response_height)
        .saturating_add(sep_tp_est)
        .saturating_add(sep_pr);

    // ── Overflow: split the FIFO head ─────────────────────────
    let transcript_available = available.saturating_sub(fixed);
    let (split_idx, _overflow_rows) = app.scrollable.overflow_split(transcript_available, width);

    let overflow_lines: Vec<Line<'static>> = if split_idx > 0 {
        let head: Vec<Line<'static>> = app.scrollable.visible_lines()[..split_idx].to_vec();
        app.scrollable.drain_head(split_idx);
        head
    } else {
        Vec::new()
    };

    // ── Build combined scrollable lines (post-drain) ──────────
    let mut combined: Vec<Line<'static>> = app.scrollable.visible_lines().to_vec();
    let sep_tp = if !combined.is_empty() && !prompt_lines.is_empty() {
        1u16
    } else {
        0
    };
    if sep_tp > 0 {
        combined.push(Line::from(""));
    }
    combined.extend(prompt_lines.iter().cloned());
    if sep_pr > 0 {
        combined.push(Line::from(""));
    }
    combined.extend(response_lines.iter().cloned());

    // ── Allocate regions bottom-up ────────────────────────────
    let content_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(available),        // scrollable
            Constraint::Length(reasoning_height), // reasoning
            Constraint::Length(tools_height),     // tools
        ])
        .split(content_area);

    // ── Render each region ────────────────────────────────────
    if !combined.is_empty() {
        let paragraph = Paragraph::new(combined).wrap(Wrap { trim: false });
        paragraph.render(content_chunks[0], buf);
    }

    if !reasoning_lines.is_empty() {
        let paragraph = Paragraph::new(reasoning_lines).wrap(Wrap { trim: false });
        paragraph.render(content_chunks[1], buf);
    }

    if !tools_lines.is_empty() {
        let paragraph = Paragraph::new(tools_lines).wrap(Wrap { trim: false });
        paragraph.render(content_chunks[2], buf);
    }

    overflow_lines
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

        app.push_transcript_line(
            "gremlins tui — type /help for commands, Ctrl+C to exit".to_string(),
        );
        app.submit_prompt("hi".to_string());
        app.push_reasoning_text("thinking...");
        app.flush_reasoning();
        app.push_reasoning_text("The user just said hi.");
        app.flush_reasoning();
        app.push_tool("Done");
        app.replace_response("Greeted the user; no task was requested yet.");

        let overflow = render_livearea(live_area.curr_mut(), area, &mut app, "0", "gremlins");
        // In a 24-row terminal, this content should not overflow.
        assert!(overflow.is_empty());

        live_area.draw(|_| {}).unwrap();

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

        let frame = |live_area: &mut LiveArea<TestBackend>, app: &mut App| {
            let overflow = render_livearea(live_area.curr_mut(), area, app, "0", "gremlins");
            if !overflow.is_empty() {
                live_area.commit_scrollback(&overflow, cols).unwrap();
            }
            live_area.draw(|_| {}).unwrap();
        };

        app.push_transcript_line(
            "gremlins tui — type /help for commands, Ctrl+C to exit".to_string(),
        );

        app.submit_prompt("hi".to_string());
        app.push_reasoning_text("thinking...");
        app.flush_reasoning();
        frame(&mut live_area, &mut app);

        app.push_reasoning_text("The user just said hi.");
        app.flush_reasoning();
        frame(&mut live_area, &mut app);

        app.push_reasoning_text("I should respond briefly.");
        app.flush_reasoning();
        app.push_tool("Done");
        frame(&mut live_area, &mut app);

        app.replace_response("Greeted the user; no task was requested yet.");
        frame(&mut live_area, &mut app);

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
