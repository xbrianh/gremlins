pub(crate) fn check_ln_symlink(cmd: &str) -> Option<String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let first = tokens.first()?.trim_matches(|c| c == '\'' || c == '"');
    // Match "ln" or paths ending in "/ln" (e.g. /bin/ln).
    if first != "ln" && !first.ends_with("/ln") {
        return None;
    }
    for tok in &tokens[1..] {
        let stripped = tok.trim_matches(|c| c == '\'' || c == '"');
        if stripped == "--symbolic" || stripped.starts_with("-s") {
            return Some("Error: creating symlinks is not allowed".into());
        }
        // Catch short-flag bundles like -sf, -fs, -sn. Any short flag
        // containing 's' is rejected. This still has false positives
        // (-ns, --version), but those are harmless denials in practice.
        if stripped.starts_with('-') && stripped.len() > 1 && stripped.contains('s') {
            return Some("Error: creating symlinks is not allowed".into());
        }
    }
    None
}

/// A single token produced by [`shell_tokenize`].
pub(crate) struct ShellToken {
    /// The raw verbatim text as it appeared in the command string,
    /// including any surrounding quotes and escape characters.
    pub(crate) raw: String,
    /// The resolved text with quotes stripped and escapes processed,
    /// as a POSIX shell would interpret it.
    pub(crate) value: String,
}

/// Shell-aware tokenizer: splits a command string into argument tokens
/// as a POSIX shell would, handling backslash escaping, single quotes,
/// and double quotes. Escapes are resolved and quotes are stripped.
///
/// This is not a full shell parser — it does not handle `$()` expansion,
/// `;`/`&&`/`||` command separators, or IFS splitting. It is designed for
/// the sandbox path checker which only needs to isolate individual path
/// arguments from the command string.
pub(crate) fn shell_tokenize(s: &str) -> Vec<ShellToken> {
    let mut tokens = Vec::new();
    let mut value = String::new();
    let mut raw = String::new();
    let mut chars = s.chars();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                raw.push(ch);
                loop {
                    match chars.next() {
                        Some('\'') => {
                            raw.push('\'');
                            break;
                        }
                        Some(c) => {
                            raw.push(c);
                            value.push(c);
                        }
                        None => break,
                    }
                }
            }
            '"' => {
                raw.push(ch);
                // Double-quoted: only \\, \", \$, \`, and \<newline>
                // are escape sequences; everything else is literal.
                loop {
                    match chars.next() {
                        None => break,
                        Some('"') => {
                            raw.push('"');
                            break;
                        }
                        Some('\\') => {
                            raw.push('\\');
                            match chars.next() {
                                Some(c @ ('$' | '`' | '"' | '\\')) => {
                                    raw.push(c);
                                    value.push(c);
                                }
                                Some('\n') => {
                                    // Backslash-newline: skip both (line continuation).
                                    raw.push('\n');
                                }
                                Some(c) => {
                                    raw.push(c);
                                    value.push('\\');
                                    value.push(c);
                                }
                                None => {}
                            }
                        }
                        Some(c) => {
                            raw.push(c);
                            value.push(c);
                        }
                    }
                }
            }
            '\\' => {
                raw.push(ch);
                if let Some(c) = chars.next() {
                    raw.push(c);
                    value.push(c);
                }
            }
            c if c.is_whitespace() => {
                if !value.is_empty() {
                    tokens.push(ShellToken {
                        raw: std::mem::take(&mut raw),
                        value: std::mem::take(&mut value),
                    });
                }
            }
            c => {
                raw.push(c);
                value.push(c);
            }
        }
    }
    if !value.is_empty() {
        tokens.push(ShellToken { raw, value });
    }
    tokens
}

