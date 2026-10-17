//! Static color theme for the LiveArea TUI.
//!
//! A single dark theme expressed as a plain `struct` of `const` `Color`
//! fields. Every field is `Copy` and read somewhere in the widget layer so
//! clippy's `dead_code` lint stays clean. There is no runtime mutation.

use ratatui::style::Color;

/// The TUI color palette.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Default/primary text.
    pub text: Color,
    /// Dim secondary text (reasoning, tool events, stage/definition, etc.).
    pub subtle: Color,
    /// Code-block background (monochrome fallback and syntect tokens).
    pub code_bg: Color,
    /// Monochrome code-block foreground.
    pub code_fg: Color,
    /// Inline-code foreground (bright ivory).
    pub code_inline_fg: Color,
    /// Code-block top/bottom rule line.
    pub code_border: Color,
    /// H1/H2 headings.
    pub heading_primary: Color,
    /// H3-H6 headings.
    pub heading_secondary: Color,
    /// Links and images.
    pub link: Color,
    /// Turn prompt `» ` marker (also the info-bar project accent).
    pub prompt_marker: Color,
    /// Subtle background tint for the prompt line.
    pub prompt_bg: Color,
    /// Bold prompt text on the prompt line.
    pub prompt_text: Color,
    /// Blockquote `│ ` prefix.
    pub blockquote_bar: Color,
    /// List bullets and task-list checkboxes.
    pub list_bullet: Color,
    /// Turn separator and thematic-break rule.
    pub separator: Color,
    /// Cyan accents (input prompt, info-bar title, watch header, debug label).
    pub accent_primary: Color,
    /// Yellow accents (gremlin count, watch footer, stopped, connecting).
    pub accent_secondary: Color,
    /// Green accents (key hints, running, connected).
    pub success: Color,
    /// Red accents (following id, failed, debug id, ended).
    pub danger: Color,
    /// Input cursor block background.
    pub cursor: Color,
    /// Prompt-line margin bar (a fixed left-edge accent for prompt blocks).
    pub prompt_margin: Color,
    /// Near-white for rules and titles.
    pub chrome: Color,
    /// Deep band background.
    pub midnight: Color,
    /// Text on colored bands.
    pub ivory: Color,
    /// Startup spire gold gradient step.
    pub sunburst: Color,
    /// Wordmark banner blue glow — soft cornflower for a faint luminous impression.
    pub wordmark: Color,
}

/// The default (dark) theme.
pub const DEFAULT_THEME: Theme = Theme {
    text: Color::Rgb(224, 224, 228),
    subtle: Color::Rgb(122, 122, 130),
    code_bg: Color::Rgb(26, 22, 16),
    code_fg: Color::Rgb(232, 214, 168),
    code_inline_fg: Color::Rgb(245, 224, 170),
    code_border: Color::Rgb(181, 137, 0),
    heading_primary: Color::Rgb(212, 175, 55),
    heading_secondary: Color::Rgb(210, 210, 220),
    link: Color::Rgb(95, 175, 230),
    prompt_marker: Color::Rgb(196, 30, 58),
    prompt_bg: Color::Rgb(32, 27, 20),
    prompt_text: Color::Rgb(255, 255, 255),
    blockquote_bar: Color::Rgb(212, 175, 55),
    list_bullet: Color::Rgb(181, 137, 0),
    separator: Color::Rgb(122, 122, 130),
    accent_primary: Color::Rgb(212, 175, 55),
    accent_secondary: Color::Rgb(181, 137, 0),
    success: Color::Rgb(0, 155, 130),
    danger: Color::Rgb(196, 30, 58),
    cursor: Color::Rgb(212, 175, 55),
    prompt_margin: Color::Rgb(212, 175, 55),
    chrome: Color::Rgb(224, 224, 228),
    midnight: Color::Rgb(12, 14, 20),
    ivory: Color::Rgb(245, 240, 225),
    sunburst: Color::Rgb(232, 196, 88),
    wordmark: Color::Rgb(120, 155, 215),
};
