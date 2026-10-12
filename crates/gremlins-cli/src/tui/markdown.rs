//! Markdown-to-ratatui rendering.
//!
//! Converts a markdown string into a `Vec<Line<'static>>` of styled spans
//! for display in the transcript. Block events (paragraph, heading,
//! blockquote, code block, list, thematic break, table) map to lines;
//! inline events (strong, emphasis, code, strikethrough, link, image) map
//! to spans. Code blocks get a distinct background/foreground style; there
//! is no syntax highlighting.
//!
//! The renderer emits logical lines only — wrapping remains ratatui's job
//! at render time.

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Extension options enabled for markdown parsing: tables, strikethrough,
/// and task lists.
const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS);

/// Style applied to fenced/indented code blocks.
fn code_block_style(base: Style) -> Style {
    base.bg(Color::Rgb(30, 30, 30))
        .fg(Color::Rgb(200, 200, 200))
}

/// Render `markdown` into styled lines using `base` as the default style.
pub fn render(markdown: &str, base: Style) -> Vec<Line<'static>> {
    let mut renderer = Renderer {
        base,
        lines: Vec::new(),
        current: Vec::new(),
        style_stack: vec![base],
        list_stack: Vec::new(),
        table: None,
        in_code_block: false,
        quote_depth: 0,
    };
    renderer.run(markdown);
    renderer.finish();
    renderer.lines
}

struct Renderer {
    base: Style,
    lines: Vec<Line<'static>>,
    /// Spans accumulated for the current logical line.
    current: Vec<Span<'static>>,
    /// Stack of active inline styles (innermost last).
    style_stack: Vec<Style>,
    /// Stack of list item prefixes (bullet/number), innermost last.
    list_stack: Vec<ListItem>,
    /// Active table column state.
    table: Option<TableState>,
    /// Whether we are inside a code block (to suppress inline parsing).
    in_code_block: bool,
    /// Depth of nested blockquotes (drives the `│ ` line prefix).
    quote_depth: usize,
}

/// State for the current list nesting level.
struct ListItem {
    /// Whether this is an ordered list.
    ordered: bool,
    /// The next number to use for an ordered list item.
    next_number: u64,
    /// Whether the current item is a task-list item (suppresses the bullet).
    task: bool,
    /// Display width of the bullet prefix, used to indent continuation lines.
    bullet_width: usize,
}

/// State for rendering a table.
struct TableState {
    /// Column alignments.
    alignments: Vec<Alignment>,
    /// Buffered rows (each cell is a styled span list).
    rows: Vec<Vec<Vec<Span<'static>>>>,
    /// Cells accumulated for the row currently being parsed.
    current_row: Vec<Vec<Span<'static>>>,
    /// Whether the table has a header row (i.e. a `|---|` separator).
    has_header: bool,
}