/// Split a shell command string into sub-commands at `;`, `&&`, and `||`
/// boundaries, respecting quotes and backslash escapes so that a quoted
/// path containing those characters is not split.
pub(crate) fn split_commands(s: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                current.push(ch);
                loop {
                    match chars.next() {
                        Some('\'') => {
                            current.push('\'');
                            break;
                        }
                        Some(c) => current.push(c),
                        None => break,
                    }
                }
            }
            '"' => {
                current.push(ch);
                loop {
                    match chars.next() {
                        None => break,
                        Some('"') => {
                            current.push('"');
                            break;
                        }
                        Some('\\') => {
                            current.push('\\');
                            if let Some(c) = chars.next() {
                                current.push(c);
                            }
                        }
                        Some(c) => current.push(c),
                    }
                }
            }
            '\\' => {
                current.push(ch);
                if let Some(c) = chars.next() {
                    current.push(c);
                }
            }
            ';' => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    parts.push(trimmed);
                }
                current.clear();
            }
            '&' if chars.clone().next() == Some('&') => {
                chars.next();
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    parts.push(trimmed);
                }
                current.clear();
            }
            '|' if chars.clone().next() == Some('|') => {
                chars.next();
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    parts.push(trimmed);
                }
                current.clear();
            }
            c => current.push(c),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }
    parts
}

/// A heredoc redirection whose body has not yet been consumed.
#[derive(Debug, Default)]
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

/// Shell metacharacters that terminate a bare heredoc delimiter word.
fn is_delimiter_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')')
}

/// A stateful scanner that copies a bash command while removing heredoc
/// redirections and their bodies. Keeping the scanner as a type — rather than
/// one long free function — makes the shell-state transitions (quoting,
/// comment, expansion, heredoc, body) independently testable.
struct HeredocStripper<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    result: String,
    pending: Vec<Heredoc>,
    /// Whether the next `#` would start a comment (i.e. we are at a word start).
    word_start: bool,
}

