//! Dynamic-context injection for skill bodies.
//!
//! Skill bodies may embed `` !`command` `` inline spans or fenced
//! ` ```! ... ``` ` blocks. At load time these commands run once via
//! `bash -c` in the project root and their stdout is inlined into the
//! rendered body before the model sees it.

use std::path::Path;

use shlex;

use super::SkillError;

/// Timeout for a single `!` dynamic-context command.
const INJECTION_TIMEOUT_SECS: f64 = 120.0;

/// Tools whose exit code 1 is a normal result rather than a failure.
///
/// `grep`, `rg`, and `ag` exit 1 when there are no matches; `diff` exits 1
/// when the inputs differ (and stdout carries the diff). `find` is *not*
/// here: exit 1 is a real error for `find`, whose "no matches" case already
/// exits 0.
fn exit_one_is_expected(script: &str) -> bool {
    matches!(
        status_command_word(script).as_deref(),
        Some("grep" | "diff" | "rg" | "ag")
    )
}

/// The shell word that sets the exit status of `script`.
///
/// For a single simple command this is its command name. For a list of
/// commands (`a; b`, `a && b`, pipelines, newline-separated scripts) the exit
/// status is that of the last command, so the scanner splits on top-level
/// control operators and inspects the final non-empty segment.
fn status_command_word(script: &str) -> Option<String> {
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = script.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            current.push(c);
            if c == q {
                quote = None;
            } else if q == '"' && c == '\\' {
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                }
            }
            continue;
        }

        match c {
            '\'' | '"' => {
                quote = Some(c);
                current.push(c);
            }
            '\\' => {
                current.push(c);
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                }
            }
            '\n' | ';' => push_segment(&mut segments, &mut current),
            '|' => {
                push_segment(&mut segments, &mut current);
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
            }
            '&' if chars.peek() == Some(&'&') => {
                push_segment(&mut segments, &mut current);
                chars.next();
            }
            _ => current.push(c),
        }
    }
    push_segment(&mut segments, &mut current);

    let last = segments.iter().rev().find(|s| !s.trim().is_empty())?;
    first_command_word(last.trim())
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
    current.clear();
}

/// Extract the first command word, skipping leading shell variable
/// assignments (`FOO=bar`) and `env` with its options (`-i`, `--unset NAME`,
/// `--unset=NAME`, `--`).
fn first_command_word(command: &str) -> Option<String> {
    let tokens = shlex::split(command)
        .unwrap_or_else(|| command.split_whitespace().map(str::to_string).collect());
    let mut tokens = tokens.into_iter().peekable();

    while matches!(tokens.peek().map(String::as_str), Some(t) if is_assignment(t)) {
        tokens.next();
    }

    if tokens.peek().map(String::as_str) == Some("env") {
        tokens.next();
        while let Some(token) = tokens.peek() {
            if is_assignment(token)
                || matches!(token.as_str(), "-i" | "--ignore-environment" | "--")
                || token.starts_with("--unset=")
            {
                tokens.next();
            } else if token == "-u" || token == "--unset" {
                tokens.next(); // flag
                tokens.next(); // NAME
            } else {
                break;
            }
        }
    }

    tokens.next()
}