impl Renderer {
    fn run(&mut self, markdown: &str) {
        let parser = Parser::new_ext(markdown, OPTIONS);
        for event in parser {
            self.handle(event);
        }
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.code(&code),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => self.task_marker(checked),
            Event::InlineHtml(html) => self.text(&html),
            Event::Html(html) => self.text(&html),
            Event::InlineMath(m) => self.text(&m),
            Event::DisplayMath(m) => self.text(&m),
            Event::FootnoteReference(r) => self.text(&r),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.push_style(self.heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(_) => {
                self.in_code_block = true;
                self.flush_line();
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(ListItem {
                    ordered: start.is_some(),
                    next_number: start.unwrap_or(1),
                    task: false,
                    bullet_width: 0,
                });
            }
            Tag::Item => {
                self.flush_line();
                let nest_indent = (self.list_stack.len() - 1) * 2;
                let Some(item) = self.list_stack.last_mut() else {
                    return;
                };
                item.task = false;
                // Blockquote markers, then list nesting indentation.
                for _ in 0..self.quote_depth {
                    self.current
                        .push(Span::styled("│ ", self.base.fg(Color::DarkGray)));
                }
                if nest_indent > 0 {
                    self.current.push(Span::raw(" ".repeat(nest_indent)));
                }
                if item.ordered {
                    let n = item.next_number;
                    item.next_number += 1;
                    let bullet = format!("{n}. ");
                    item.bullet_width = bullet.chars().count();
                    self.current
                        .push(Span::styled(bullet, self.base.fg(Color::DarkGray)));
                } else {
                    item.bullet_width = 2;
                    self.current
                        .push(Span::styled("• ", self.base.fg(Color::DarkGray)));
                }
            }
            Tag::Table(alignments) => {
                self.flush_line();
                self.table = Some(TableState {
                    alignments,
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    has_header: false,
                });
            }
            Tag::TableHead => {}
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.current_row.clear();
                }
            }
            Tag::TableCell => {}
            Tag::Emphasis => self.push_style(self.top_style().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(self.top_style().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(self.top_style().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::Link { .. } => self.push_style(self.top_style().fg(Color::Cyan)),
            Tag::Image { .. } => self.push_style(self.top_style().fg(Color::Cyan)),
            Tag::HtmlBlock => {}
            Tag::FootnoteDefinition(_) => {}
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_)
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_line(),
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush_line();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.flush_line();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Table => {
                self.flush_line();
                self.emit_table();
                self.table = None;
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.has_header = true;
                    let row = std::mem::take(&mut t.current_row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.current_row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    let cell = std::mem::take(&mut self.current);
                    t.current_row.push(cell);
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image => {
                self.pop_style();
            }
            TagEnd::HtmlBlock => self.flush_line(),
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_block_text(text);
            return;
        }
        self.begin_line();
        self.current
            .push(Span::styled(text.to_string(), self.top_style()));
    }

    fn code(&mut self, code: &str) {
        self.begin_line();
        let style = code_block_style(self.top_style());
        self.current.push(Span::styled(code.to_string(), style));
    }

    fn soft_break(&mut self) {
        // Inside a blockquote or list, a soft break is a hard line break so
        // the block prefix/indentation is re-applied to the next line.
        if self.quote_depth > 0 || !self.list_stack.is_empty() {
            self.flush_line();
        } else {
            self.current.push(Span::raw(" "));
        }
    }

    fn hard_break(&mut self) {
        self.flush_line();
    }

    fn rule(&mut self) {
        self.flush_line();
        self.lines.push(Line::from(Span::styled(
            "─".repeat(40),
            self.base.fg(Color::DarkGray),
        )));
    }

    fn task_marker(&mut self, checked: bool) {
        // A task-list item replaces its bullet with a checkbox. Drop the
        // bullet span that `Tag::Item` pushed.
        if let Some(item) = self.list_stack.last_mut() {
            if !item.task {
                item.task = true;
                self.current.pop();
                item.bullet_width = 4;
            }
        }
        let mark = if checked { "[x] " } else { "[ ] " };
        self.current
            .push(Span::styled(mark, self.base.fg(Color::DarkGray)));
    }

    fn heading_style(&self, level: HeadingLevel) -> Style {
        let mut style = self.base.add_modifier(Modifier::BOLD);
        match level {
            HeadingLevel::H1 => style = style.fg(Color::Yellow),
            HeadingLevel::H2 => style = style.fg(Color::Yellow),
            HeadingLevel::H3 => style = style.fg(Color::Cyan),
            HeadingLevel::H4 => style = style.fg(Color::Cyan),
            HeadingLevel::H5 => style = style.fg(Color::Cyan),
            HeadingLevel::H6 => style = style.fg(Color::Cyan),
        }
        style
    }

    fn top_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&self.base)
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    /// Flush the current line even when empty, so blank lines survive.
    fn flush_line_force(&mut self) {
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    /// Apply the active block prefix (blockquote markers + list continuation
    /// indentation) to a freshly-started line.
    fn begin_line(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        for _ in 0..self.quote_depth {
            self.current
                .push(Span::styled("│ ", self.base.fg(Color::DarkGray)));
        }
        if let Some(item) = self.list_stack.last() {
            let indent = (self.list_stack.len() - 1) * 2 + item.bullet_width;
            if indent > 0 {
                self.current.push(Span::raw(" ".repeat(indent)));
            }
        }
    }

    /// Emit code-block text verbatim, preserving blank lines.
    fn code_block_text(&mut self, text: &str) {
        let style = code_block_style(self.base);
        let mut parts = text.split('\n');
        // The first segment continues the current line.
        if let Some(first) = parts.next() {
            if !first.is_empty() {
                self.current.push(Span::styled(first.to_string(), style));
            }
        }
        // Each subsequent segment begins a new line (blank segments become
        // empty lines).
        for part in parts {
            self.flush_line_force();
            if !part.is_empty() {
                self.current.push(Span::styled(part.to_string(), style));
            }
        }
    }

    fn finish(&mut self) {
        self.flush_line();
    }

    fn emit_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        if table.rows.is_empty() {
            return;
        }

        // Column widths are the max display width across every row (header
        // and body), so a wide body cell cannot overrun its column.
        let ncols = table.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut widths = vec![0usize; ncols];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                let w: usize = cell
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                if w > widths[i] {
                    widths[i] = w;
                }
            }
        }

