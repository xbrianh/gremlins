use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

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
    /// Produce this widget's styled lines for the given terminal width.
    ///
    /// The caller assembles all widgets' lines into a single `Paragraph`
    /// and lets ratatui handle wrapping.
    fn lines(&self, width: u16) -> Vec<Line<'static>>;

    /// Whether Ctrl+O can expand/collapse this widget.
    fn is_expandable(&self) -> bool;

    /// Whether this widget is currently expanded.
    fn is_expanded(&self) -> bool {
        false
    }

    /// Toggle expanded state. No-op for non-expandable widgets.
    fn toggle_expand(&mut self);

    // ── Streaming API (default no-ops) ──────────────────────────────

    fn push_stream_text(&mut self, _text: &str, _style: Style) {}
    fn push_stream_line(&mut self, _line: Line<'static>) {}
    fn push_response_text(&mut self, _text: &str) {}
    fn push_tool_result(&mut self, _name: &str, _output: &str) {}
    fn replace_response(&mut self, _text: &str) {}
    fn flush_partial(&mut self) {}

    /// Transition from streaming to finished. Mutates in place.
    ///
    /// Panics for terminal-state widgets (SystemWidget).
    fn finish(&mut self, events: Vec<WidgetEvent>) {
        let _ = events;
        panic!("finish called on terminal widget");
    }

    /// Whether this is a SystemWidget (for auto-collapse targeting).
    #[allow(dead_code)]
    fn is_system(&self) -> bool {
        false
    }

    /// Whether this widget is an in-flight (streaming) transcript turn.
    /// Used to keep the active turn bottom-most when inserting system
    /// messages. Finished transcript widgets are not active.
    fn is_active(&self) -> bool {
        false
    }

    /// Auto-collapse this widget. Only SystemWidget responds.
    fn auto_collapse(&mut self) {}
}

// ── TranscriptWidget ─────────────────────────────────────────────────────

/// A chat turn — either in-flight (streaming) or finished.
///
/// Layout, top-to-bottom, with one blank line between every element:
/// 1. Prompt line.
/// 2. Reasoning (last 2–3 visible lines, dim italic).
/// 3. Tool list (non-read/edit tools, each with indented output).
/// 4. Response text.
/// 5. Stats line (finished only).
///
/// Not expandable. Not greedy — it uses its natural height, which grows as
/// content streams in.
pub struct TranscriptWidget {
    prompt: String,
    finished: bool,
    /// Reasoning text accumulated during streaming.
    reasoning: Vec<Line<'static>>,
    reasoning_partial: String,
    /// Tool calls that are not read/edit variants. Each entry is the tool
    /// name plus its output lines (indented).
    tools: Vec<(String, Vec<String>)>,
    response_lines: Vec<Line<'static>>,
    response_partial: String,
    events: Vec<WidgetEvent>,
}

impl TranscriptWidget {
    pub fn new(prompt: String) -> Self {
        Self {
            prompt,
            finished: false,
            reasoning: Vec::new(),
            reasoning_partial: String::new(),
            tools: Vec::new(),
            response_lines: Vec::new(),
            response_partial: String::new(),
            events: Vec::new(),
        }
    }

    fn flush_reasoning_partial(&mut self) {
        if !self.reasoning_partial.is_empty() {
            let remaining = std::mem::take(&mut self.reasoning_partial);
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            self.reasoning
                .push(Line::from(Span::styled(format!("  {remaining}"), style)));
        }
    }

    fn flush_response_partial(&mut self) {
        if !self.response_partial.is_empty() {
            let remaining = std::mem::take(&mut self.response_partial);
            self.response_lines
                .push(Line::from(Span::styled(remaining, Style::default())));
        }
    }

    /// Whether a tool name indicates a read or edit operation.
    fn is_read_or_edit(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        // Match whole-word read/edit operations, not arbitrary substrings
        // (so `thread_search` and `credit_check` are not filtered).
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        words.iter().any(|w| *w == "read" || *w == "edit")
            || lower == "read"
            || lower == "edit"
            || lower == "read_file"
            || lower == "edit_file"
            || lower == "readfile"
            || lower == "editfile"
    }
}

