//! Markdown-to-ratatui rendering.
//!
//! Converts a markdown string into a `Vec<Line<'static>>` of styled spans
//! for display in the transcript. Block events (paragraph, heading,
//! blockquote, code block, list, thematic break, table) map to lines;
//! inline events (strong, emphasis, code, strikethrough, link, image) map
//! to spans. Fenced code blocks get syntect syntax highlighting when the
//! language is recognized; otherwise they fall back to a monochrome
//! background/foreground style. Indented code blocks always use the
//! monochrome style.
//!
//! The renderer emits logical lines only — wrapping remains ratatui's job
//! at render time.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::tui::motif::{self, glyph};
use crate::tui::theme::DEFAULT_THEME;

/// Extension options enabled for markdown parsing: tables, strikethrough,
/// and task lists.
const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS);

/// Render `markdown` into styled lines using `base` as the default style.
///
/// `rule_width` is the display width available for full-width rules (the
/// thematic break and code-block borders). Callers pass the transcript's
/// content width (usually `width - 1` for the one-cell margin).
pub fn render(markdown: &str, base: Style, rule_width: u16) -> Vec<Line<'static>> {
    render_with(markdown, base, rule_width, false)
}

/// Render `markdown` into styled lines treating a `SoftBreak` (a single
/// newline) as a hard line break everywhere, so literal newlines in the
/// source are preserved.
///
/// This is the prompt path: the prompt is what the operator actually typed,
/// so their hard newlines must survive markdown parsing. Fenced code blocks
/// and other multi-line constructs still parse as one unit — only bare
/// newlines between spaces become hard breaks. Blank lines remain paragraph
/// separators.
pub fn render_preserving_breaks(
    markdown: &str,
    base: Style,
    rule_width: u16,
) -> Vec<Line<'static>> {
    render_with(markdown, base, rule_width, true)
}

fn render_with(
    markdown: &str,
    base: Style,
    rule_width: u16,
    soft_break_hard: bool,
) -> Vec<Line<'static>> {
    let mut renderer = Renderer {
        base,
        rule_width,
        soft_break_hard,
        lines: Vec::new(),
        current: Vec::new(),
        style_stack: vec![base],
        list_stack: Vec::new(),
        table: None,
        code_block: None,
        quote_depth: 0,
        heading_level: None,
        last_block: None,
    };
    renderer.run(markdown);
    renderer.finish();
    renderer.lines
}

struct Renderer {
    base: Style,
    /// Display width for full-width rules (thematic break, code borders).
    rule_width: u16,
    /// Whether `SoftBreak` events are hard line breaks rather than a space.
    /// The prompt renderer opts in so a user's literal newlines survive.
    soft_break_hard: bool,
    lines: Vec<Line<'static>>,
    /// Spans accumulated for the current logical line.
    current: Vec<Span<'static>>,
    /// Stack of active inline styles (innermost last).
    style_stack: Vec<Style>,
    /// Stack of list item prefixes (bullet/number), innermost last.
    list_stack: Vec<ListItem>,
    /// Active table column state.
    table: Option<TableState>,
    /// Active code-block buffering state (language + raw text), if inside
    /// a code block.
    code_block: Option<CodeBlock>,
    /// Depth of nested blockquotes (drives the `║ ` line prefix).
    quote_depth: usize,
    /// The level of the heading currently being accumulated, if any.
    heading_level: Option<HeadingLevel>,
    /// The kind of the most recently completed block (drives blank-line
    /// spacing between blocks).
    last_block: Option<BlockKind>,
}

/// The kind of a completed block, used to decide blank-line spacing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Paragraph,
    Heading,
    CodeBlock,
    Other,
}

