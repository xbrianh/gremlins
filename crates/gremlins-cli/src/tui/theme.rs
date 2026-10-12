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
    /// Code-block top/bottom rule line.
    pub code_border: Color,
    /// H1/H2 headings.
    pub heading_primary: Color,
    /// H3-H6 headings.
    pub heading_secondary: Color,
    /// Links and images.
    pub link: Color,
    /// Turn prompt `> ` marker (also the info-bar project accent).
    pub prompt_marker: Color,
    /// Subtle background tint for the prompt line.
    pub prompt_bg: Color,
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
}

/// The default (dark) theme.
pub const DEFAULT_THEME: Theme = Theme {
    text: Color::White,
    subtle: Color::DarkGray,
    code_bg: Color::Rgb(30, 30, 30),
    code_fg: Color::Rgb(200, 200, 200),
    code_border: Color::DarkGray,
    heading_primary: Color::Yellow,
    heading_secondary: Color::Cyan,
    link: Color::Cyan,
    prompt_marker: Color::Magenta,
    prompt_bg: Color::Rgb(40, 40, 40),
    blockquote_bar: Color::DarkGray,
    list_bullet: Color::DarkGray,
    separator: Color::DarkGray,
    accent_primary: Color::Cyan,
    accent_secondary: Color::Yellow,
    success: Color::Green,
    danger: Color::Red,
    cursor: Color::White,
};