impl Widget for TranscriptWidget {
    fn lines(&self, _width: u16) -> Vec<Line<'static>> {
        let marker_style = Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD);
        let text_style = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        let tool_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        let reasoning_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);

        let mut lines: Vec<Line<'static>> = Vec::new();

        // 1. Prompt line.
        lines.push(Line::from(vec![
            Span::styled("> ", marker_style),
            Span::styled(self.prompt.clone(), text_style),
        ]));

        // 2. Reasoning — last 3 raw lines.
        let mut reasoning = self.reasoning.clone();
        if !self.reasoning_partial.is_empty() {
            reasoning.push(Line::from(Span::styled(
                format!("  {}", self.reasoning_partial),
                reasoning_style,
            )));
        }
        if !reasoning.is_empty() {
            lines.push(Line::from(""));
            let start = reasoning.len().saturating_sub(3);
            lines.extend(reasoning[start..].to_vec());
        }

        // 3. Tool list.
        if !self.tools.is_empty() {
            lines.push(Line::from(""));
            for (name, output) in &self.tools {
                lines.push(Line::from(Span::styled(format!("  {name}"), tool_style)));
                for line in output {
                    lines.push(Line::from(Span::styled(format!("    {line}"), tool_style)));
                }
            }
        }

        // 4. Response.
        let mut response_lines = self.response_lines.clone();
        if !self.response_partial.is_empty() {
            response_lines.push(Line::from(Span::styled(
                self.response_partial.clone(),
                Style::default(),
            )));
        }
        if !response_lines.is_empty() {
            lines.push(Line::from(""));
            lines.extend(response_lines);
        }

        // 5. Stats line (finished only).
        if self.finished {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                summary_line(&self.events),
                Style::default(),
            )));
        }

        lines
    }

    fn is_expandable(&self) -> bool {
        false
    }

    fn is_active(&self) -> bool {
        !self.finished
    }

    fn toggle_expand(&mut self) {
        // No-op: not expandable.
    }

    fn push_stream_text(&mut self, text: &str, style: Style) {
        self.reasoning_partial.push_str(text);
        while let Some(newline_pos) = self.reasoning_partial.find('\n') {
            let line: String = self.reasoning_partial[..newline_pos].into();
            self.reasoning_partial = self.reasoning_partial[newline_pos + 1..].into();
            self.reasoning
                .push(Line::from(Span::styled(format!("  {line}"), style)));
        }
    }

    fn push_stream_line(&mut self, line: Line<'static>) {
        self.flush_reasoning_partial();
        self.reasoning.push(line);
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
        self.flush_reasoning_partial();
        self.events.push(WidgetEvent::ToolCall {
            name: name.to_string(),
            output: output.to_string(),
        });
        if Self::is_read_or_edit(name) {
            return;
        }
        let output_lines: Vec<String> = output.lines().map(|s| s.to_string()).collect();
        self.tools.push((name.to_string(), output_lines));
    }

    fn replace_response(&mut self, text: &str) {
        self.response_lines.clear();
        self.response_partial.clear();
        self.push_response_text(text);
    }

    fn flush_partial(&mut self) {
        self.flush_response_partial();
        self.flush_reasoning_partial();
    }

    fn finish(&mut self, events: Vec<WidgetEvent>) {
        self.flush_partial();
        self.finished = true;
        self.events.extend(events);
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
    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.lines.is_empty() {
            return Vec::new();
        }
        if self.expanded {
            self.lines
                .iter()
                .map(|(s, style)| Line::from(Span::styled(s.clone(), *style)))
                .collect()
        } else {
            // First line, truncated to fit `width` display columns so the
            // outer `Paragraph` wrap cannot split it across rows.
            let (text, style) = &self.lines[0];
            let truncated: String = text
                .chars()
                .scan(0usize, |cols, ch| {
                    let w = ch.width().unwrap_or(0);
                    if *cols + w > width as usize {
                        None
                    } else {
                        *cols += w;
                        Some(ch)
                    }
                })
                .collect();
            vec![Line::from(Span::styled(truncated, *style))]
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