/// State for a code block currently being buffered.
struct CodeBlock {
    /// Whether this is a fenced block (vs. indented). Fenced blocks get
    /// top/bottom borders; indented blocks keep their plain look.
    is_fenced: bool,
    /// The fenced language identifier, if any. `None` for indented blocks
    /// and unlabeled fences.
    language: Option<String>,
    /// Raw text chunks accumulated while inside the block.
    text: String,
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
            Event::InlineHtml(html) => self.html_text(&html),
            Event::Html(html) => self.html_text(&html),
            Event::InlineMath(m) => self.text(&m),
            Event::DisplayMath(m) => self.text(&m),
            Event::FootnoteReference(r) => self.text(&r),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.blank_line_before_block(BlockKind::Paragraph);
            }
            Tag::Heading { level, .. } => {
                self.blank_line_before_block(BlockKind::Heading);
                self.heading_level = Some(level);
                self.push_style(self.heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.blank_line_before_block(BlockKind::Other);
                self.flush_line();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                let (is_fenced, language) = match kind {
                    CodeBlockKind::Fenced(info) => {
                        // Keep only the first whitespace-delimited token, so
                        // annotations like `rust,ignore` or `js {1}` still
                        // resolve to their language syntax.
                        // Strip comma-delimited annotations (e.g.
                        // `rust,ignore` → `rust`) so the first token
                        // always resolves to a real syntax.
                        let language = info
                            .split_whitespace()
                            .next()
                            .map(|s| s.split(',').next().unwrap_or(s))
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        (true, language)
                    }
                    CodeBlockKind::Indented => (false, None),
                };
                self.code_block = Some(CodeBlock {
                    is_fenced,
                    language,
                    text: String::new(),
                });
            }
            Tag::List(start) => {
                self.blank_line_before_block(BlockKind::Other);
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
                    self.current.push(Span::styled(
                        format!("{} ", glyph::QUOTE_BAR),
                        self.base.fg(DEFAULT_THEME.blockquote_bar),
                    ));
                }
                if nest_indent > 0 {
                    self.current.push(Span::raw(" ".repeat(nest_indent)));
                }
                if item.ordered {
                    let n = item.next_number;
                    item.next_number += 1;
                    let bullet = format!("{n}. ");
                    item.bullet_width = bullet.chars().count();
                    self.current.push(Span::styled(
                        bullet,
                        self.base.fg(DEFAULT_THEME.list_bullet),
                    ));
                } else {
                    item.bullet_width = 2;
                    self.current.push(Span::styled(
                        format!("{} ", glyph::BULLET),
                        self.base.fg(DEFAULT_THEME.list_bullet),
                    ));
                }
            }
            Tag::Table(alignments) => {
                self.blank_line_before_block(BlockKind::Other);
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
            Tag::Link { .. } => self.push_style(self.top_style().fg(DEFAULT_THEME.link)),
            Tag::Image { .. } => self.push_style(self.top_style().fg(DEFAULT_THEME.link)),
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
            TagEnd::Paragraph => {
                self.flush_line();
                self.last_block = Some(BlockKind::Paragraph);
            }
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flank_heading();
                self.flush_line();
                self.heading_level = None;
                self.last_block = Some(BlockKind::Heading);
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.last_block = Some(BlockKind::Other);
            }
            TagEnd::CodeBlock => {
                let block = self.code_block.take();
                self.flush_code_block(block);
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                self.last_block = Some(BlockKind::Other);
            }
            TagEnd::Item => {
                self.flush_line();
                self.last_block = Some(BlockKind::Other);
            }
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

    /// Handle an `Html` or `InlineHtml` event payload. When
    /// `soft_break_hard` is on (the prompt path), split the payload at
    /// newlines and flush each segment as a separate line so that
    /// multiline HTML blocks preserve their source layout. Otherwise,
    /// accumulate into the current line unchanged.
    fn html_text(&mut self, html: &str) {
        if self.soft_break_hard {
            let mut segments = html.split('\n').peekable();
            while let Some(seg) = segments.next() {
                self.text(seg);
                if segments.peek().is_some() {
                    self.flush_line();
                }
            }
        } else {
            self.text(html);
        }
    }

    fn text(&mut self, text: &str) {
        if self.code_block.is_some() {
            self.code_block_text(text);
            return;
        }
        self.begin_line();
        self.current
            .push(Span::styled(text.to_string(), self.top_style()));
    }

    fn code(&mut self, code: &str) {
        self.begin_line();
        let style = self.base.fg(DEFAULT_THEME.code_inline_fg);
        self.current.push(Span::styled(code.to_string(), style));
    }

    fn soft_break(&mut self) {
        // Inside a blockquote or list, a soft break is a hard line break so
        // the block prefix/indentation is re-applied to the next line. The
        // prompt path also opts into treating every soft break as a hard
        // break, preserving the operator's literal newlines.
        if self.soft_break_hard || self.quote_depth > 0 || !self.list_stack.is_empty() {
            self.flush_line();
        } else {
            self.current.push(Span::raw(" "));
        }
    }

    fn hard_break(&mut self) {
        self.flush_line();
    }

    fn rule(&mut self) {
        self.blank_line_before_block(BlockKind::Other);
        self.flush_line();
        self.lines.push(motif::streamline_rule(self.rule_width));
        self.last_block = Some(BlockKind::Other);
    }

    fn task_marker(&mut self, checked: bool) {
        // A task-list item replaces its bullet with a checkbox. Drop the
        // bullet span that `Tag::Item` pushed.
        if let Some(item) = self.list_stack.last_mut() {
            if !item.task {
                item.task = true;
                self.current.pop();
                item.bullet_width = 2;
            }
        }
        let mark = if checked {
            glyph::TASK_DONE
        } else {
            glyph::TASK_OPEN
        };
        let color = if checked {
            DEFAULT_THEME.success
        } else {
            DEFAULT_THEME.list_bullet
        };
        self.current
            .push(Span::styled(format!("{mark} "), self.base.fg(color)));
    }

    fn heading_style(&self, level: HeadingLevel) -> Style {
        let mut style = self.base.add_modifier(Modifier::BOLD);
        match level {
            HeadingLevel::H1 => style = style.fg(DEFAULT_THEME.heading_primary),
            HeadingLevel::H2 => style = style.fg(DEFAULT_THEME.heading_primary),
            HeadingLevel::H3 => style = style.fg(DEFAULT_THEME.heading_secondary),
            HeadingLevel::H4 => style = style.fg(DEFAULT_THEME.heading_secondary),
            HeadingLevel::H5 => style = style.fg(DEFAULT_THEME.heading_secondary),
            HeadingLevel::H6 => style = style.fg(DEFAULT_THEME.heading_secondary),
        }
        style
    }

    /// Flank H1/H2 headings with `◆ TITLE ◆` / `─── TITLE ───`; H3-H6 stay
    /// quiet (chrome bold, no flank) so nesting reads as a hierarchy.
    fn flank_heading(&mut self) {
        let Some(level) = self.heading_level else {
            return;
        };
        let (flank, fill) = match level {
            HeadingLevel::H1 => (glyph::DIAMOND, ""),
            HeadingLevel::H2 => ("───", ""),
            _ => return,
        };
        let style = self.heading_style(level);
        let mut spans = Vec::new();
        spans.push(Span::styled(flank.to_string(), style));
        if !fill.is_empty() {
            spans.push(Span::styled(fill.to_string(), style));
        }
        spans.push(Span::styled(" ", style));
        spans.extend(std::mem::take(&mut self.current));
        spans.push(Span::styled(" ", style));
        if !fill.is_empty() {
            spans.push(Span::styled(fill.to_string(), style));
        }
        spans.push(Span::styled(flank.to_string(), style));
        self.current = spans;
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

    /// Push a blank line before a block according to the spacing rules:
    /// between consecutive paragraphs, before a heading (unless first), and
    /// around code blocks (unless the block is first, or the adjacent block
    /// is also a code block).
    fn blank_line_before_block(&mut self, current: BlockKind) {
        let insert = match (self.last_block, current) {
            (Some(BlockKind::CodeBlock), BlockKind::CodeBlock) => false,
            (Some(BlockKind::CodeBlock), _) => true,
            (Some(BlockKind::Paragraph), BlockKind::Paragraph) => true,
            (Some(_), BlockKind::Heading) => true,
            (Some(prev), BlockKind::CodeBlock) => prev != BlockKind::CodeBlock,
            _ => false,
        };
        if insert {
            self.lines.push(Line::from(""));
        }
        self.last_block = Some(current);
    }

    /// Apply the active block prefix (blockquote markers + list continuation
    /// indentation) to a freshly-started line.
    fn begin_line(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        for _ in 0..self.quote_depth {
            self.current.push(Span::styled(
                format!("{} ", glyph::QUOTE_BAR),
                self.base.fg(DEFAULT_THEME.blockquote_bar),
            ));
        }
        if let Some(item) = self.list_stack.last() {
            let indent = (self.list_stack.len() - 1) * 2 + item.bullet_width;
            if indent > 0 {
                self.current.push(Span::raw(" ".repeat(indent)));
            }
        }
    }

    /// Buffer code-block text verbatim, preserving blank lines.
    fn code_block_text(&mut self, text: &str) {
        if let Some(block) = &mut self.code_block {
            block.text.push_str(text);
        }
    }

    /// Flush a completed code block into styled lines.
    fn flush_code_block(&mut self, block: Option<CodeBlock>) {
        let Some(block) = block else {
            return;
        };

        // Split the buffered text into lines.
        let mut block_lines: Vec<String> = block.text.split('\n').map(str::to_string).collect();
        // A trailing newline yields a spurious empty final line; drop it.
        if block_lines.len() > 1 && block_lines.last().is_some_and(|l| l.is_empty()) {
            block_lines.pop();
        }

        let is_fenced = block.is_fenced;
        let highlighted = if is_fenced {
            highlight_lines(&block.language, &block_lines)
        } else {
            None
        };

        // Blank line before the block unless it is the first block emitted
        // or the previous block was also a code block.
        self.blank_line_before_block(BlockKind::CodeBlock);

        // Compute the display-width of the longest content line (including
        // the single-space indent that fenced blocks receive) so the
        // top/bottom borders hug the content rather than spanning the full
        // terminal width.  Clamp to `rule_width` so long lines still hit
        // the edge.
        let code_rule_width = if is_fenced {
            let max_content = block_lines
                .iter()
                .map(|l| UnicodeWidthStr::width(format!(" {l}").as_str()))
                .max()
                .unwrap_or(0);
            (max_content as u16).min(self.rule_width)
        } else {
            0
        };

        if is_fenced {
            // Top border: brass groove with corner caps.
            self.lines
                .push(motif::groove_rule(code_rule_width, ("◤", "◥")));
        }

        let indent = if is_fenced { " " } else { "" };
        let mut highlighted = highlighted.into_iter().flatten();
        for line in &block_lines {
            let spans: Vec<Span<'static>> = highlighted.next().unwrap_or_else(|| {
                vec![Span::styled(
                    format!("{indent}{line}"),
                    self.base
                        .bg(DEFAULT_THEME.code_bg)
                        .fg(DEFAULT_THEME.code_fg),
                )]
            });
            self.lines.push(Line::from(spans));
        }

        if is_fenced {
            // Bottom border: brass groove with corner caps and a lower bar.
            self.lines.push(motif::groove_rule_with_fill(
                code_rule_width,
                ("◣", "◢"),
                "▁",
            ));
        }

        self.last_block = Some(BlockKind::CodeBlock);

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
        self.last_block = Some(BlockKind::Other);
    }
}