        for (ri, row) in table.rows.iter().enumerate() {
            let is_header = table.has_header && ri == 0;
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (i, cell) in row.iter().enumerate() {
                let w = widths[i];
                let cell_width: usize = cell
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                let pad = w.saturating_sub(cell_width);
                let (left, right) = match table.alignments.get(i).copied() {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                if left > 0 {
                    spans.push(Span::raw(" ".repeat(left)));
                }
                for s in cell {
                    let mut span = s.clone();
                    if is_header {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                    spans.push(span);
                }
                if right > 0 {
                    spans.push(Span::raw(" ".repeat(right)));
                }
                if i + 1 < row.len() {
                    spans.push(Span::raw("  "));
                }
            }
            self.lines.push(Line::from(spans));
        }
    }
}

/// Render markdown with a default (unstyled) base style.
#[cfg(test)]
fn render_default(markdown: &str) -> Vec<Line<'static>> {
    render(markdown, Style::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn renders_plain_paragraph() {
        let lines = render_default("hello world");
        assert_eq!(text_of(&lines), vec!["hello world"]);
    }

    #[test]
    fn renders_headings() {
        let lines = render_default("# Title\n\n## Sub");
        assert_eq!(text_of(&lines), vec!["Title", "Sub"]);
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn renders_inline_styles() {
        let lines = render_default("**bold** and *em* and `code` and ~~strike~~");
        assert_eq!(text_of(&lines), vec!["bold and em and code and strike"]);
    }

    #[test]
    fn renders_fenced_code_block() {
        let lines = render_default("```\nlet x = 1;\n```");
        assert_eq!(text_of(&lines), vec!["let x = 1;"]);
        assert!(lines[0].spans[0].style.bg.is_some());
    }

    #[test]
    fn renders_list() {
        let lines = render_default("- one\n- two\n- three");
        assert_eq!(text_of(&lines), vec!["• one", "• two", "• three"]);
    }

    #[test]
    fn renders_ordered_list() {
        let lines = render_default("1. first\n2. second");
        assert_eq!(text_of(&lines), vec!["1. first", "2. second"]);
    }

    #[test]
    fn renders_task_list() {
        let lines = render_default("- [ ] todo\n- [x] done");
        assert_eq!(text_of(&lines), vec!["[ ] todo", "[x] done"]);
    }

    #[test]
    fn renders_blockquote() {
        let lines = render_default("> quoted");
        assert_eq!(text_of(&lines), vec!["│ quoted"]);
    }

    #[test]
    fn renders_table() {
        let lines = render_default("| a | b |\n|---|---|\n| 1 | 2 |");
        let text = text_of(&lines);
        assert_eq!(text.len(), 2);
        assert!(text[0].contains('a') && text[0].contains('b'));
        assert!(text[1].contains('1') && text[1].contains('2'));
    }

    #[test]
    fn renders_thematic_break() {
        let lines = render_default("before\n\n---\n\nafter");
        let text = text_of(&lines);
        assert_eq!(text.len(), 3);
        assert!(text[1].chars().all(|c| c == '─'));
    }

    #[test]
    fn code_block_preserves_blank_lines() {
        let lines = render_default("```\na\n\nb\n```");
        assert_eq!(text_of(&lines), vec!["a", "", "b"]);
    }

    #[test]
    fn blockquote_prefix_applies_to_continuation_lines() {
        let lines = render_default("> one\n> two");
        assert_eq!(text_of(&lines), vec!["│ one", "│ two"]);
    }

    #[test]
    fn blockquote_prefix_applies_to_multiple_paragraphs() {
        let lines = render_default("> para one\n>\n> para two");
        let text = text_of(&lines);
        assert!(text.iter().all(|l| l.starts_with('│')), "{text:?}");
    }

    #[test]
    fn nested_lists_are_indented() {
        let lines = render_default("- a\n  - b");
        let text = text_of(&lines);
        assert_eq!(text.len(), 2);
        assert_eq!(text[0], "• a");
        assert_eq!(text[1], "  • b");
    }

    #[test]
    fn table_widths_account_for_wide_body_cells() {
        let lines = render_default("| h |\n|---|\n| longer |");
        let text = text_of(&lines);
        assert_eq!(text.len(), 2);
        // The body cell is wider than the header, so the header is padded.
        assert_eq!(text[0], "h     ");
        assert_eq!(text[1], "longer");
    }
}