impl<'a> HeredocStripper<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            chars: s.chars().peekable(),
            result: String::with_capacity(s.len()),
            pending: Vec::new(),
            word_start: true,
        }
    }

    fn strip(mut self) -> String {
        while let Some(ch) = self.chars.next() {
            self.step(ch);
        }
        self.result
    }

    fn step(&mut self, ch: char) {
        match ch {
            '\'' => {
                self.result.push(ch);
                self.copy_quoted_body('\'');
                self.word_start = false;
            }
            '"' => {
                self.result.push(ch);
                self.copy_quoted_body('"');
                self.word_start = false;
            }
            '\\' => {
                // Backslash escapes the next character, so `\<<` is not a
                // heredoc operator.
                self.copy_escaped(ch);
                self.word_start = false;
            }
            '#' if self.word_start => {
                self.copy_comment();
                self.word_start = true;
            }
            '$' => self.copy_dollar(),
            '(' => {
                if self.chars.peek().copied() == Some('(') {
                    // `((` arithmetic command: `<<` inside is a shift, not a
                    // heredoc.
                    self.result.push(ch);
                    self.copy_balanced('(', ')');
                    self.word_start = false;
                } else {
                    self.result.push(ch);
                    self.word_start = true;
                }
            }
            '<' => self.copy_less_than(),
            '\n' => {
                self.result.push(ch);
                if !self.pending.is_empty() {
                    self.skip_heredoc_bodies();
                }
                self.word_start = true;
            }
            c if c.is_whitespace() => {
                self.result.push(c);
                self.word_start = true;
            }
            c => {
                self.result.push(c);
                self.word_start = matches!(c, ';' | '&' | '|' | ')' | '>');
            }
        }
    }

    fn copy_escaped(&mut self, ch: char) {
        self.result.push(ch);
        if let Some(escaped) = self.chars.next() {
            self.result.push(escaped);
        }
    }

    /// Copy the remainder of a quoted span whose opening quote has already
    /// been written. Backslashes are literal in single quotes; in double
    /// quotes they escape the following character.
    fn copy_quoted_body(&mut self, quote: char) {
        loop {
            match self.chars.next() {
                None => break,
                Some(c) if c == quote => {
                    self.result.push(c);
                    break;
                }
                Some('\\') if quote == '"' => {
                    self.result.push('\\');
                    if let Some(escaped) = self.chars.next() {
                        self.result.push(escaped);
                    }
                }
                Some(c) => self.result.push(c),
            }
        }
    }

    /// Copy a balanced `open`..`close` region verbatim (no heredoc detection
    /// inside), skipping quoted spans so nested delimiters are not miscounted.
    /// A backslash escapes the following character, so an escaped `close` is
    /// copied without changing the nesting depth.
    fn copy_balanced(&mut self, open: char, close: char) {
        let mut depth = 1u32;
        while depth > 0 {
            match self.chars.next() {
                None => break,
                Some(c) if c == '\'' => {
                    self.result.push(c);
                    self.copy_quoted_body('\'');
                }
                Some(c) if c == '"' => {
                    self.result.push(c);
                    self.copy_quoted_body('"');
                }
                Some('\\') => self.copy_escaped('\\'),
                Some(c) if c == open => {
                    depth += 1;
                    self.result.push(c);
                }
                Some(c) if c == close => {
                    depth -= 1;
                    self.result.push(c);
                }
                Some(c) => self.result.push(c),
            }
        }
    }

    fn copy_comment(&mut self) {
        self.result.push('#');
        for c in self.chars.by_ref() {
            self.result.push(c);
            if c == '\n' {
                break;
            }
        }
    }

    fn copy_dollar(&mut self) {
        let mut probe = self.chars.clone();
        match probe.next() {
            // `$(` subshell and `$((` arithmetic: copy verbatim so a `<<`
            // inside (e.g. a bitwise shift) is not a heredoc.
            Some('(') => {
                self.result.push('$');
                self.result.push('(');
                self.chars = probe;
                self.copy_balanced('(', ')');
                self.word_start = false;
            }
            // `${...}` parameter expansion: `<<` inside is not a heredoc.
            Some('{') => {
                self.result.push('$');
                self.result.push('{');
                self.chars = probe;
                self.copy_balanced('{', '}');
                self.word_start = false;
            }
            _ => {
                self.result.push('$');
                self.word_start = false;
            }
        }
    }

    fn copy_less_than(&mut self) {
        if self.chars.peek().copied() != Some('<') {
            self.result.push('<');
            self.word_start = true;
            return;
        }

        // `<<<` is a here-string, not a heredoc: emit all three `<` and keep
        // scanning.
        let mut probe = self.chars.clone();
        probe.next(); // past the second `<`
        if probe.peek().copied() == Some('<') {
            self.result.push_str("<<<");
            self.chars = probe;
            self.chars.next(); // past the third `<`
            self.word_start = true;
            return;
        }

        // Heredoc: consume the second `<` and an optional `-` (which strips
        // leading tabs from the terminator line).
        self.chars.next();
        let mut heredoc = Heredoc::default();
        if self.chars.peek().copied() == Some('-') {
            self.chars.next();
            heredoc.strip_tabs = true;
        }
        while self.chars.peek().is_some_and(|&c| c == ' ' || c == '\t') {
            self.chars.next();
        }
        heredoc.delimiter = self.read_delimiter_word();
        self.result.push('_');
        self.pending.push(heredoc);
        self.word_start = false;
    }

    /// Read a heredoc delimiter word, applying bash quote removal so that
    /// `$'EOF'`, `EOF'x'`, `"EOF"`, `\EOF`, and bare `EOF` all resolve to
    /// the delimiter bash will actually compare against the terminator line.
    fn read_delimiter_word(&mut self) -> String {
        let mut value = String::new();
        loop {
            match self.chars.peek().copied() {
                None => break,
                Some(c) if is_delimiter_boundary(c) => break,
                Some('\'') => {
                    self.chars.next();
                    loop {
                        match self.chars.next() {
                            None => break,
                            Some('\'') => break,
                            Some(c) => value.push(c),
                        }
                    }
                }
                Some('"') => {
                    self.chars.next();
                    loop {
                        match self.chars.next() {
                            None => break,
                            Some('"') => break,
                            Some('\\') => match self.chars.next() {
                                Some(c @ ('$' | '`' | '"' | '\\')) => value.push(c),
                                Some(c) => {
                                    value.push('\\');
                                    value.push(c);
                                }
                                None => {}
                            },
                            Some(c) => value.push(c),
                        }
                    }
                }
                Some('$') => {
                    let mut probe = self.chars.clone();
                    probe.next(); // `$`
                    if probe.peek().copied() == Some('\'') {
                        // ANSI-C quoting `$'...'`.
                        self.chars = probe;
                        self.chars.next(); // opening `'`
                        loop {
                            match self.chars.next() {
                                None => break,
                                Some('\'') => break,
                                Some('\\') => match self.chars.next() {
                                    Some('n') => value.push('\n'),
                                    Some('t') => value.push('\t'),
                                    Some('r') => value.push('\r'),
                                    Some('\\') => value.push('\\'),
                                    Some('\'') => value.push('\''),
                                    Some('"') => value.push('"'),
                                    Some(c) => value.push(c),
                                    None => {}
                                },
                                Some(c) => value.push(c),
                            }
                        }
                    } else {
                        self.chars.next();
                        value.push('$');
                    }
                }
                Some('\\') => {
                    self.chars.next();
                    if let Some(c) = self.chars.next() {
                        value.push(c);
                    }
                }
                Some(_) => value.push(self.chars.next().unwrap()),
            }
        }
        value
    }

    /// Drop body lines for each pending heredoc, in order, up to and
    /// including the terminator line. Emits a single newline so a following
    /// command is not glued onto the heredoc's header line.
    fn skip_heredoc_bodies(&mut self) {
        for heredoc in self.pending.drain(..) {
            loop {
                let mut line = String::new();
                loop {
                    match self.chars.next() {
                        None => return, // no terminator: drop to end of input
                        Some('\n') => break,
                        Some(c) => line.push(c),
                    }
                }
                if line.ends_with('\r') {
                    line.pop();
                }
                let candidate = if heredoc.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if candidate == heredoc.delimiter {
                    break;
                }
            }
        }
        self.result.push('\n');
    }
}

