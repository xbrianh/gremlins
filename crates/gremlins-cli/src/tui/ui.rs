use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    text::Line,
};

use crate::tui::app::App;
use crate::tui::widgets::{LiveAreaWidget, RenderContext};

/// Render the TUI using the LiveArea region model.
///
/// The five LiveArea widgets are laid out top-to-bottom by their height
/// constraints:
///
///   transcript (fills) → reasoning → tools → input bar → info bar
///
/// All scrollable content lives in the transcript widget's FIFO. When its
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
    let App {
        transcript,
        reasoning,
        tools,
        input_bar,
        info_bar,
        input,
        following_log,
        ..
    } = app;

    let ctx = RenderContext {
        input,
        following_log: following_log.as_deref(),
        gremlin_count,
        project_name,
    };

    let width = area.width;

    // Reserve the two fixed bars first, exactly as the original two-phase
    // split did. The content widgets (transcript, reasoning, tools) are laid
    // out only within the remaining rect, so their `Length` constraints can
    // never compress or drop the bars.
    let bar_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let content_area = bar_chunks[0];
    let input_bar_area = bar_chunks[1];
    let info_bar_area = bar_chunks[2];

    // Render the bars unconditionally.
    input_bar.render(input_bar_area, buf, &ctx);
    info_bar.render(info_bar_area, buf, &ctx);

    if content_area.height == 0 {
        return Vec::new();
    }

    // Ordered top-to-bottom within the content area.
    let mut widgets: Vec<&mut dyn LiveAreaWidget> = vec![transcript, reasoning, tools];

    let constraints: Vec<Constraint> = widgets.iter().map(|w| w.height_constraint(width)).collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(content_area);

    let mut overflow = Vec::new();
    for (widget, chunk) in widgets.iter_mut().zip(chunks.iter()) {
        overflow.extend(widget.render(*chunk, buf, &ctx));
    }
    overflow
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

    #[test]
    fn short_viewport_reserves_bars_and_skips_overflow() {
        let cols: u16 = 80;
        let rows: u16 = 2;
        let backend = TestBackend::new(cols, rows);
        let mut live_area = LiveArea::with_backend(backend, cols, rows);
        let area = Rect::new(0, 0, cols, rows);

        let mut app = App::new();
        app.project_name = "gremlins".to_string();
        app.push_transcript_line("some transcript line".to_string());
        app.submit_prompt("hi".to_string());
        app.replace_response("a response");

        let overflow = render_livearea(live_area.curr_mut(), area, &mut app, "0", "gremlins");
        // With zero content rows, nothing can scroll into the terminal.
        assert!(overflow.is_empty());

        live_area.draw(|_| {}).unwrap();

        let visible = dump_buffer(live_area.backend().buffer());
        // The two fixed bars must still be rendered even when the content
        // area has zero height.
        assert!(
            visible.contains("gremlins tui"),
            "Info bar should be visible.\nBuffer:\n{visible}"
        );
        assert!(
            visible.contains(">"),
            "Input bar prompt should be visible.\nBuffer:\n{visible}"
        );
    }
}