/// Highlight code-block lines with syntect, returning one span list per input
/// line in order. Returns `None` when the language is unknown or syntect is
/// unavailable, so the caller falls back to the monochrome style.
fn highlight_lines(language: &Option<String>, lines: &[String]) -> Option<Vec<Vec<Span<'static>>>> {
    let language = language.as_deref()?;
    let syntax_set = syntax_set();
    let syntax = find_syntax(syntax_set, language)?;
    let theme = syntect_theme();

    let mut highlighter = syntect::easy::HighlightLines::new(syntax, theme);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let ranges = highlighter.highlight_line(line, syntax_set).ok()?;
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(ranges.len() + 1);
        spans.push(Span::styled(
            " ",
            Style::default().bg(DEFAULT_THEME.code_bg),
        ));
        for (style, text) in ranges {
            let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            spans.push(Span::styled(
                text.to_string(),
                Style::default().fg(fg).bg(DEFAULT_THEME.code_bg),
            ));
        }
        out.push(spans);
    }
    Some(out)
}

/// Resolve a language identifier to a syntax reference.
fn find_syntax<'a>(
    syntax_set: &'a syntect::parsing::SyntaxSet,
    language: &str,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    syntax_set
        .find_syntax_by_token(language)
        .or_else(|| syntax_set.find_syntax_by_extension(language))
        .or_else(|| syntax_set.find_syntax_by_name(language))
}