fn is_assignment(token: &str) -> bool {
    let Some(eq) = token.find('=') else {
        return false;
    };
    if eq == 0 {
        return false;
    }
    let name = &token[..eq];
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Strip trailing `\r`/`\n` from captured stdout before it is inlined.
fn trim_trailing_newlines(s: &str) -> &str {
    s.trim_end_matches(['\r', '\n'])
}

/// Run a single dynamic-context command with `bash -c` in the project root.
///
/// Returns the command's stdout with trailing newlines stripped.
fn run_injection_command(script: &str, project_root: &Path) -> Result<String, SkillError> {
    if script.trim().is_empty() {
        return Ok(String::new());
    }

    let argv = vec!["bash".to_string(), "-c".to_string(), script.to_string()];
    let result = crate::core::proc::run(
        &argv,
        Some(project_root),
        false,
        Some(INJECTION_TIMEOUT_SECS),
    );

    match result {
        Ok(proc) if proc.returncode == 0 => {
            Ok(trim_trailing_newlines(&String::from_utf8_lossy(&proc.stdout)).to_string())
        }
        Ok(proc) => {
            if proc.returncode == 1 && exit_one_is_expected(script) {
                return Ok(
                    trim_trailing_newlines(&String::from_utf8_lossy(&proc.stdout)).to_string(),
                );
            }
            Err(SkillError::InjectionFailed {
                command: script.to_string(),
                output: crate::core::proc::combine_output(&proc.stdout, &proc.stderr),
            })
        }
        Err(e) => Err(SkillError::InjectionFailed {
            command: script.to_string(),
            output: e.full_message(),
        }),
    }
}

struct FenceBlock {
    /// Info string from the opener (the text after the backtick run).
    info: String,
    /// Byte offset of the first content byte (just past the opener's newline).
    content_start: usize,
    /// Byte offset of the start of the closing fence line.
    content_end: usize,
    /// Byte offset of the closing fence line start; its trailing newline is
    /// left for the scanner.
    end: usize,
    /// Byte offset just past the closing fence line's trailing newline.
    block_end: usize,
}

/// Try to match a fenced code block starting at `line_start`.
///
/// Returns `None` unless `line_start` is at the start of a line whose first
/// non-space token is a backtick fence of at least three backticks.
fn find_fence(body: &str, line_start: usize) -> Option<FenceBlock> {
    let line = &body[line_start..];
    let bytes = line.as_bytes();
    let mut idx = 0;

    while idx < bytes.len() && (bytes[idx] == b' ' || bytes[idx] == b'\t') {
        idx += 1;
    }
    let mut fence_len = 0usize;
    while idx < bytes.len() && bytes[idx] == b'`' {
        fence_len += 1;
        idx += 1;
    }
    if fence_len < 3 {
        return None;
    }

    let line_end = line.find('\n').unwrap_or(line.len());
    let info = line[idx..line_end].trim().to_string();

    // Content begins after the opener's newline (or immediately if the opener
    // is the last line, which has no script to run).
    let content_start = if line_end < line.len() {
        line_start + line_end + 1
    } else {
        line_start + line.len()
    };

    // Find the matching closing fence line.
    let mut cursor = content_start;
    while cursor <= body.len() {
        let cursor_line_end = body[cursor..]
            .find('\n')
            .map(|off| cursor + off)
            .unwrap_or(body.len());
        let candidate = &body[cursor..cursor_line_end];
        if is_closing_fence(candidate, fence_len) {
            let block_end = if cursor_line_end < body.len() {
                cursor_line_end + 1
            } else {
                body.len()
            };
            return Some(FenceBlock {
                info,
                content_start,
                content_end: cursor,
                end: cursor_line_end,
                block_end,
            });
        }
        if cursor_line_end == body.len() {
            break;
        }
        cursor = cursor_line_end + 1;
    }
    None
}

/// A closing fence is a line consisting of at least `fence_len` backticks
/// (after optional leading whitespace), followed only by whitespace.
fn is_closing_fence(line: &str, fence_len: usize) -> bool {
    let bytes = line.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() && (bytes[idx] == b' ' || bytes[idx] == b'\t') {
        idx += 1;
    }
    let mut count = 0usize;
    while idx < bytes.len() && bytes[idx] == b'`' {
        count += 1;
        idx += 1;
    }
    count >= fence_len && bytes[idx..].iter().all(|b| *b == b' ' || *b == b'\t')
}

/// Try to match an inline `!` code-span at `bang`, returning the script text
/// and the byte offset just past the closing backtick run.
fn find_inline_span(body: &str, bang: usize) -> Option<(&str, usize)> {
    let after = &body[bang + 1..];
    let bytes = after.as_bytes();
    let mut idx = 0;
    let mut delimiter_len = 0usize;
    while idx < bytes.len() && bytes[idx] == b'`' {
        delimiter_len += 1;
        idx += 1;
    }
    if delimiter_len == 0 {
        return None;
    }

    let delimiter = "`".repeat(delimiter_len);
    let script_start = bang + 1 + delimiter_len;
    let close_off = body[script_start..].find(&delimiter)?;
    let script_end = script_start + close_off;
    Some((&body[script_start..script_end], script_end + delimiter_len))
}

/// Try to match an ordinary inline code span at `start`, returning the byte
/// offset just past the closing backtick run.
fn find_code_span(body: &str, start: usize) -> Option<usize> {
    let rest = &body[start..];
    let bytes = rest.as_bytes();
    let mut idx = 0;
    let mut delimiter_len = 0usize;
    while idx < bytes.len() && bytes[idx] == b'`' {
        delimiter_len += 1;
        idx += 1;
    }
    if delimiter_len == 0 {
        return None;
    }

    let delimiter = "`".repeat(delimiter_len);
    let close_off = rest[idx..].find(&delimiter)?;
    Some(start + idx + close_off + delimiter_len)
}

/// Scan a skill body for inline `` !`command` `` and fenced ` ```! ... ``` `
/// placeholders, run each command in the project root, and inline the
/// captured stdout (with trailing newlines stripped).
///
/// Ordinary fenced code blocks and inline code spans are copied verbatim so
/// examples that merely document the placeholder syntax are not executed.
///
/// Any command that fails (outside the documented exit-code-1 exceptions)
/// aborts the entire invocation — no partial injection is returned.
pub(crate) fn inject_dynamic_context(
    body: &str,
    project_root: &Path,
) -> Result<String, SkillError> {
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    let bytes = body.as_bytes();

    while i < body.len() {
        let at_line_start = i == 0 || bytes[i - 1] == b'\n';
        if at_line_start {
            if let Some(fence) = find_fence(body, i) {
                if fence.info == "!" {
                    let script = &body[fence.content_start..fence.content_end];
                    out.push_str(&run_injection_command(script, project_root)?);
                    i = fence.end;
                } else {
                    out.push_str(&body[i..fence.block_end]);
                    i = fence.block_end;
                }
                continue;
            }
        }

        if bytes[i] == b'!' {
            if let Some((script, end)) = find_inline_span(body, i) {
                out.push_str(&run_injection_command(script, project_root)?);
                i = end;
                continue;
            }
        }

        // Ordinary inline code span: keep it verbatim. Real placeholders are
        // consumed above, so only documentation-style spans reach this point.
        if bytes[i] == b'`' {
            if let Some(end) = find_code_span(body, i) {
                out.push_str(&body[i..end]);
                i = end;
                continue;
            }
        }

        let ch = body[i..].chars().next().expect("valid UTF-8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_command_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "Result: !`echo hello`";
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert_eq!(out, "Result: hello");
    }

    #[test]
    fn failed_command_aborts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = inject_dynamic_context("!`exit 2`", tmp.path());
        match result {
            Err(SkillError::InjectionFailed { command, .. }) => {
                assert!(command.contains("exit 2"));
            }
            other => panic!("expected InjectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn grep_exit_one_is_empty_result() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "pre !`grep needle /dev/null` post";
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert_eq!(out, "pre  post");
    }

    #[test]
    fn diff_exit_one_inlines_stdout() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "b\n").unwrap();
        let body = "!`diff a.txt b.txt`";
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert!(out.contains("< a"), "unexpected diff output: {out:?}");
        assert!(out.contains("> b"), "unexpected diff output: {out:?}");
    }

    #[test]
    fn find_exit_one_is_injection_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = inject_dynamic_context("!`find /nonexistent`", tmp.path());
        match result {
            Err(SkillError::InjectionFailed { command, .. }) => {
                assert!(command.contains("find /nonexistent"));
            }
            other => panic!("expected InjectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn fenced_block_runs_as_single_script() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "before\n```!\necho one\necho two\n```\nafter";
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert_eq!(out, "before\none\ntwo\nafter");
    }

    #[test]
    fn fenced_block_ending_in_grep_preserves_earlier_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "before\n```!\necho one\ngrep needle /dev/null\n```\nafter";
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert_eq!(out, "before\none\nafter");
    }

    #[test]
    fn body_without_commands_is_identity() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "Just a body with no placeholders.\nStill $ARGUMENTS here.";
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn or_true_suppresses_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "!`false || true`";
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn trailing_newlines_are_stripped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = r#"!`printf 'hi\n\n\n'`"#;
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert_eq!(out, "hi");
    }

    #[test]
    fn ordinary_fenced_block_is_not_executed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let body = "Example:\n```\n!`echo nope`\n```\nafter";
        let out = inject_dynamic_context(body, tmp.path()).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn closing_fence_requires_whitespace_after_backticks() {
        assert!(is_closing_fence("```", 3));
        assert!(is_closing_fence("   ```   ", 3));
        assert!(!is_closing_fence("```bash", 3));
    }

    #[test]
    fn first_command_word_skips_env_flags_and_assignments() {
        assert_eq!(first_command_word("grep x y").as_deref(), Some("grep"));
        assert_eq!(
            first_command_word("FOO=bar grep x y").as_deref(),
            Some("grep")
        );
        assert_eq!(
            first_command_word("env -i FOO=bar grep x").as_deref(),
            Some("grep")
        );
        assert_eq!(
            first_command_word("env --unset=FOO grep x").as_deref(),
            Some("grep")
        );
        assert_eq!(first_command_word("ls -i").as_deref(), Some("ls"));
    }

    #[test]
    fn exit_one_expected_uses_status_command_word() {
        assert!(exit_one_is_expected("echo one\ngrep needle /dev/null"));
        assert!(!exit_one_is_expected("grep needle /dev/null\necho one"));
        assert!(!exit_one_is_expected("find /nonexistent"));
        assert!(exit_one_is_expected("diff a.txt b.txt"));
    }
}