/// Strip heredoc redirections and their bodies from a command string.
///
/// A heredoc body is file *content*, not a path argument the agent is passing
/// to the shell, so `bash_check` must never tokenize it. Each `<<`/`<<-`
/// redirection plus its body is replaced with the same inert `_` placeholder
/// used by [`strip_subshells`], while the rest of the redirection's first line
/// (such as `> /tmp/out` in `cat <<EOF > /tmp/out`) passes through unchanged.
pub(crate) fn strip_heredocs(s: &str) -> String {
    HeredocStripper::new(s).strip()
}

/// Strip `$(...)` and backtick command substitutions from a command string.
/// Paths inside subshells are runtime-expanded by the shell, not literal
/// arguments from the agent, so they must not be checked by `bash_check`.
pub(crate) fn strip_subshells(s: &str) -> String {
    /// Skip a quoted region: returns when the matching close-quote is found.
    fn skip_quoted(chars: &mut std::str::Chars, quote: char) {
        loop {
            match chars.next() {
                None => break,
                Some(c) if c == quote => break,
                Some('\\') => {
                    chars.next();
                }
                _ => {}
            }
        }
    }

    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '$' => {
                // Peek ahead for `$(` — if found, skip the entire subshell.
                let mut peek = chars.clone();
                if peek.next() == Some('(') {
                    chars = peek;
                    let mut depth = 1u32;
                    while depth > 0 {
                        match chars.next() {
                            None => break,
                            Some(c) if c == '\'' || c == '"' => skip_quoted(&mut chars, c),
                            Some('(') => depth += 1,
                            Some(')') => depth -= 1,
                            Some('\\') => {
                                chars.next();
                            }
                            _ => {}
                        }
                    }
                    result.push('_'); // placeholder — never path-like
                    continue;
                }
                result.push(ch);
            }
            '`' => {
                // Consume until matching backtick, skipping quoted regions.
                loop {
                    match chars.next() {
                        None => break,
                        Some('`') => break,
                        Some(c) if c == '\'' || c == '"' => skip_quoted(&mut chars, c),
                        Some('\\') => {
                            chars.next();
                        }
                        _ => {}
                    }
                }
                result.push('_');
            }
            _ => result.push(ch),
        }
    }
    result
}