/// Lazily load the default syntax set (embedded at compile time).
fn syntax_set() -> &'static syntect::parsing::SyntaxSet {
    static SYNTAX_SET: std::sync::OnceLock<syntect::parsing::SyntaxSet> =
        std::sync::OnceLock::new();
    SYNTAX_SET.get_or_init(syntect::parsing::SyntaxSet::load_defaults_nonewlines)
}

/// Lazily load the default syntect theme.
fn syntect_theme() -> &'static syntect::highlighting::Theme {
    static THEME: std::sync::OnceLock<syntect::highlighting::Theme> = std::sync::OnceLock::new();
    THEME.get_or_init(|| {
        syntect::highlighting::ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_else(|| {
                syntect::highlighting::ThemeSet::load_defaults()
                    .themes
                    .into_values()
                    .next()
                    .expect("default syntect themes are non-empty")
            })
    })
}

/// Render markdown with a default (unstyled) base style.
#[cfg(test)]
fn render_default(markdown: &str) -> Vec<Line<'static>> {
    render(markdown, Style::default(), 40)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    /// Top groove border at `width` (for code-block test expectations).
    fn groove_top(width: u16) -> String {
        motif::groove_rule(width, ("◤", "◥")).to_string()
    }

    /// Bottom groove border at `width`.
    fn groove_bottom(width: u16) -> String {
        motif::groove_rule_with_fill(width, ("◣", "◢"), "▁").to_string()
    }

    #[test]
    fn renders_plain_paragraph() {
        let lines = render_default("hello world");
        assert_eq!(text_of(&lines), vec!["hello world"]);
    }

    #[test]
    fn render_preserving_breaks_keeps_bare_newlines() {
        let lines = render_preserving_breaks("one\ntwo", Style::default(), 40);
        assert_eq!(text_of(&lines), vec!["one", "two"]);
    }

    #[test]
    fn render_preserving_breaks_still_parses_fenced_blocks_as_one_unit() {
        let lines = render_preserving_breaks("```\nlet x = 1;\n```", Style::default(), 40);
        assert_eq!(
            text_of(&lines),
            vec![groove_top(11), " let x = 1;".to_string(), groove_bottom(11)]
        );
    }

    #[test]
    fn renders_headings() {
        let lines = render_default("# Title\n\n## Sub");
        assert_eq!(text_of(&lines), vec!["◆ Title ◆", "", "─── Sub ───"]);
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
        // Unlabeled fence: monochrome block with borders.
        let text = text_of(&lines);
        assert_eq!(
            text,
            vec![
                groove_top(11),
                " let x = 1;".to_string(),
                groove_bottom(11)
            ]
        );
        assert!(lines[1].spans[0].style.bg.is_some());
    }

    #[test]
    fn renders_list() {
        let lines = render_default("- one\n- two\n- three");
        assert_eq!(text_of(&lines), vec!["» one", "» two", "» three"]);
    }

    #[test]
    fn renders_ordered_list() {
        let lines = render_default("1. first\n2. second");
        assert_eq!(text_of(&lines), vec!["1. first", "2. second"]);
    }

    #[test]
    fn renders_task_list() {
        let lines = render_default("- [ ] todo\n- [x] done");
        assert_eq!(text_of(&lines), vec!["▢ todo", "▣ done"]);
    }

    #[test]
    fn renders_blockquote() {
        let lines = render_default("> quoted");
        assert_eq!(text_of(&lines), vec!["║ quoted"]);
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
        assert!(text[1].chars().all(|c| c == '─' || c == '═'));
    }

    #[test]
    fn code_block_preserves_blank_lines() {
        let lines = render_default("```\na\n\nb\n```");
        let text = text_of(&lines);
        assert_eq!(
            text,
            vec![
                groove_top(2),
                " a".to_string(),
                " ".to_string(),
                " b".to_string(),
                groove_bottom(2)
            ]
        );
    }

    #[test]
    fn blockquote_prefix_applies_to_continuation_lines() {
        let lines = render_default("> one\n> two");
        assert_eq!(text_of(&lines), vec!["║ one", "║ two"]);
    }

    #[test]
    fn blockquote_prefix_applies_to_multiple_paragraphs() {
        let lines = render_default("> para one\n>\n> para two");
        let text = text_of(&lines);
        assert_eq!(text, vec!["║ para one", "", "║ para two"]);
    }

    #[test]
    fn nested_lists_are_indented() {
        let lines = render_default("- a\n  - b");
        let text = text_of(&lines);
        assert_eq!(text.len(), 2);
        assert_eq!(text[0], "» a");
        assert_eq!(text[1], "  » b");
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

    #[test]
    fn consecutive_paragraphs_have_blank_line_between() {
        let lines = render_default("one\n\ntwo");
        assert_eq!(text_of(&lines), vec!["one", "", "two"]);
    }

    #[test]
    fn heading_gets_blank_line_before_unless_first() {
        let lines = render_default("para\n\n# Heading");
        assert_eq!(text_of(&lines), vec!["para", "", "◆ Heading ◆"]);
    }

    #[test]
    fn fenced_code_block_gets_blank_line_before_unless_first() {
        let lines = render_default("para\n\n```\ncode\n```");
        assert_eq!(
            text_of(&lines),
            vec![
                "para".to_string(),
                "".to_string(),
                groove_top(5),
                " code".to_string(),
                groove_bottom(5)
            ]
        );
    }

    #[test]
    fn unknown_language_falls_back_to_monochrome() {
        let lines = render_default("```no-such-lang\nlet x = 1;\n```");
        let text = text_of(&lines);
        assert_eq!(
            text,
            vec![groove_top(11), " let x = 1;".to_string(), groove_bottom(11)]
        );
        assert!(lines[1].spans[0].style.bg.is_some());
    }

    #[test]
    fn rust_code_block_is_highlighted_with_code_bg() {
        let lines = render_default("```rust\nfn main() {}\n```");
        let text = text_of(&lines);
        assert_eq!(
            text,
            vec![
                groove_top(14),
                " fn main() {}".to_string(),
                groove_bottom(14)
            ]
        );
        // The code line is highlighted into multiple token spans, each with
        // the code background.
        let code_line = &lines[1];
        assert!(code_line.spans.len() > 1, "{code_line:?}");
        assert!(code_line
            .spans
            .iter()
            .all(|s| s.style.bg == Some(DEFAULT_THEME.code_bg)));
    }

    #[test]
    fn render_preserving_breaks_splits_multiline_html_on_newlines() {
        // `<div>\none\ntwo\n</div>` is a block-level HTML event where
        // pulldown-cmark emits `Html` payloads with embedded newlines
        // rather than `SoftBreak` events. The preserving-breaks path
        // must split those payloads into separate lines.
        let input = "<div>\none\ntwo\n</div>";
        let lines = render_preserving_breaks(input, Style::default(), 40);
        assert_eq!(text_of(&lines), vec!["<div>", "one", "two", "</div>"]);
    }

    #[test]
    fn normal_renderer_does_not_split_html_on_newlines() {
        // Outside the prompt path, the normal renderer produces a single
        // line — pulldown-cmark treats bare newlines between inline HTML
        // and text as SoftBreak events (space), so the literal newlines
        // are gone.
        let lines = render_default("<div>\none\ntwo\n</div>");
        let text = text_of(&lines);
        assert_eq!(text.len(), 1);
        assert_eq!(text[0], "<div>onetwo</div>");
    }

    #[test]
    fn comma_annotated_language_strips_to_base_syntax() {
        // `rust,ignore` should resolve to `rust`, not fall back to mono.
        let lines = render_default("```rust,ignore\nfn main() {}\n```");
        let text = text_of(&lines);
        assert_eq!(
            text,
            vec![
                groove_top(14),
                " fn main() {}".to_string(),
                groove_bottom(14)
            ]
        );
        let code_line = &lines[1];
        assert!(
            code_line.spans.len() > 1,
            "comma-annotated rust fence should be highlighted: {code_line:?}"
        );
    }
}
