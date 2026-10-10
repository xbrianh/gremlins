use ratatui::{
    prelude::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

/// Number of stream lines to keep when freezing.
const FREEZE_STREAM_LINES: usize = 3;

/// Minimum rows allocated to the stream section when output is present.
const STREAM_MIN: u16 = 5;

// ── WidgetEvent ──────────────────────────────────────────────────────────

/// Events captured during streaming for statistics display.
#[derive(Debug, Clone)]
pub enum WidgetEvent {
    #[allow(dead_code)]
    ToolCall { name: String, output: String },
    TokenUsage {
        prompt: usize,
        completion: usize,
        cache_read: usize,
    },
}

// ── Widget trait ─────────────────────────────────────────────────────────

/// A widget in the transcript viewport.
///
/// Streaming methods default to no-ops so passive widgets don't need to
/// care about them. [`finish`] transitions an active widget into a passive
/// one, consuming the event log for statistics.
pub trait Widget {
    /// Height in rows at the given terminal width.
    fn height(&self, width: u16) -> u16;

    /// Render into `area`. The widget may use internal scrolling when
    /// content exceeds the allocated height.
    fn render(&self, frame: &mut Frame, area: Rect);

    /// Whether Ctrl+O can expand/collapse this widget.
    fn is_expandable(&self) -> bool;

    /// Whether this widget is currently expanded.
    fn is_expanded(&self) -> bool {
        false
    }

    /// Toggle expanded state. No-op for non-expandable widgets.
    fn toggle_expand(&mut self);

    /// Whether this widget is a takeover (occludes transcript, renders
    /// above input/info bars). Future use; defaults to false.
    #[allow(dead_code)]
    fn is_takeover(&self) -> bool {
        false
    }

    /// Whether this widget should absorb all remaining transcript space
    /// rather than using its natural [`height`](Widget::height). Only
    /// [`ActivePromptWidget`] is greedy. Defaults to false.
    fn is_greedy(&self) -> bool {
        false
    }

    // ── Streaming API (default no-ops) ──────────────────────────────

    fn push_stream_text(&mut self, _text: &str, _style: Style) {}
    fn push_stream_line(&mut self, _line: Line<'static>) {}
    fn push_response_text(&mut self, _text: &str) {}
    fn push_tool_result(&mut self, _name: &str, _output: &str) {}
    fn replace_response(&mut self, _text: &str) {}
    fn flush_partial(&mut self) {}

    /// Transition from streaming to finished. Consumes internal state and
    /// returns a new passive widget built from the event log.
    ///
    /// Panics for terminal-state widgets (FinishedPromptWidget, SystemWidget).
    fn finish(&mut self, events: Vec<WidgetEvent>) -> Box<dyn Widget> {
        let _ = events;
        panic!("finish called on terminal widget");
    }

    /// Whether this is a SystemWidget (for auto-collapse targeting).
    #[allow(dead_code)]
    fn is_system(&self) -> bool {
        false
    }

    /// Auto-collapse this widget. Only SystemWidget responds.
    fn auto_collapse(&mut self) {}
}

// ── ActivePromptWidget ───────────────────────────────────────────────────

/// The in-flight chat turn. Always fully rendered (no collapsed state).
/// Not expandable — it's transient. Always the bottom-most widget when
/// present.
pub struct ActivePromptWidget {
    prompt: String,
    stream_lines: Vec<Line<'static>>,
    stream_partial: String,
    response_lines: Vec<Line<'static>>,
    response_partial: String,
    events: Vec<WidgetEvent>,
}

impl ActivePromptWidget {
    pub fn new(prompt: String) -> Self {
        Self {
            prompt,
            stream_lines: Vec::new(),
            stream_partial: String::new(),
            response_lines: Vec::new(),
            response_partial: String::new(),
            events: Vec::new(),
        }
    }
}

impl Widget for ActivePromptWidget {
    fn height(&self, width: u16) -> u16 {
        let prompt_h = 1u16;
        let stream_h = wrapped_rows(&self.stream_lines, &self.stream_partial, width);
        let response_h = wrapped_rows(&self.response_lines, &self.response_partial, width);
        let sep_h = if stream_h > 0 && response_h > 0 { 1 } else { 0 };
        prompt_h + stream_h + sep_h + response_h
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }

        let width = area.width;
        let stream_nat = wrapped_rows(&self.stream_lines, &self.stream_partial, width);
        let output_nat = wrapped_rows(&self.response_lines, &self.response_partial, width);
        let sep_h = if stream_nat > 0 && output_nat > 0 {
            1
        } else {
            0
        };

        // Reserve 1 row for the prompt at the top.
        let prompt_h = 1u16.min(area.height);
        let available = area.height.saturating_sub(prompt_h);

        // Allocate rows below the prompt.
        //
        // Output gets priority up to its natural height, but when stream
        // content is present it keeps a minimum floor of STREAM_MIN rows
        // (plus the separator row when both sections are shown). With no
        // output, the stream fills all remaining space.
        let stream_floor = if stream_nat > 0 { STREAM_MIN } else { 0 };
        let (stream_h, output_h) = if output_nat > 0 {
            let reserve = (stream_floor + sep_h).min(available);
            let output_h = output_nat.min(available.saturating_sub(reserve));
            let stream_h = available.saturating_sub(output_h).saturating_sub(sep_h);
            (stream_h, output_h)
        } else {
            (available, 0)
        };

        let mut y = area.y;

        // Prompt (top).
        if prompt_h > 0 {
            let prompt = Line::from(vec![
                Span::styled(
                    "● ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    self.prompt.as_str(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            let p = Paragraph::new(prompt);
            frame.render_widget(
                p,
                Rect {
                    x: area.x,
                    y,
                    width,
                    height: 1,
                },
            );
            y += 1;
        }

        // Stream section.
        if stream_h > 0 {
            let mut lines = self.stream_lines.clone();
            if !self.stream_partial.is_empty() {
                let style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                lines.push(Line::from(Span::styled(
                    format!("  {}", self.stream_partial),
                    style,
                )));
            }
            let scroll = stream_nat.saturating_sub(stream_h);
            let p = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(
                p,
                Rect {
                    x: area.x,
                    y,
                    width,
                    height: stream_h,
                },
            );
            y += stream_h;
        }

        // Separator.
        if sep_h > 0 && stream_h > 0 && output_h > 0 && y < area.bottom() {
            let sep = Paragraph::new(Line::from(Span::styled(
                "─".repeat(width as usize),
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(
                sep,
                Rect {
                    x: area.x,
                    y,
                    width,
                    height: 1,
                },
            );
            y += 1;
        }

        // Response section (bottom).
        if output_h > 0 {
            let mut lines = self.response_lines.clone();
            if !self.response_partial.is_empty() {
                lines.push(Line::from(Span::styled(
                    self.response_partial.as_str(),
                    Style::default(),
                )));
            }
            let scroll = output_nat.saturating_sub(output_h);
            let p = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(
                p,
                Rect {
                    x: area.x,
                    y,
                    width,
                    height: output_h,
                },
            );
        }
    }

    fn is_expandable(&self) -> bool {
        false
    }

    fn is_greedy(&self) -> bool {
        true
    }

    fn toggle_expand(&mut self) {
        // No-op: transient widget
    }

    fn push_stream_text(&mut self, text: &str, style: Style) {
        self.stream_partial.push_str(text);
        while let Some(newline_pos) = self.stream_partial.find('\n') {
            let line: String = self.stream_partial[..newline_pos].into();
            self.stream_partial = self.stream_partial[newline_pos + 1..].into();
            self.stream_lines
                .push(Line::from(Span::styled(format!("  {line}"), style)));
        }
    }

    fn push_stream_line(&mut self, line: Line<'static>) {
        self.flush_stream_partial();
        self.stream_lines.push(line);
    }

    fn push_response_text(&mut self, text: &str) {
        self.response_partial.push_str(text);
        while let Some(newline_pos) = self.response_partial.find('\n') {
            let line: String = self.response_partial[..newline_pos].into();
            self.response_partial = self.response_partial[newline_pos + 1..].into();
            self.response_lines
                .push(Line::from(Span::styled(line, Style::default())));
        }
    }

    fn push_tool_result(&mut self, name: &str, output: &str) {
        self.flush_stream_partial();
        self.events.push(WidgetEvent::ToolCall {
            name: name.to_string(),
            output: output.to_string(),
        });
        let tool_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        if output.is_empty() {
            self.stream_lines.push(Line::from(Span::styled(
                format!("  {name}: (empty)"),
                tool_style,
            )));
        } else {
            for (i, line) in output.lines().enumerate() {
                if i == 0 {
                    self.stream_lines.push(Line::from(Span::styled(
                        format!("  {name}: {line}"),
                        tool_style,
                    )));
                } else {
                    self.stream_lines.push(Line::from(Span::styled(
                        format!("        {line}"),
                        tool_style,
                    )));
                }
            }
        }
    }

    fn replace_response(&mut self, text: &str) {
        self.response_lines.clear();
        self.response_partial.clear();
        self.push_response_text(text);
    }

    fn flush_partial(&mut self) {
        if !self.response_partial.is_empty() {
            let remaining = std::mem::take(&mut self.response_partial);
            self.response_lines
                .push(Line::from(Span::styled(remaining, Style::default())));
        }
        self.flush_stream_partial();
    }

    fn finish(&mut self, events: Vec<WidgetEvent>) -> Box<dyn Widget> {
        self.flush_partial();

        let response_lines: Vec<String> = std::mem::take(&mut self.response_lines)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        let take_count = self.stream_lines.len().min(FREEZE_STREAM_LINES);
        let start = self.stream_lines.len().saturating_sub(take_count);
        let stream_tail: Vec<String> = self.stream_lines[start..]
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        let mut all_events = std::mem::take(&mut self.events);
        all_events.extend(events);

        Box::new(FinishedPromptWidget {
            prompt: std::mem::take(&mut self.prompt),
            response_lines,
            stream_tail,
            events: all_events,
            expanded: false,
        })
    }
}

impl ActivePromptWidget {
    fn flush_stream_partial(&mut self) {
        if !self.stream_partial.is_empty() {
            let remaining = std::mem::take(&mut self.stream_partial);
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            self.stream_lines
                .push(Line::from(Span::styled(format!("  {remaining}"), style)));
        }
    }
}

// ── FinishedPromptWidget ─────────────────────────────────────────────────

/// A completed chat turn. Expandable.
///
/// **Collapsed**: prompt + full response (response is always visible).
/// **Expanded**: prompt + summary + stream tail + separator + response.
pub struct FinishedPromptWidget {
    prompt: String,
    response_lines: Vec<String>,
    stream_tail: Vec<String>,
    events: Vec<WidgetEvent>,
    expanded: bool,
}

impl Widget for FinishedPromptWidget {
    fn height(&self, width: u16) -> u16 {
        let prompt_h = 1u16;
        let response_h = wrapped_rows_str(&self.response_lines, width);
        if self.expanded {
            let summary_h = 1u16;
            let stream_h = wrapped_rows_str(&self.stream_tail, width);
            let sep_h = if stream_h > 0 { 1 } else { 0 };
            prompt_h + summary_h + stream_h + sep_h + response_h
        } else {
            prompt_h + response_h
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let width = area.width;

        let mut y = area.bottom();

        // Response (bottom)
        let response_content = wrapped_rows_str(&self.response_lines, width);
        if response_content > 0 && y > area.y {
            let h = response_content.min(y - area.y);
            y = y.saturating_sub(h);
            let total = response_content as usize;
            let scroll = total.saturating_sub(h as usize) as u16;
            let text: Vec<Line> = self
                .response_lines
                .iter()
                .map(|s| Line::from(Span::styled(s.as_str(), Style::default())))
                .collect();
            let p = Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(
                p,
                Rect {
                    x: area.x,
                    y,
                    width,
                    height: h,
                },
            );
        }

        if self.expanded {
            // Separator
            if !self.stream_tail.is_empty() && y > area.y {
                y = y.saturating_sub(1);
                let sep = Paragraph::new(Line::from(Span::styled(
                    "─".repeat(width as usize),
                    Style::default().fg(Color::DarkGray),
                )));
                frame.render_widget(
                    sep,
                    Rect {
                        x: area.x,
                        y,
                        width,
                        height: 1,
                    },
                );
            }

            // Stream tail
            let stream_content = wrapped_rows_str(&self.stream_tail, width);
            if stream_content > 0 && y > area.y {
                let h = stream_content.min(y - area.y);
                y = y.saturating_sub(h);
                let total = stream_content as usize;
                let scroll = total.saturating_sub(h as usize) as u16;
                let stream_style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                let text: Vec<Line> = self
                    .stream_tail
                    .iter()
                    .map(|s| Line::from(Span::styled(s.as_str(), stream_style)))
                    .collect();
                let p = Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0));
                frame.render_widget(
                    p,
                    Rect {
                        x: area.x,
                        y,
                        width,
                        height: h,
                    },
                );
            }

            // Summary
            if y > area.y {
                y = y.saturating_sub(1);
                let summary = summary_line(&self.events);
                let p = Paragraph::new(Line::from(Span::styled(summary, Style::default())));
                frame.render_widget(
                    p,
                    Rect {
                        x: area.x,
                        y,
                        width,
                        height: 1,
                    },
                );
            }
        }

        // Prompt (top)
        if y > area.y {
            y = y.saturating_sub(1);
            let prompt = Line::from(vec![
                Span::styled(
                    "● ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    self.prompt.as_str(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            let p = Paragraph::new(prompt);
            frame.render_widget(
                p,
                Rect {
                    x: area.x,
                    y,
                    width,
                    height: 1,
                },
            );
        }
    }

    fn is_expandable(&self) -> bool {
        true
    }

    fn is_expanded(&self) -> bool {
        self.expanded
    }

    fn toggle_expand(&mut self) {
        self.expanded = !self.expanded;
    }
}

// ── SystemWidget ─────────────────────────────────────────────────────────

/// Banner, daemon events, command output, errors — anything that isn't a
/// chat turn. Expandable.
///
/// **Collapsed**: 1 row (first line, truncated to fit width).
/// **Expanded**: full wrapped text.
pub struct SystemWidget {
    lines: Vec<(String, Style)>,
    expanded: bool,
}

impl SystemWidget {
    pub fn new(lines: Vec<(String, Style)>, expanded: bool) -> Self {
        Self { lines, expanded }
    }
}

impl Widget for SystemWidget {
    fn height(&self, width: u16) -> u16 {
        if self.lines.is_empty() {
            return 0;
        }
        if self.expanded {
            wrapped_rows_styled(&self.lines, width)
        } else {
            1
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        if self.lines.is_empty() {
            return;
        }
        if self.expanded {
            let total = wrapped_rows_styled(&self.lines, area.width) as usize;
            let scroll = total.saturating_sub(area.height as usize) as u16;
            let text: Vec<Line> = self
                .lines
                .iter()
                .map(|(s, style)| Line::from(Span::styled(s.as_str(), *style)))
                .collect();
            let p = Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(p, area);
        } else {
            // Render first line, truncated to fit width.
            let (text, style) = &self.lines[0];
            let truncated: String = text.chars().take(area.width as usize).collect();
            let p = Paragraph::new(Line::from(Span::styled(truncated, *style)));
            frame.render_widget(
                p,
                Rect {
                    x: area.x,
                    y: area.bottom().saturating_sub(1),
                    width: area.width,
                    height: 1,
                },
            );
        }
    }

    fn is_expandable(&self) -> bool {
        true
    }

    fn is_expanded(&self) -> bool {
        self.expanded
    }

    fn toggle_expand(&mut self) {
        self.expanded = !self.expanded;
    }

    fn is_system(&self) -> bool {
        true
    }

    fn auto_collapse(&mut self) {
        self.expanded = false;
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Compute total wrapped rows for an iterator of string slices.
fn wrapped_rows_iter<'a>(items: impl Iterator<Item = &'a str>, width: u16) -> u16 {
    let wrap_width = (width as usize).max(1);
    let mut total: u16 = 0;
    for item in items {
        let chars = item.chars().count();
        let rows = if chars == 0 {
            1
        } else {
            chars.div_ceil(wrap_width)
        };
        total = total.saturating_add(rows as u16);
    }
    total
}

fn wrapped_rows(lines: &[Line], partial: &str, width: u16) -> u16 {
    let line_strs: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    if partial.is_empty() {
        wrapped_rows_iter(line_strs.iter().map(|s| s.as_str()), width)
    } else {
        let mut all: Vec<&str> = line_strs.iter().map(|s| s.as_str()).collect();
        all.push(partial);
        wrapped_rows_iter(all.into_iter(), width)
    }
}

fn wrapped_rows_str(lines: &[String], width: u16) -> u16 {
    wrapped_rows_iter(lines.iter().map(|s| s.as_str()), width)
}

fn wrapped_rows_styled(lines: &[(String, Style)], width: u16) -> u16 {
    wrapped_rows_iter(lines.iter().map(|(s, _)| s.as_str()), width)
}

// ── Statistics ───────────────────────────────────────────────────────────

fn tool_count(events: &[WidgetEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, WidgetEvent::ToolCall { .. }))
        .count()
}

fn token_count(events: &[WidgetEvent]) -> usize {
    events
        .iter()
        .filter_map(|e| match e {
            WidgetEvent::TokenUsage {
                prompt, completion, ..
            } => Some(prompt + completion),
            _ => None,
        })
        .sum()
}

fn cache_pct(events: &[WidgetEvent]) -> Option<f64> {
    let mut prompt_total = 0usize;
    let mut cache_total = 0usize;
    for e in events {
        if let WidgetEvent::TokenUsage {
            prompt, cache_read, ..
        } = e
        {
            prompt_total += prompt;
            cache_total += cache_read;
        }
    }
    if prompt_total == 0 {
        None
    } else {
        Some((cache_total as f64 / prompt_total as f64) * 100.0)
    }
}

fn summary_line(events: &[WidgetEvent]) -> String {
    let tools = tool_count(events);
    let tokens = token_count(events);
    let cache = cache_pct(events);

    let mut parts: Vec<String> = Vec::new();
    if tools > 0 {
        parts.push(format!("{} tool calls", tools));
    }
    parts.push(format!("{} tokens", tokens));
    if let Some(pct) = cache {
        parts.push(format!("{:.0}% cached", pct));
    }
    parts.join(" · ")
}
