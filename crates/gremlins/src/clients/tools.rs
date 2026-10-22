use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use rig_core::completion::ToolDefinition;
use tokio::process::Command;

use super::agent_loop::format_tool_call_parts;
use super::interactive::StreamEventSender;
use super::task_kind::TaskKind;
use crate::landscape::{LandscapeError, LandscapeExplorer};

const GREP_MAX_LINES: usize = 2000;
const BASH_TIMEOUT_SECS: u64 = 120;
const WEBSEARCH_TIMEOUT_SECS: u64 = 30;
/// Cap on Task calls spawned from a single model message. Without this, one
/// response could fan out unbounded child loops and overflow the context.
pub(crate) const TASK_MAX_PER_TURN: usize = 8;
const SKIP_DIRS: &[&str] = &["__pycache__", "node_modules", "target"];

/// Tools that stay available even when a tool filter is set. `task_fn`
/// re-applies both the tool filter and the sandbox mode to every nested agent,
/// so bypassing the filter here cannot escape containment.
const ALWAYS_AVAILABLE: &[&str] = &["Task", "Done", "Mischief"];

fn always_available(name: &str) -> bool {
    ALWAYS_AVAILABLE.contains(&name)
}

/// First `max_chars` characters of a string, with \n escaped to `\\n`.
pub(crate) fn preview_str(s: &str, max_chars: usize) -> String {
    let truncated: String = s.chars().take(max_chars).collect();
    let escaped = truncated.replace('\n', "\\n").replace('\r', "\\r");
    if s.chars().count() > max_chars {
        format!("{escaped}…")
    } else {
        escaped
    }
}

type TaskFuture = Pin<Box<dyn std::future::Future<Output = String> + Send>>;

/// Callback that `invoke` calls for Task tool invocations.
pub(crate) type TaskFn = Arc<
    dyn Fn(TaskKind, String, Option<Vec<rig_core::completion::Message>>) -> TaskFuture
        + Send
        + Sync,
>;

/// How tool-path sandboxing is applied for a stage's agent loop.
///
/// `Roots` is the default: every path is canonicalized and must fall under one
/// of the listed roots (empty roots deny everything). `AllowAll` disables
/// containment entirely — every check short-circuits to "allowed" without
/// canonicalizing or comparing paths. It is a hidden, programmatic-only mode
/// (not exposed in the YAML schema) used by the chat gremlin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SandboxMode {
    Roots(Vec<PathBuf>),
    AllowAll,
}

impl SandboxMode {
    /// The containment roots, or `None` when containment is disabled.
    pub(crate) fn roots(&self) -> Option<&[PathBuf]> {
        match self {
            SandboxMode::Roots(roots) => Some(roots),
            SandboxMode::AllowAll => None,
        }
    }
}

/// One live tool-output chunk streamed from a running tool into the TUI's
/// embedded tool slot. Display-only; never reaches the model or the
/// permanent transcript.
#[derive(Clone, Debug)]
pub(crate) struct ToolOutputLine {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) call: String,
    pub(crate) text: String,
}

/// Bundled send-side telemetry channels for the agent loop: stream events
/// (model deltas, tool results, turn-complete events) and live tool-output
/// line forwarding (`Bash`/`WebSearch`).
///
/// Cloned cheaply — every nested loop inherits the parent's channels.
#[derive(Clone)]
pub(crate) struct AgentLoopTelemetry {
    pub(crate) stream_events: Option<StreamEventSender>,
    pub(crate) tool_output: Option<tokio::sync::mpsc::Sender<ToolOutputLine>>,
}

#[derive(Clone)]
pub(crate) struct AgentLoopRuntime {
    pub cwd: Option<PathBuf>,
    pub extra_env: Option<HashMap<String, String>>,
    pub(crate) base_env: Option<HashMap<String, String>>,
    pub(crate) sandbox: SandboxMode,
    pub(crate) audit_log: Option<PathBuf>,
    pub(crate) allowed_tools: Option<Vec<String>>,
    pub(crate) task_fn: Option<TaskFn>,
    pub(crate) audit_lock: Option<Arc<std::sync::Mutex<()>>>,
    /// Point-in-time snapshot of the parent conversation, available to Task
    /// tool calls (used only by `Fork`). `None` when unavailable.
    pub(crate) parent_history: Option<Vec<rig_core::completion::Message>>,
    /// Prefix prepended to tool call IDs emitted on the shared event stream.
    ///
    /// Nested Task conversations run on the parent's stream but reuse raw
    /// provider call IDs (`call_0`, …). Two independent tasks would then share
    /// one TUI live slot and the second result would be dropped, so each nested
    /// loop sets its own task-chain prefix to keep IDs unique. Empty at the top
    /// level, so top-level IDs are unchanged.
    pub(crate) id_prefix: String,
    /// Bundled send-side telemetry channels. Cloned into every nested loop.
    pub(crate) telemetry: AgentLoopTelemetry,
}

impl AgentLoopRuntime {
    /// Send one completed live-output line for the tool call identified by
    /// `id` to the optional display channel. Tool results returned to the
    /// model are unaffected.
    ///
    /// Awaits channel capacity so a slow live-output forwarder throttles the
    /// producing read loop rather than overflowing the buffer and silently
    /// dropping chunks from the live tool stream.
    pub(crate) async fn emit_output(&self, id: &str, name: &str, call: &str, text: &str) {
        if let Some(tx) = &self.telemetry.tool_output {
            let _ = tx
                .send(ToolOutputLine {
                    id: format!("{}{id}", self.id_prefix),
                    name: name.to_string(),
                    call: call.to_string(),
                    text: text.to_string(),
                })
                .await;
        }
    }
}

pub(crate) fn project_root() -> PathBuf {
    crate::config::project_root()
}

pub(crate) fn worktree_root(cwd: Option<&Path>) -> PathBuf {
    match cwd {
        Some(cwd) => canonicalize_or_ancestor(cwd),
        None => canonicalize_or_ancestor(&project_root()),
    }
}

pub(crate) fn scratch_root() -> Option<PathBuf> {
    crate::config::scratch_dir(None)
}

pub(crate) fn audit_log_path(raw_path: &Path) -> PathBuf {
    let stem = raw_path.file_stem().unwrap_or_default();
    let name = format!("{}.audit.jsonl", stem.to_string_lossy());
    match raw_path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

pub(crate) fn resolve(file_path: &str, cwd: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(file_path);
    if p.is_absolute() {
        return p;
    }
    let base = match cwd {
        Some(cwd) => canonicalize_or_ancestor(cwd),
        None => canonicalize_or_ancestor(&project_root()),
    };
    base.join(p)
}

fn normalize_dots(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Resolve the real path by canonicalizing the deepest ancestor that exists
/// on disk, then re-appending the non-existent suffix. Falls back to the
/// original path if even the filesystem root can't be canonicalized.
pub(crate) fn canonicalize_or_ancestor(p: &Path) -> PathBuf {
    match p.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            let mut ancestor = p;
            loop {
                match ancestor.canonicalize() {
                    Ok(c) => {
                        let suffix = p.strip_prefix(ancestor).unwrap_or(p);
                        // Resolve dangling symlinks in the suffix.
                        // canonicalize() walks past symlinks whose target
                        // doesn't exist yet. A dangling symlink pointing
                        // outside would pass containment as an ordinary
                        // name. Check each suffix prefix; if we find a
                        // symlink, resolve it and recurse so the real
                        // target is seen by the containment check.
                        let components: Vec<_> = suffix.components().collect();
                        let mut probe = c.clone();
                        for (i, comp) in components.iter().enumerate() {
                            probe = probe.join(comp);
                            if probe.is_symlink() {
                                if let Ok(target) = std::fs::read_link(&probe) {
                                    let mut resolved = if target.is_absolute() {
                                        target
                                    } else {
                                        probe.parent().unwrap().join(&target)
                                    };
                                    for comp in &components[i + 1..] {
                                        resolved = resolved.join(comp);
                                    }
                                    return canonicalize_or_ancestor(&resolved);
                                }
                                break;
                            }
                        }
                        return c.join(suffix);
                    }
                    Err(_) => match ancestor.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => ancestor = parent,
                        _ => return p.to_path_buf(),
                    },
                }
            }
        }
    }
}

fn normalize_path(p: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        let base = match cwd {
            Some(cwd) => canonicalize_or_ancestor(cwd),
            None => canonicalize_or_ancestor(&project_root()),
        };
        base.join(p)
    };
    let canonical = canonicalize_or_ancestor(&abs);
    // normalize_dots handles `.`/`..` in the non-existent suffix only —
    // canonicalize_or_ancestor already resolved them in the existing prefix.
    // Lexical `..` in a not-yet-existent suffix is the safe conservative choice.
    Some(normalize_dots(&canonical))
}

pub(crate) fn within_worktree(p: &Path, roots: &[PathBuf], cwd: Option<&Path>) -> bool {
    let p_norm = match normalize_path(p, cwd) {
        Some(n) => n,
        None => return false,
    };
    roots.iter().any(|root| match normalize_path(root, cwd) {
        Some(r) => p_norm.starts_with(&r),
        None => false,
    })
}

fn roots_suffix(roots: &[PathBuf], cwd: Option<&Path>) -> String {
    // Normalized, to match the paths the containment checks actually compare
    // against — raw roots read misleadingly under symlinked ancestors
    // (/var -> /private/var).
    let mut parts: Vec<String> = roots
        .iter()
        .filter_map(|r| normalize_path(r, cwd))
        .map(|r| r.display().to_string())
        .collect();
    parts.sort();
    parts.dedup();
    if parts.is_empty() {
        return String::new();
    }
    format!("\n→ sandbox roots: {}", parts.join(", "))
}

/// Lexical-leaf containment for [`bash_check`] ONLY.
///
/// Resolves symlinked ancestor *directories* (so `/var` -> `/private/var` and
/// any symlinked parent dir are judged by their real location) but does NOT
/// follow a final-component symlink. A venv interpreter is created as
/// `.venv/bin/python -> /opt/homebrew/.../python3.14`: the symlink node lives
/// inside the worktree even though its target does not. Executing it is not a
/// containment breach — bash can already run out-of-tree binaries via PATH
/// (git, cargo, ruff, make) and read/write anywhere via `python -c`. Likewise,
/// this permits leaf-symlink *reads* through bash (e.g. `cat somelink` where
/// `somelink -> /etc/passwd`) that `bash_check` previously denied. This is
/// acceptable because the real
/// data boundary is [`io_enforce`] on the Read/Write/Edit tools, which stays
/// fully canonical. This check is a usability guardrail: an early, clear denial
/// for obviously-stray path arguments, not a sandbox.
///
/// DO NOT "harden" this to canonicalize the final component. Doing so re-breaks
/// agent self-verify (`.venv/bin/python` becomes "path outside worktree") — the
/// exact regression from #1197 that removed `enforce_allows_venv_symlink_in_cwd`
/// — while adding zero real containment, because execution is not gated here.
/// See `bash_check_allows_venv_symlink_leaf`.
fn within_worktree_lexical(p: &Path, canonical_roots: &[PathBuf]) -> bool {
    let resolved = match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => canonicalize_or_ancestor(parent).join(name),
        _ => canonicalize_or_ancestor(p),
    };
    let norm = normalize_dots(&resolved);
    canonical_roots.iter().any(|cr| norm.starts_with(cr))
}

fn expand_user(s: &str) -> String {
    if s == "~" || s.starts_with("~/") {
        let home = crate::config::home_dir();
        let home_str = home.to_string_lossy();
        // If home resolved to the sentinel fallback (no HOME set, no
        // platform dir), preserve ~ literally rather than expanding to ".".
        if home_str.is_empty() || home_str == "." {
            return s.to_string();
        }
        if s == "~" {
            return home_str.into_owned();
        }
        return format!("{home_str}{}", &s[1..]);
    }
    s.to_string()
}

pub(crate) fn enforce(roots: &[PathBuf], pth: &str, cwd: Option<&Path>) -> Option<String> {
    let p = resolve(pth, cwd);
    if !within_worktree(&p, roots, cwd) {
        return Some(format!(
            "Error: path outside sandbox: {pth}{}",
            roots_suffix(roots, cwd)
        ));
    }
    None
}

/// Symlink-aware containment check for file I/O. Uses
/// [`canonicalize_or_ancestor`] to resolve the real path, including dangling
/// symlinks that would otherwise escape containment. Returns Some(error) or
/// None if the path is safe.
///
/// Fail-closed by design: a root that cannot be fully canonicalized is
/// partially resolved via [`canonicalize_or_ancestor`] (existing prefix
/// canonicalized, non-existent suffix kept lexical) rather than being
/// dropped. If the path does not fall under any root `allowed` stays false
/// and the path is denied. This is stricter and safer than the old
/// single-root behavior, which returned `None` (allow) on canonicalization
/// failure — a "paranoid, shouldn't happen" branch that was effectively
/// fail-open.
pub(crate) fn io_enforce(path: &Path, roots: &[PathBuf]) -> Option<String> {
    let real = normalize_dots(&canonicalize_or_ancestor(path));
    let mut allowed = false;
    for root in roots {
        let real_root = normalize_dots(&canonicalize_or_ancestor(root));
        if real.starts_with(&real_root) {
            allowed = true;
            break;
        }
    }
    if !allowed {
        return Some(format!(
            "Error: path outside sandbox (resolved): {}{}",
            real.display(),
            roots_suffix(roots, None)
        ));
    }
    None
}

/// Returns Some(error) if `cmd` is an `ln` invocation that requests
/// symbolic linking. Token-based: inspects argv[0] for `ln` and scans
/// subsequent tokens for `-s*`, `--symbolic`, or flags containing `s`.
///
/// Defense-in-depth only. `io_enforce` is the real containment boundary:
/// this misses `env ln -s`, `command ln -s`, `sh -c 'ln -s'`, and any `ln`
/// after `;`/`&&`/`|`, since it only inspects argv[0]. It exists to give the
/// model a clear early denial for the common `ln -s` case, not to be airtight.
fn check_ln_symlink(cmd: &str) -> Option<String> {
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
struct ShellToken {
    /// The raw verbatim text as it appeared in the command string,
    /// including any surrounding quotes and escape characters.
    raw: String,
    /// The resolved text with quotes stripped and escapes processed,
    /// as a POSIX shell would interpret it.
    value: String,
}

/// Shell-aware tokenizer: splits a command string into argument tokens
/// as a POSIX shell would, handling backslash escaping, single quotes,
/// and double quotes. Escapes are resolved and quotes are stripped.
///
/// This is not a full shell parser — it does not handle `$()` expansion,
/// `;`/`&&`/`||` command separators, or IFS splitting. It is designed for
/// the sandbox path checker which only needs to isolate individual path
/// arguments from the command string.
fn shell_tokenize(s: &str) -> Vec<ShellToken> {
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
fn split_commands(s: &str) -> Vec<String> {
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
fn strip_heredocs(s: &str) -> String {
    HeredocStripper::new(s).strip()
}

/// Strip `$(...)` and backtick command substitutions from a command string.
/// Paths inside subshells are runtime-expanded by the shell, not literal
/// arguments from the agent, so they must not be checked by `bash_check`.
fn strip_subshells(s: &str) -> String {
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

/// Pre-flight static analysis for Bash commands. Rejects commands that
/// reference paths outside the sandbox roots.
///
/// Splits compound commands on `;`, `&&`, and `||`, then processes each
/// sub-command in order while tracking a simulated working directory
/// (`sim_cwd`). When a sub-command is `cd <arg>`, `sim_cwd` is updated so
/// that subsequent relative paths are resolved against the directory the
/// shell would actually be in; `cd` only affects *later* sub-commands, so
/// the operand of the `cd` itself still resolves from the pre-`cd` directory.
///
/// This is a heuristic, not shell emulation. `cd -`, bare `cd`, and `cd`
/// with a variable/substitution target are not tracked. The real containment
/// boundary is `io_enforce`; this check provides early, clear denials for
/// obviously stray path arguments.
pub(crate) fn bash_check(roots: &[PathBuf], cmd: &str, cwd: Option<&Path>) -> Option<String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    // Guard against symlink creation via ln.
    if let Some(err) = check_ln_symlink(cmd) {
        return Some(err);
    }
    // Strip heredoc bodies and subshells up front. Neither contains literal
    // path arguments from the agent: heredoc bodies are file content, and
    // subshell paths are runtime-expanded by the shell. Stripping first keeps
    // `;`, `&&`, and `||` inside those regions from being treated as
    // sub-command boundaries.
    let cleaned = strip_subshells(&strip_heredocs(cmd));
    // Canonicalize roots once — avoid re-resolving per token.
    let canonical_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| normalize_path(r, cwd))
        .collect();
    if canonical_roots.is_empty() {
        return Some("Error: invalid sandbox root".into());
    }
    // Simulated working directory — updated when we see a resolvable cd.
    // Canonicalized to match `resolve`, which canonicalizes the base before
    // joining every relative path.
    let mut sim_cwd = match cwd {
        Some(c) => canonicalize_or_ancestor(c),
        None => canonicalize_or_ancestor(&project_root()),
    };

    let sub_commands = split_commands(&cleaned);
    for sub_cmd in &sub_commands {
        let tokens = shell_tokenize(sub_cmd);
        if tokens.is_empty() {
            continue;
        }

        // Check all path tokens against the *current* sim_cwd. A `cd` in this
        // sub-command only changes the directory for subsequent commands, so
        // its operand (and any redirection on the same line) resolves from the
        // pre-`cd` directory. Resolving it against the updated directory would
        // falsely deny `cd ..` from a child of the sandbox root.
        for tok in &tokens {
            if tok.value.is_empty() {
                continue;
            }
            let looks_like_path = tok.value.starts_with('/')
                || tok.value.starts_with('~')
                || tok.value.starts_with("..")
                || tok.value.contains('/');
            if !looks_like_path {
                continue;
            }
            let expanded = if tok.value.starts_with('~') {
                expand_user(&tok.value)
            } else {
                tok.value.clone()
            };
            let p = resolve(&expanded, Some(&sim_cwd));
            // Lexical-leaf, not canonical: a final-component symlink whose node is
            // in-worktree (e.g. `.venv/bin/python`) is allowed. See
            // within_worktree_lexical for the rationale and the anti-regression note.
            if !within_worktree_lexical(&p, &canonical_roots) {
                return Some(format!(
                    "Error: path outside sandbox (from {}): {}{}",
                    tok.raw,
                    tok.value,
                    roots_suffix(roots, Some(&sim_cwd))
                ));
            }
        }

        // Check for a `cd` command — update sim_cwd for subsequent sub-commands.
        let first = &tokens[0];
        if (first.value == "cd" || first.value.ends_with("/cd")) && tokens.len() >= 2 {
            let arg = &tokens[1];
            // cd with no arg (goes HOME), cd - (previous dir), or a
            // variable/substitution target ($DIR, `...`, $(...)) — can't
            // statically resolve, skip without touching sim_cwd.
            let unresolvable =
                arg.value == "-" || arg.value.contains('$') || arg.value.contains('`');
            if !unresolvable {
                let expanded = expand_user(&arg.value);
                let p = resolve(&expanded, Some(&sim_cwd));
                // Validate cd target with canonical containment.
                if !within_worktree(&p, roots, Some(&sim_cwd)) {
                    return Some(format!(
                        "Error: cd target outside sandbox: {}{}",
                        arg.value,
                        roots_suffix(roots, Some(&sim_cwd))
                    ));
                }
                // Update sim_cwd to the fully-normalized target.
                if let Some(normalized) = normalize_path(&p, None) {
                    sim_cwd = normalized;
                }
            }
        }
    }
    None
}

fn audit_key_arg(args_json: &str) -> String {
    let Ok(d) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return String::new();
    };
    if let Some(obj) = d.as_object() {
        for k in ["file_path", "command", "pattern", "path", "kind"] {
            if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    String::new()
}

/// Seconds-resolution UTC timestamp for audit JSONL entries.
fn audit_ts() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // civil_from_days inline
    let z = (days as i64) + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn audit(
    log: Option<&Path>,
    lock: Option<&std::sync::Mutex<()>>,
    tool: &str,
    key_arg: &str,
    status: &str,
) {
    let Some(log) = log else {
        return;
    };
    let truncated: String = key_arg.chars().take(200).collect();
    let entry = serde_json::json!({
        "ts": audit_ts(),
        "tool": tool,
        "key_arg": truncated,
        "status": status,
    });
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    if let Some(lock) = lock {
        // audit is best-effort; skip write if the mutex is poisoned rather
        // than panicking and crashing the tool invocation.
        if let Ok(_guard) = lock.lock() {
            audit_write(log, &line);
        }
    } else {
        audit_write(log, &line);
    }
}

fn audit_write(log: &Path, line: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{line}")
        });
}

pub(crate) fn result_status(res: &str) -> &'static str {
    if res.starts_with("Error:") || res.starts_with("[exit") || res.starts_with("[timeout]") {
        "error"
    } else {
        "ok"
    }
}

fn check_tool(name: &str, ctx: &AgentLoopRuntime, args: &serde_json::Value) -> Option<String> {
    // AllowAll disables every containment check — no path is ever denied.
    let roots = ctx.sandbox.roots()?;
    match name {
        "Read" | "Edit" | "Write" | "Landscape" => enforce(
            roots,
            args.get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("."),
            ctx.cwd.as_deref(),
        ),
        "Grep" => enforce(
            roots,
            args.get("path").and_then(|v| v.as_str()).unwrap_or("."),
            ctx.cwd.as_deref(),
        ),
        "Glob" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let base = resolve(path, ctx.cwd.as_deref());
            let full_path = base.join(pattern);
            let full_str = full_path.to_string_lossy();
            enforce(roots, &full_str, ctx.cwd.as_deref())
        }
        "Bash" => bash_check(
            roots,
            args.get("command").and_then(|v| v.as_str()).unwrap_or(""),
            ctx.cwd.as_deref(),
        ),
        // Network I/O has no filesystem path to sandbox.
        "WebSearch" => None,
        _ => None,
    }
}

async fn blocking_string(f: impl FnOnce() -> String + Send + 'static) -> String {
    match tokio::task::spawn_blocking(f).await {
        Ok(s) => s,
        Err(e) => format!("Error: {e}"),
    }
}

fn parse_args(args_json: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(args_json).map_err(|e| format!("Error: invalid arguments: {e}"))
}

fn req_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Error: missing '{key}'"))
}

pub(crate) async fn read_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || read_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn read_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let file_path = match req_str(&args, "file_path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = resolve(file_path, cwd);
    if let Some(roots) = roots {
        if let Some(err) = io_enforce(&path, roots) {
            return err;
        }
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    if content.is_empty() {
        return String::new();
    }
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let mut lines: Vec<&str> = content.split_inclusive('\n').collect();
    if offset > 0 {
        lines = lines.into_iter().skip(offset).collect();
    }
    if let Some(limit) = limit {
        lines.truncate(limit);
    }
    let mut numbered = String::new();
    for (idx, line) in lines.iter().enumerate() {
        // `offset` is a 0-indexed skip count, so the file line number is
        // offset + window index + 1 — never the window-relative index.
        numbered.push_str(&format!("{}|{line}", offset + idx + 1));
    }
    numbered
}

pub(crate) async fn landscape_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || landscape_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn landscape_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let file_path = match req_str(&args, "file_path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = resolve(file_path, cwd);
    if let Some(roots) = roots {
        if let Some(err) = io_enforce(&path, roots) {
            return err;
        }
    }

    let depth: Option<usize> = if args.get("depth").is_some() {
        let n = match args.get("depth").and_then(|v| v.as_u64()) {
            Some(n) => n as usize,
            None => return "Error: 'depth' must be a non-negative integer".to_string(),
        };
        Some(n)
    } else {
        None
    };

    let token_budget: Option<usize> =
        if let Some(v) = args.get("token_budget").filter(|v| !v.is_null()) {
            match v.as_u64() {
                Some(n) => Some(n as usize),
                None => {
                    return "Error: 'token_budget' must be a non-negative integer".to_string();
                }
            }
        } else {
            None
        };

    // Reject a symlink root so the directory walk does not follow it.
    let meta = match path.symlink_metadata() {
        Ok(m) => m,
        Err(e) => return format!("Error: {e}"),
    };
    if meta.is_symlink() {
        return format!(
            "Error: Landscape does not follow symlinks: {}",
            path.display()
        );
    }

    if meta.is_dir() {
        let depth_limit = depth.unwrap_or(usize::MAX);
        let build = match LandscapeExplorer.build_with_failures(&path, depth_limit) {
            Ok(b) => b,
            Err(LandscapeError::Read(e)) => return format!("Error: {e}"),
            // `build_with_failures` does not return Utf8, BudgetExceeded,
            // or Tokenizer; these arms exist only for exhaustive matching.
            Err(LandscapeError::Utf8(e)) => return format!("Error: file is not valid UTF-8: {e}"),
            Err(LandscapeError::BudgetExceeded) => {
                return String::from("Error: budget exceeded");
            }
            Err(LandscapeError::Tokenizer(e)) => {
                return format!("Error: tokenizer error: {e}");
            }
        };

        match crate::landscape::format_landscape_block_tool(&build, &path, token_budget) {
            Ok(block) => {
                if block.block.is_empty() {
                    if build.outlines.is_empty() && build.failures.is_empty() {
                        "(empty directory)".to_string()
                    } else {
                        "(no outline items)".to_string()
                    }
                } else {
                    block.block
                }
            }
            Err(LandscapeError::BudgetExceeded) => String::from("Error: budget exceeded"),
            Err(LandscapeError::Tokenizer(e)) => {
                format!("Error: tokenizer error: {e}")
            }
            Err(LandscapeError::Read(e)) => format!("Error: {e}"),
            Err(LandscapeError::Utf8(e)) => format!("Error: file is not valid UTF-8: {e}"),
        }
    } else {
        match LandscapeExplorer.outline_file(&path, false) {
            Ok(items) => render_landscape(&items, &path),
            Err(LandscapeError::Utf8(e)) => format!("Error: file is not valid UTF-8: {e}"),
            Err(LandscapeError::Read(e)) => format!("Error: {e}"),
            // `outline_file` never returns BudgetExceeded or Tokenizer either;
            // these arms exist only for exhaustive matching.
            Err(LandscapeError::BudgetExceeded) => String::from("Error: budget exceeded"),
            Err(LandscapeError::Tokenizer(e)) => format!("Error: tokenizer init failed: {e}"),
        }
    }
}

fn render_landscape(items: &[crate::landscape::OutlineItem], path: &Path) -> String {
    if items.is_empty() {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if crate::landscape::heuristic::recognizes(&ext) {
            return "(no outline items)".to_string();
        }
        return path.display().to_string();
    }
    crate::landscape::format_outline_items(items)
}

pub(crate) async fn edit_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || edit_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn edit_not_found_diagnostic(
    content: &str,
    file_path: &str,
    old_str: &str,
    edit_num: usize,
) -> String {
    let first_line = old_str.lines().next().unwrap_or("").trim();
    let needle: &str = if first_line.is_empty() {
        return format!(
            "Error: old_string not found in {file_path} (edit {edit_num}) — first line is empty or whitespace-only"
        );
    } else if first_line.chars().count() > 80 {
        let byte_idx = first_line
            .char_indices()
            .nth(80)
            .map(|(i, _)| i)
            .unwrap_or(first_line.len());
        &first_line[..byte_idx]
    } else {
        first_line
    };
    let hint = if let Some(pos) = content.find(needle) {
        let line_no = content[..pos].lines().count() + 1;
        let context_start = content[..pos].rfind('\n').map_or(0, |n| n + 1);
        let context_end = content[pos..].find('\n').map_or(content.len(), |n| pos + n);
        let cap_byte = content[context_start..]
            .char_indices()
            .nth(200)
            .map(|(i, _)| context_start + i)
            .unwrap_or(content.len());
        let context_end = context_end.min(cap_byte);
        let context = &content[context_start..context_end];
        format!(" — did you mean to match near line {line_no}? Found:\n{context}")
    } else {
        String::new()
    };
    format!(
        "Error: old_string not found in {file_path} (edit {edit_num}) — first line: {needle:?}{hint}"
    )
}

fn edit_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let file_path = match req_str(&args, "file_path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let edits_arr = match args.get("edits").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return "Error: missing 'edits'".into(),
    };
    if edits_arr.is_empty() {
        return "Error: no edits provided".into();
    }

    let path = resolve(file_path, cwd);
    if let Some(roots) = roots {
        if let Some(err) = io_enforce(&path, roots) {
            return err;
        }
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };

    // Collect old/new pairs with validation.
    let mut edits: Vec<(&str, &str)> = Vec::with_capacity(edits_arr.len());
    for (i, e) in edits_arr.iter().enumerate() {
        let obj = match e.as_object() {
            Some(o) => o,
            None => return format!("Error: edit {} is not an object in {file_path}", i + 1),
        };
        let old_str = match obj.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return format!("Error: edit {} missing 'old_string' in {file_path}", i + 1),
        };
        let new_str = match obj.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return format!("Error: edit {} missing 'new_string' in {file_path}", i + 1),
        };
        edits.push((old_str, new_str));
    }

    // Validate all against original content.
    for (i, (old_str, _)) in edits.iter().enumerate() {
        if old_str.is_empty() {
            return format!("Error: old_string is empty in {file_path} (edit {})", i + 1);
        }
        if !content.contains(old_str) {
            return edit_not_found_diagnostic(&content, file_path, old_str, i + 1);
        }
        if content.matches(old_str).count() > 1 {
            return format!(
                "Error: old_string is not unique in {file_path} (edit {})",
                i + 1
            );
        }
    }

    // Validate non-overlapping.
    let ranges: Vec<(usize, usize)> = edits
        .iter()
        .map(|(o, _)| {
            let pos = content.find(o).unwrap();
            (pos, pos + o.len())
        })
        .collect();
    for i in 0..ranges.len() {
        for j in (i + 1)..ranges.len() {
            if ranges[i].0 < ranges[j].1 && ranges[j].0 < ranges[i].1 {
                return format!(
                    "Error: edits {} and {} overlap in {file_path}",
                    i + 1,
                    j + 1
                );
            }
        }
    }

    // Apply in reverse byte-order.
    let mut indexed: Vec<(usize, &str, &str)> = edits
        .iter()
        .map(|(o, n)| (content.find(o).unwrap(), *o, *n))
        .collect();
    indexed.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut updated = content;
    for (pos, old_str, new_str) in &indexed {
        let end = *pos + old_str.len();
        updated.replace_range(*pos..end, new_str);
    }

    if let Err(e) = std::fs::write(&path, updated) {
        return format!("Error: {e}");
    }
    "OK".into()
}

/// Kills the command's process group when the tool future is dropped or the
/// timeout elapses. `tokio::process::Child::kill_on_drop` only reaches the
/// direct `bash` process; a process group guard is what stops descendants
/// spawned by the shell (`sleep 3600 & wait`).
#[cfg(unix)]
struct ProcessGroupGuard {
    pgid: Option<u32>,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn new(pgid: Option<u32>) -> Self {
        Self { pgid }
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }

    fn kill(&mut self) {
        if let Some(pgid) = self.pgid {
            unsafe {
                libc::killpg(pgid as i32, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            unsafe {
                libc::killpg(pgid as i32, libc::SIGKILL);
            }
        }
    }
}

/// Cursor state for [`emit_live_lines`]: `scan` marks the start of the line
/// currently being assembled and `searched` how far into the buffer the
/// previous call already looked for newlines.
struct LineScanner {
    scan: usize,
    searched: usize,
}

impl LineScanner {
    fn new() -> Self {
        Self {
            scan: 0,
            searched: 0,
        }
    }
}

async fn emit_live_lines(
    id: &str,
    name: &str,
    call: &str,
    ctx: &AgentLoopRuntime,
    bytes: &[u8],
    scanner: &mut LineScanner,
    flush: bool,
) {
    // `scanner.scan` marks the start of the line currently being assembled;
    // `scanner.searched` is how far into `bytes` the previous call already
    // looked for newlines. Each call only searches the region appended since
    // the last call, so a long newline-free run costs O(new bytes) instead of
    // re-scanning the whole accumulated buffer on every 8 KiB read (the old
    // `bytes[scan..]` scan was O(n²) on newline-free output).
    //
    // Completed lines are coalesced into a single chunk so a busy command
    // that emits hundreds of lines per read costs one channel event rather
    // than one event per line — per-line chunks would crowd the terminal
    // `ToolResult`/`TurnComplete`/`Done` control events out of the bounded
    // chat channel. Downstream line-buffered widgets split on `\n` anyway.
    let mut merged = String::new();
    let mut cursor = scanner.searched;
    while cursor < bytes.len() {
        let pos = match bytes[cursor..].iter().position(|&b| b == b'\n') {
            Some(pos) => pos,
            None => break,
        };
        let end = cursor + pos;
        merged.push_str(&String::from_utf8_lossy(&bytes[scanner.scan..end]));
        merged.push('\n');
        cursor = end + 1;
        scanner.scan = cursor;
    }
    scanner.searched = bytes.len();
    if flush && scanner.scan < bytes.len() {
        merged.push_str(&String::from_utf8_lossy(&bytes[scanner.scan..]));
        merged.push('\n');
        scanner.scan = bytes.len();
    }
    if !merged.is_empty() {
        ctx.emit_output(id, name, call, &merged).await;
    }
}

pub(crate) async fn bash_invoke(ctx: &AgentLoopRuntime, args_json: &str, id: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let command = match req_str(&args, "command") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let call = format_tool_call_parts("Bash", &args);
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(BASH_TIMEOUT_SECS)
        .clamp(1, 3600);
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(command);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Start `bash` in its own process group. `kill_on_drop` only kills
        // the direct `bash` child; making it a group leader lets the timeout
        // path (and the dropped-tool-future path, via `kill_on_drop` plus
        // the timeout killpg below) stop every descendant too.
        cmd.as_std_mut().process_group(0);
    }
    if let Some(cwd) = &ctx.cwd {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(project_root());
    }
    if let Some(base) = &ctx.base_env {
        let mut env = base.clone();
        if let Some(extra) = &ctx.extra_env {
            env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        cmd.env_clear().envs(env);
    } else if let Some(extra) = &ctx.extra_env {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        cmd.env_clear().envs(env);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    // Own the process group for the tool call. When the surrounding
    // `join_all` future is dropped on Esc, this guard is dropped too and
    // kills the whole group — not just the direct `bash` child.
    #[cfg(unix)]
    let mut process_group_guard = ProcessGroupGuard::new(child.id());

    // Take the pipes explicitly so the read loop can process stdout and
    // stderr concurrently and emit completed lines to the live pane while
    // keeping the exact same raw stdout-then-stderr result string.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let read_all = async {
        use tokio::io::AsyncReadExt;
        let mut stdout_done = stdout.is_none();
        let mut stderr_done = stderr.is_none();
        let mut read_error: Option<std::io::Error> = None;
        let mut out_scanner = LineScanner::new();
        let mut err_scanner = LineScanner::new();
        loop {
            if read_error.is_some() || (stdout_done && stderr_done) {
                break;
            }
            tokio::select! {
                r = async {
                    stdout.as_mut().unwrap().read(&mut out_buf).await
                }, if !stdout_done => {
                    match r {
                        Ok(0) => {
                            stdout_done = true;
                            stdout = None;
                        }
                        Ok(n) => {
                            out.extend_from_slice(&out_buf[..n]);
                            emit_live_lines(id, "Bash", &call, ctx, &out, &mut out_scanner, false)
                                .await;
                        }
                        Err(e) => {
                            stdout = None;
                            read_error = Some(e);
                        }
                    }
                }
                r = async {
                    stderr.as_mut().unwrap().read(&mut err_buf).await
                }, if !stderr_done => {
                    match r {
                        Ok(0) => {
                            stderr_done = true;
                            stderr = None;
                        }
                        Ok(n) => {
                            err.extend_from_slice(&err_buf[..n]);
                            emit_live_lines(id, "Bash", &call, ctx, &err, &mut err_scanner, false)
                                .await;
                        }
                        Err(e) => {
                            stderr = None;
                            read_error = Some(e);
                        }
                    }
                }
            }
        }
        if let Some(e) = read_error {
            return Err(e);
        }
        emit_live_lines(id, "Bash", &call, ctx, &out, &mut out_scanner, true).await;
        emit_live_lines(id, "Bash", &call, ctx, &err, &mut err_scanner, true).await;
        Ok(())
    };

    let run = async {
        read_all.await?;
        child.wait().await
    };
    match tokio::time::timeout(Duration::from_secs(timeout_secs), run).await {
        Err(_) => {
            // `process_group_guard` fires as this function returns; the
            // direct child is also covered by `kill_on_drop` when the
            // timed-out future was dropped.
            "[timeout]".into()
        }
        Ok(Err(e)) => {
            #[cfg(unix)]
            process_group_guard.disarm();
            format!("Error: {e}")
        }
        Ok(Ok(status)) => {
            #[cfg(unix)]
            process_group_guard.disarm();
            let mut text = String::from_utf8_lossy(&out).into_owned();
            text.push_str(&String::from_utf8_lossy(&err));
            if status.success() {
                text
            } else {
                format!("[exit {}]\n{text}", status.code().unwrap_or(1))
            }
        }
    }
}

pub(crate) async fn websearch_invoke(ctx: &AgentLoopRuntime, args_json: &str, id: &str) -> String {
    let cap = crate::config::max_tool_output_bytes();
    websearch_invoke_with_cap(ctx, args_json, cap, id).await
}

async fn websearch_invoke_with_cap(
    ctx: &AgentLoopRuntime,
    args_json: &str,
    cap: u64,
    id: &str,
) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let url = match req_str(&args, "url") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let call = format_tool_call_parts("WebSearch", &args);
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(WEBSEARCH_TIMEOUT_SECS)
        .clamp(1, 3600);

    // The truncated result must fit inside `cap`. Reserve room for the marker
    // up front, so the outer `invoke` cap sees `len <= cap` and keeps the
    // capped page body instead of replacing it with the generic limit error.
    let marker = if cap > 0 {
        let marker = format!("[truncated at {cap} bytes]\n");
        if marker.len() as u64 <= cap {
            marker
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let data_limit = if cap > 0 {
        cap.checked_sub(marker.len() as u64).map(|n| n as usize)
    } else {
        None
    };

    // Cap the download size so a huge response cannot exhaust memory before
    // `invoke` applies `max_tool_output_bytes`. `--max-filesize` makes curl
    // abort the transfer once the cap is exceeded; we also stop reading at the
    // cap and kill the process group as a belt-and-suspenders measure.
    let max_filesize = if cap > 0 { cap } else { 64 * 1024 * 1024 };
    let mut cmd = Command::new("curl");
    // `-q` disables `.curlrc` so a local config cannot rewrite the request.
    // `-sS` silences the progress meter but still surfaces errors on stderr.
    // `--proto`/`--proto-redir` restrict both the initial transfer and any
    // `-L` redirects to HTTP(S), blocking `file://` and other schemes.
    cmd.arg("-qsSL")
        .arg("--proto")
        .arg("=http,https")
        .arg("--proto-redir")
        .arg("=http,https")
        .arg("--max-filesize")
        .arg(max_filesize.to_string())
        .arg("--max-time")
        .arg(timeout_secs.to_string())
        .arg("--")
        .arg(url);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
    if let Some(cwd) = &ctx.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(base) = &ctx.base_env {
        let mut env = base.clone();
        if let Some(extra) = &ctx.extra_env {
            env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        cmd.env_clear().envs(env);
    } else if let Some(extra) = &ctx.extra_env {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        cmd.env_clear().envs(env);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    // `curl` is a group leader; its pgid matches its pid on POSIX. Capture
    // the pid *before* taking stdout/stderr because `tokio::process::Child::
    // id()` returns `None` once the handles have been reaped.
    #[cfg(unix)]
    let child_pid = child.id();
    #[cfg(unix)]
    let mut process_group_guard = ProcessGroupGuard::new(child_pid);
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let read_all = async {
        use tokio::io::AsyncReadExt;
        let mut capped = false;
        let mut stdout_done = stdout.is_none();
        let mut stderr_done = stderr.is_none();
        let out_limit = data_limit;
        let err_limit = data_limit;
        let mut out_scanner = LineScanner::new();
        let mut err_scanner = LineScanner::new();
        loop {
            if capped || (stdout_done && stderr_done) {
                break;
            }
            tokio::select! {
                r = async {
                    stdout.as_mut().unwrap().read(&mut out_buf).await
                }, if !stdout_done => {
                    match r {
                        Ok(0) | Err(_) => stdout_done = true,
                        Ok(n) => {
                            let remaining = out_limit.map_or(n, |limit| limit.saturating_sub(out.len()));
                            if remaining == 0 {
                                capped = true;
                                break;
                            }
                            let take = n.min(remaining);
                            out.extend_from_slice(&out_buf[..take]);
                            emit_live_lines(id, "WebSearch", &call, ctx, &out, &mut out_scanner, false)
                                .await;
                            if take < n {
                                capped = true;
                                break;
                            }
                        }
                    }
                }
                r = async {
                    stderr.as_mut().unwrap().read(&mut err_buf).await
                }, if !stderr_done => {
                    match r {
                        Ok(0) | Err(_) => stderr_done = true,
                        Ok(n) => {
                            // curl keeps emitting diagnostics after it trips
                            // `--max-filesize` ("Exceeded the maximum allowed"),
                            // so stderr may fill first and the combined output
                            // can legitimately be empty when stdout never got
                            // a chance to deliver body bytes. Prefer body bytes
                            // when they exist; otherwise fall back to stderr.
                            let body_room = out_limit.map_or(n, |limit| limit.saturating_sub(out.len()));
                            let err_room = err_limit.map_or(n, |limit| limit.saturating_sub(err.len()));
                            if body_room > 0 {
                                let take = n.min(err_room);
                                err.extend_from_slice(&err_buf[..take]);
                                emit_live_lines(id, "WebSearch", &call, ctx, &err, &mut err_scanner, false)
                                    .await;
                            } else if err_room == 0 {
                                stderr_done = true;
                            }
                        }
                    }
                }
            }
        }
        emit_live_lines(id, "WebSearch", &call, ctx, &out, &mut out_scanner, true).await;
        emit_live_lines(id, "WebSearch", &call, ctx, &err, &mut err_scanner, true).await;
        capped
    };
    let capped = match tokio::time::timeout(Duration::from_secs(timeout_secs), read_all).await {
        Err(_) => {
            // `process_group_guard` fires as this function returns.
            return "[timeout]".into();
        }
        Ok(capped) => capped,
    };
    if capped {
        // Stop the transfer now that we have enough bytes.
        #[cfg(unix)]
        process_group_guard.kill();
        let _ = child.kill().await;
    }
    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => {
            #[cfg(unix)]
            process_group_guard.disarm();
            return format!("Error: {e}");
        }
    };
    #[cfg(unix)]
    process_group_guard.disarm();
    let mut combined = out;
    combined.extend_from_slice(&err);
    if let Some(limit) = data_limit {
        combined.truncate(limit);
    }
    let text = String::from_utf8_lossy(&combined).trim().to_string();
    if capped {
        format!("{marker}{text}")
    } else if status.success() {
        text
    } else {
        let code = status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(signal) = status.signal() {
                    return 128 + signal;
                }
            }
            #[allow(unreachable_code)]
            1
        });
        format!("[exit {code}]\n{text}")
    }
}

pub(crate) async fn write_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || write_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn write_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let file_path = match req_str(&args, "file_path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let content = match req_str(&args, "content") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = resolve(file_path, cwd);
    if let Some(roots) = roots {
        if let Some(err) = io_enforce(&path, roots) {
            return err;
        }
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("Error: {e}");
        }
    }
    match std::fs::write(&path, content) {
        Ok(()) => "OK".into(),
        Err(e) => format!("Error: {e}"),
    }
}

fn is_binary_prefix(path: &Path) -> bool {
    match std::fs::File::open(path) {
        Ok(mut f) => {
            use std::io::Read;
            let mut buf = [0u8; 8192];
            match f.read(&mut buf) {
                Ok(n) => buf[..n].contains(&0),
                Err(_) => true,
            }
        }
        Err(_) => true,
    }
}

fn fnmatch_name(name: &str, pat: &str) -> bool {
    glob::Pattern::new(pat)
        .map(|p| p.matches(name))
        .unwrap_or(false)
}

#[derive(Clone, Copy)]
struct GrepOptions {
    max_matches: usize,
    context_before: usize,
    context_after: usize,
}

/// Accumulates match/context output across the files visited by one grep
/// invocation. Keeping the mutable scan state together avoids threading four
/// separate `&mut` references through the recursive walk.
struct GrepCollector {
    matches: Vec<String>,
    match_count: usize,
    blocked: bool,
    truncated: bool,
}

/// Shared per-file matcher. Both the single-file and directory-walk grep
/// paths call this so match formatting, context expansion, and truncation
/// semantics cannot drift between the two.
fn append_file_matches(
    path: &Path,
    content: &str,
    pattern: &Regex,
    options: &GrepOptions,
    out: &mut GrepCollector,
) {
    // Zero context keeps the exact legacy format: one `path:line:content`
    // entry per match and no block separators.
    if options.context_before == 0 && options.context_after == 0 {
        for (idx, line) in content.lines().enumerate() {
            if pattern.is_match(line) {
                if out.match_count >= options.max_matches {
                    out.truncated = true;
                    return;
                }
                out.matches
                    .push(format!("{}:{}:{line}", path.display(), idx + 1));
                out.match_count += 1;
            }
        }
        return;
    }

    let lines: Vec<&str> = content.lines().collect();
    let last_line = lines.len();

    let mut match_lines = Vec::new();
    let mut first_excluded: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if pattern.is_match(line) {
            if out.match_count >= options.max_matches {
                out.truncated = true;
                first_excluded = Some(idx + 1);
                break;
            }
            match_lines.push(idx + 1);
            out.match_count += 1;
        }
    }
    if match_lines.is_empty() {
        return;
    }

    // Expand each match into a 1-based inclusive [start, end] window and
    // merge windows that touch or overlap. A match past `max_matches` that
    // would otherwise fall inside an admitted window is never emitted:
    // truncation caps the window before that line, so it is not mislabeled
    // as context.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for &line_no in &match_lines {
        let start = line_no.saturating_sub(options.context_before).max(1);
        let mut end = line_no.saturating_add(options.context_after).min(last_line);
        if let Some(excluded) = first_excluded {
            end = end.min(excluded - 1);
        }
        match windows.last_mut() {
            Some((_, last_end)) if start <= last_end.saturating_add(1) => {
                *last_end = (*last_end).max(end);
            }
            _ => windows.push((start, end)),
        }
    }

    // Emit each merged window as one context block. Context lines use `-`
    // where match lines use `:`; non-contiguous blocks are separated by `--`.
    // Since windows are capped to exclude any match past `max_matches`, every
    // emitted line that matches the pattern is a genuine admitted match.
    for (block_idx, &(start, end)) in windows.iter().enumerate() {
        if out.blocked || block_idx > 0 {
            out.matches.push("--".to_string());
        }
        out.blocked = true;
        for line_no in start..=end {
            let line = lines[line_no - 1];
            let separator = if pattern.is_match(line) { ':' } else { '-' };
            out.matches.push(format!(
                "{}{}{}{}{}",
                path.display(),
                separator,
                line_no,
                separator,
                line
            ));
        }
    }
}

fn scan_file(
    path: &Path,
    roots: Option<&[PathBuf]>,
    pattern: &Regex,
    options: &GrepOptions,
    out: &mut GrepCollector,
) {
    // Skip files that resolve outside any allowed root (e.g. via a symlink).
    // `None` roots means containment is disabled (AllowAll).
    if let Some(roots) = roots {
        if io_enforce(path, roots).is_some() {
            return;
        }
    }
    if is_binary_prefix(path) {
        return;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    append_file_matches(path, &content, pattern, options, out);
}

fn walk_grep(
    dir: &Path,
    roots: Option<&[PathBuf]>,
    pattern: &Regex,
    glob_filter: Option<&str>,
    options: &GrepOptions,
    out: &mut GrepCollector,
) {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in entries {
        let name = e.file_name();
        let name_str = name.to_string_lossy();
        let ft = match e.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            dirs.push(e.path());
        } else if ft.is_file() || ft.is_symlink() {
            files.push((e.path(), name_str.into_owned()));
        }
    }
    for (path, name) in files {
        if out.truncated {
            return;
        }
        if let Some(g) = glob_filter {
            if !fnmatch_name(&name, g) {
                continue;
            }
        }
        scan_file(&path, roots, pattern, options, out);
    }
    for d in dirs {
        if out.truncated {
            return;
        }
        // Skip directory symlinks that resolve outside any allowed root.
        if let Some(roots) = roots {
            if io_enforce(&d, roots).is_some() {
                continue;
            }
        }
        walk_grep(&d, roots, pattern, glob_filter, options, out);
    }
}

pub(crate) async fn grep_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || grep_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn grep_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pat = match req_str(&args, "pattern") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let pattern = match Regex::new(pat) {
        Ok(r) => r,
        Err(e) => return format!("Error: invalid regex: {e}"),
    };
    let search_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let base = resolve(search_path, cwd);
    if !base.exists() {
        return format!("Error: path does not exist: {}", base.display());
    }
    let glob_filter = args
        .get("glob")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let options = GrepOptions {
        max_matches: args
            .get("max_matches")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(GREP_MAX_LINES),
        context_before: args
            .get("context_before")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        context_after: args
            .get("context_after")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
    };
    let mut out = GrepCollector {
        matches: Vec::new(),
        match_count: 0,
        blocked: false,
        truncated: false,
    };
    if base.is_file() {
        let name = base.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(roots) = roots {
            if let Some(err) = io_enforce(&base, roots) {
                return err;
            }
        }
        if glob_filter.is_none_or(|g| fnmatch_name(name, g)) {
            if is_binary_prefix(&base) {
                return "(no matches)".into();
            }
            let content = match std::fs::read_to_string(&base) {
                Ok(content) => content,
                Err(e) => return format!("Error: {e}"),
            };
            append_file_matches(&base, &content, &pattern, &options, &mut out);
        }
    } else {
        walk_grep(&base, roots, &pattern, glob_filter, &options, &mut out);
    }
    if out.matches.is_empty() {
        return "(no matches)".into();
    }
    let mut result = out.matches.join("\n");
    if out.truncated {
        result.push_str(&format!("\n[truncated at {} matches]", options.max_matches));
    }
    result
}

pub(crate) async fn glob_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || glob_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn glob_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pattern = match req_str(&args, "pattern") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let search_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let base = resolve(search_path, cwd);
    let full = base.join(pattern);
    let pattern_str = full.to_string_lossy();
    let mut matches: Vec<String> = match glob::glob(&pattern_str) {
        Ok(paths) => paths
            .filter_map(|p| p.ok())
            // Drop matches that resolve outside any allowed root (e.g. via a symlink).
            // `None` roots means containment is disabled (AllowAll).
            .filter(|p| roots.is_none_or(|roots| io_enforce(p, roots).is_none()))
            .map(|p| p.display().to_string())
            .collect(),
        Err(e) => return format!("Error: {e}"),
    };
    matches.sort();
    if matches.is_empty() {
        "(no matches)".into()
    } else {
        matches.join("\n")
    }
}

/// Used by `validate_tool_args` to produce rich error messages when
/// the model sends malformed tool-call arguments. The boolean marks whether
/// the parameter is required; optional parameters are type-checked only when
/// present.
fn tool_param_schema(name: &str) -> Option<Vec<(&'static str, &'static str, bool)>> {
    match name {
        "Read" => Some(vec![("file_path", "string", true)]),
        "Landscape" => Some(vec![
            ("file_path", "string", true),
            ("depth", "integer", false),
            ("token_budget", "integer", false),
        ]),
        "Edit" => Some(vec![
            ("file_path", "string", true),
            ("edits", "array", true),
        ]),
        "Bash" => Some(vec![("command", "string", true)]),
        "Write" => Some(vec![
            ("file_path", "string", true),
            ("content", "string", true),
        ]),
        "Grep" => Some(vec![
            ("pattern", "string", true),
            ("context_before", "integer", false),
            ("context_after", "integer", false),
        ]),
        "Glob" => Some(vec![("pattern", "string", true)]),
        "Task" => Some(vec![("kind", "string", true), ("prompt", "string", true)]),
        "Done" => Some(vec![("summary", "string", true)]),
        "WebSearch" => Some(vec![("url", "string", true), ("timeout", "integer", false)]),
        "Mischief" => Some(vec![]),
        _ => None,
    }
}

/// Validate tool arguments against the expected schema before dispatching
/// to the handler. Returns an error message with the expected shape on mismatch,
/// or `None` if valid.
fn validate_tool_args(name: &str, args: &serde_json::Value) -> Option<String> {
    let schema = tool_param_schema(name)?;
    for (field, expected_type, required) in &schema {
        match args.get(field) {
            None if *required => {
                let expected: Vec<String> = schema
                    .iter()
                    .map(|(f, t, required)| {
                        if *required {
                            format!("{f}: {t}")
                        } else {
                            format!("{f}: {t} (optional)")
                        }
                    })
                    .collect();
                let found: Vec<String> = args
                    .as_object()
                    .map(|o| o.keys().map(|k| format!("{k}: …")).collect())
                    .unwrap_or_default();
                let found_str = if found.is_empty() {
                    "<not an object>".into()
                } else {
                    format!("{{{}}}", found.join(", "))
                };
                let mut msg = format!(
                    "Error: missing '{field}' for {name} tool. \
                     Expected: {{{}}}. Got: {found_str}",
                    expected.join(", ")
                );
                // If file_path is missing for Edit and any edit item has a
                // file_path key, the model likely nested it — give a targeted hint.
                if name == "Edit" && *field == "file_path" {
                    if let Some(edits) = args.get("edits").and_then(|v| v.as_array()) {
                        let misplaced = edits.iter().any(|e| {
                            e.as_object()
                                .map(|o| o.contains_key("file_path"))
                                .unwrap_or(false)
                        });
                        if misplaced {
                            msg.push_str(
                                " (Note: file_path belongs at the top level, not inside individual edits — all edits target the same file)",
                            );
                        }
                    }
                }
                return Some(msg);
            }
            None => continue,
            Some(val) => {
                let ok = match *expected_type {
                    "string" => val.is_string(),
                    "array" => val.is_array(),
                    "integer" => val.is_number(),
                    "object" => val.is_object(),
                    _ => true,
                };
                if !ok {
                    let actual = if val.is_string() {
                        "string"
                    } else if val.is_array() {
                        "array"
                    } else if val.is_number() {
                        "number"
                    } else if val.is_object() {
                        "object"
                    } else if val.is_null() {
                        "null"
                    } else if val.is_boolean() {
                        "bool"
                    } else {
                        "?"
                    };
                    return Some(format!(
                        "Error: '{field}' for {name} must be {expected_type}, got {actual}"
                    ));
                }
            }
        }
    }
    // Nested constraints for tools with structured array items.
    if name == "Edit" {
        if let Some(edits) = args.get("edits").and_then(|v| v.as_array()) {
            if edits.is_empty() {
                return Some("Error: 'edits' array must not be empty for Edit tool".into());
            }
            for (i, edit) in edits.iter().enumerate() {
                let obj = match edit.as_object() {
                    Some(o) => o,
                    None => {
                        return Some(format!(
                            "Error: edit {} in 'edits' must be an object for Edit tool",
                            i + 1
                        ));
                    }
                };
                if !obj.contains_key("old_string") {
                    return Some(format!(
                        "Error: edit {} missing 'old_string' for Edit tool",
                        i + 1
                    ));
                }
                if !obj.contains_key("new_string") {
                    return Some(format!(
                        "Error: edit {} missing 'new_string' for Edit tool",
                        i + 1
                    ));
                }
            }
        }
    }
    None
}

pub(crate) async fn invoke(
    id: &str,
    name: &str,
    ctx: &AgentLoopRuntime,
    args_json: &str,
) -> String {
    if let Some(allowed) = &ctx.allowed_tools {
        if !always_available(name) && !allowed.iter().any(|n| n == name) {
            return format!("Error: unknown tool {name}");
        }
    }
    let ka = audit_key_arg(args_json);
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(_) => {
            audit(
                ctx.audit_log.as_deref(),
                ctx.audit_lock.as_deref(),
                name,
                &ka,
                "error",
            );
            return "Error: invalid arguments".into();
        }
    };
    // Validate args against the tool's expected schema before dispatching.
    // Produces a rich, schema-specific error message so the model can
    // self-correct in a single retry.
    if let Some(err) = validate_tool_args(name, &args) {
        audit(
            ctx.audit_log.as_deref(),
            ctx.audit_lock.as_deref(),
            name,
            &ka,
            "error",
        );
        return err;
    }
    if let Some(err) = check_tool(name, ctx, &args) {
        audit(
            ctx.audit_log.as_deref(),
            ctx.audit_lock.as_deref(),
            name,
            &ka,
            "denied",
        );
        return err;
    }
    let res = match name {
        "Read" => read_invoke(ctx, args_json).await,
        "Edit" => edit_invoke(ctx, args_json).await,
        "Bash" => bash_invoke(ctx, args_json, id).await,
        "Write" => write_invoke(ctx, args_json).await,
        "Grep" => grep_invoke(ctx, args_json).await,
        "Glob" => glob_invoke(ctx, args_json).await,
        "Landscape" => landscape_invoke(ctx, args_json).await,
        "WebSearch" => websearch_invoke(ctx, args_json, id).await,
        "Task" => {
            if let Some(f) = &ctx.task_fn {
                let kind_str = args.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                if prompt.is_empty() {
                    return "Error: Task prompt is required".to_string();
                }
                let Some(kind) = TaskKind::parse(kind_str) else {
                    return format!(
                        "Error: unknown Task kind {kind_str:?}. Expected one of: {}",
                        TaskKind::ALL
                            .iter()
                            .map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                };
                let history = if kind == TaskKind::Fork {
                    ctx.parent_history.clone()
                } else {
                    None
                };
                let out = f(kind, prompt.to_string(), history).await;
                // Label the result so concurrent Task outputs can be told apart.
                let final_output = format!("# {kind}\n\n{out}");
                log::info!(
                    "task dispatch complete: kind={kind} prompt_len={p_len} prompt_preview={p_preview:?} output_len={o_len} output_preview={o_preview:?}",
                    p_len = prompt.len(),
                    p_preview = preview_str(prompt, 120),
                    o_len = final_output.len(),
                    o_preview = preview_str(&final_output, 300),
                );
                final_output
            } else {
                "Error: Task not available for this backend".to_string()
            }
        }
        "Done" => {
            let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            format!("Done. {summary}")
        }
        "Mischief" => include_str!("mischief_dsl.md").to_string(),
        other => format!("Error: unknown tool {other}"),
    };
    audit(
        ctx.audit_log.as_deref(),
        ctx.audit_lock.as_deref(),
        name,
        &ka,
        result_status(&res),
    );
    // Cap output for tools that can produce unbounded results.
    // Task and Done are excluded: Task manages its own context, Done is tiny.
    // Edit and Write always return tiny results.
    let cap = crate::config::max_tool_output_bytes();
    if cap > 0
        && matches!(
            name,
            "Read" | "Bash" | "Grep" | "Glob" | "Landscape" | "WebSearch"
        )
        && res.len() as u64 > cap
    {
        return format!(
            "Tool output exceeded the limit ({} bytes, cap is {}). Try a narrower approach:\n\
- Bash: pipe through `head`, filter with `grep`, or target a specific file.\n\
- Grep: lower `max_matches`, use a more specific `glob`, or narrow the `path`.\n\
- Read: use `offset`/`limit` to paginate.\n\
- Glob: restrict the `path`.",
            res.len(),
            cap
        );
    }
    res
}

pub(crate) fn tool_definitions(filter: Option<&[String]>) -> Vec<ToolDefinition> {
    let mut all = vec![
        ToolDefinition {
            name: "Read".into(),
            description: "Read the contents of a file. Paths outside the sandbox are rejected. Use offset/limit to paginate large files — continue reading with offset until you have the whole file. Each returned line is prefixed with its true 1-based line number in the source file followed by a `|` separator (e.g. `12|content`); `offset` is a 0-indexed line skip count, so line numbers stay aligned with `Grep` results even when paging.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to read (relative or absolute)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Number of lines to skip from the start of the file (0-indexed, default 0)"
                    }
                },
                "required": ["file_path"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Landscape".into(),
            description: "Show the structural outline of a file or directory. For a file, returns space-separated `line|kind|name` items on a single line. Kinds include fn, struct, impl, trait, enum, mod, class, method, heading, and key. Test functions inside `#[cfg(test)]` modules are shown individually. Unknown file extensions yield the file path. For a directory, returns a tree-formatted outline using the same `line|kind|name` item format as the landscape block: directory headers end with `/` and file headers end with `:` followed by space-separated items. The `depth` argument limits how many nested directory levels to descend (default: all levels). The optional `token_budget` argument applies skeletonization and depth back-off when the tree is too large, just like the system prompt landscape block. Paths outside the sandbox are rejected.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file or directory to outline (relative or absolute)"
                    },
                    "depth": {
                        "type": "integer",
                        "description": "For directories, how many nested directory levels to descend (default: all levels)"
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Optional token budget for directory outlines. When set, the tree is trimmed with skeletonization and depth back-off to fit the budget."
                    }
                },
                "required": ["file_path"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Edit".into(),
            description: "Make one or more non-overlapping replacements in a file. Each edit replaces old_string with new_string (requires unique match). Edits are validated against the original file content and must not overlap. When a substring appears multiple times, include 2-3 lines of surrounding context in old_string to make it unique. After a failed edit due to non-uniqueness, re-read only the failing region (with offset/limit), not the whole file. All edits in the array target the same file, specified by the top-level `file_path` parameter — do not nest `file_path` inside individual edit objects.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to edit (relative or absolute)"
                    },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {
                                    "type": "string",
                                    "description": "Exact text to replace — must be unique in the file"
                                },
                                "new_string": {
                                    "type": "string",
                                    "description": "Replacement text"
                                }
                            },
                            "required": ["old_string", "new_string"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["file_path", "edits"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Bash".into(),
            description: "Execute a shell command via `bash -c`. Returns combined stdout and stderr. Exit codes are reported as `[exit N]`; commands exceeding the timeout return `[timeout]`. Runs in the gremlin's working directory with no stdin. Path arguments are checked against the sandbox before execution — escapes return an error without running the command. Default timeout is 120 seconds.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute (run via bash -c)"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (optional, default 120)"
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Write".into(),
            description: "Write content to a file, creating it if necessary. Overwrites the file if it already exists. Parent directories are created as needed.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to write (relative or absolute)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["file_path", "content"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Grep".into(),
            description: "Search file contents using a Rust regex pattern. Returns matches in `path:line_number:line_content` format. When `context_before`/`context_after` are nonzero, each match expands to include up to that many surrounding lines; context lines use a `-` separator instead of `:` (e.g. `path-12-context`), touching/overlapping windows merge into one block, and non-contiguous blocks are separated by a single `--` line. `max_matches` counts matches, not emitted lines. Binaries and dot-directories are skipped, as are common build dirs (node_modules, target, __pycache__). Results are truncated at max_matches (default 2000). The regex engine does NOT support lookahead or lookbehind.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for (Rust regex syntax — no lookahead/lookbehind)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search (default: current directory)"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Glob filter for file names (e.g. `*.rs`)"
                    },
                    "max_matches": {
                        "type": "integer",
                        "description": "Maximum number of matches to return (optional, default 2000)"
                    },
                    "context_before": {
                        "type": "integer",
                        "description": "Number of context lines to include before each match (optional, default 0)"
                    },
                    "context_after": {
                        "type": "integer",
                        "description": "Number of context lines to include after each match (optional, default 0)"
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Glob".into(),
            description: "Find files matching a glob pattern. Supports `**` for recursive matching. Returns sorted paths, one per line.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern (e.g. `**/*.rs`, `src/**/mod.rs`)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Base directory to search in (default: current directory)"
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "WebSearch".into(),
            description: "Fetch a web page and extract its text via `curl -sL`. Returns the body text capped at a byte limit. Use this to look up documentation, check package versions, or read external references. Default timeout is 30 seconds.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch (e.g. https://example.com/page)"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (optional, default 30)"
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Mischief".into(),
            description: "Return the gremlins YAML DSL reference. Call with no arguments to get a complete reference for definition files, stage types, artifact URIs, interpolation, bootstrap, land, and stage definitions.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        },
    ];
    // Task is always available, even when a tool filter is set.
    all.push(ToolDefinition {
        name: "Done".into(),
        description: "Signal that your work is complete — call Done(summary) alongside your final \
message to finish. The summary parameter briefly describes what you accomplished."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Brief summary of what was accomplished"
                }
            },
            "required": ["summary"],
            "additionalProperties": false
        }),
    });
    all.push(ToolDefinition {
        name: "Task".into(),
        description: format!("Delegate a self-contained task to a worker. Non-Fork kinds start with a clean conversation context; the Fork kind inherits the parent conversation history. Multiple Task calls in the same message run concurrently, up to {TASK_MAX_PER_TURN} per message. Returns the worker's final text output."),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": TaskKind::ALL
                        .iter()
                        .map(|kind| serde_json::Value::String(kind.as_str().to_string()))
                        .collect::<Vec<_>>(),
                    "description": "The kind of task to delegate."
                },
                "prompt": {
                    "type": "string",
                    "description": "Full task briefing for the worker. Non-Fork kinds start with a clean conversation context and see nothing from your conversation — include all relevant file contents, diffs, plans, and instructions the worker needs directly in this field. The Fork kind inherits the parent conversation history before this prompt."
                }
            },
            "required": ["kind", "prompt"],
            "additionalProperties": false
        }),
    });
    match filter {
        Some(names) => all
            .into_iter()
            .filter(|t| always_available(&t.name) || names.iter().any(|n| n == &t.name))
            .collect(),
        None => all,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvGuard;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Returns true if a process with the given pid exists on this host.
    ///
    /// `kill -0` reports success for zombies too, because a zombie still
    /// occupies a process-table entry until its parent (or init) reaps it.
    /// A killed descendant is precisely what the process-group timeout test
    /// asserts, so on Linux treat zombies as dead by reading `/proc`.
    fn process_alive(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        {
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // `/proc/<pid>/stat` is `pid (comm) state ...`. `comm` may
                // itself contain spaces and parentheses, so split after the
                // last `)` and read the next field.
                if let Some(state) = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                {
                    if matches!(state, "Z" | "X" | "x") {
                        return false;
                    }
                }
            }
        }
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn tmp() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gremlins-tools-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ctx(cwd: &Path) -> AgentLoopRuntime {
        AgentLoopRuntime {
            cwd: Some(cwd.to_path_buf()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![cwd.to_path_buf()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        }
    }

    #[tokio::test]
    async fn emit_output_prefixes_the_tool_call_id() {
        let dir = tmp();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolOutputLine>(4);
        let mut c = ctx(&dir);
        c.id_prefix = "a3f1.".to_string();
        c.telemetry.tool_output = Some(tx);

        c.emit_output("call_0", "Bash", "Bash echo hi", "hi\n")
            .await;
        let line = rx.recv().await.expect("a line was emitted");
        assert_eq!(
            line.id, "a3f1.call_0",
            "id must carry the task-chain prefix"
        );
        assert_eq!(line.name, "Bash");
        assert_eq!(line.text, "hi\n");
    }

    #[tokio::test]
    async fn emit_output_without_prefix_keeps_the_raw_id() {
        let dir = tmp();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolOutputLine>(4);
        let mut c = ctx(&dir);
        c.telemetry.tool_output = Some(tx);

        c.emit_output("call_0", "Bash", "call", "x").await;
        let line = rx.recv().await.expect("a line was emitted");
        assert_eq!(line.id, "call_0", "top-level ids must stay unprefixed");
    }

    #[tokio::test]
    async fn read_write_roundtrip() {
        let dir = tmp();
        let c = ctx(&dir);
        let path = dir.join("a.txt");
        let args =
            serde_json::json!({"file_path": path.to_str().unwrap(), "content": "hello\nworld\n"})
                .to_string();
        assert_eq!(write_invoke(&c, &args).await, "OK");
        let read_args = serde_json::json!({"file_path": "a.txt"}).to_string();
        assert_eq!(read_invoke(&c, &read_args).await, "1|hello\n2|world\n");
        let limited =
            serde_json::json!({"file_path": "a.txt", "offset": 1, "limit": 1}).to_string();
        assert_eq!(read_invoke(&c, &limited).await, "2|world\n");
    }

    #[tokio::test]
    async fn read_numbers_lines_with_true_file_line_numbers() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::write(dir.join("nums.txt"), "a\nb\nc\nd\n").unwrap();
        let args =
            serde_json::json!({"file_path": "nums.txt", "offset": 1, "limit": 2}).to_string();
        assert_eq!(read_invoke(&c, &args).await, "2|b\n3|c\n");

        // No trailing newline and empty files stay well-formed.
        std::fs::write(dir.join("no-newline.txt"), "x\ny").unwrap();
        let args = serde_json::json!({"file_path": "no-newline.txt"}).to_string();
        assert_eq!(read_invoke(&c, &args).await, "1|x\n2|y");

        std::fs::write(dir.join("empty.txt"), "").unwrap();
        let args = serde_json::json!({"file_path": "empty.txt"}).to_string();
        assert_eq!(read_invoke(&c, &args).await, "");
    }

    #[tokio::test]
    async fn landscape_renders_rust_items() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::write(
            dir.join("lib.rs"),
            "fn main() {}\nstruct Foo;\nimpl Foo {}\ntrait Shape {}\npub enum Color {}\n",
        )
        .unwrap();
        let args = serde_json::json!({"file_path": "lib.rs"}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(out.contains("fn") && out.contains("main"), "got: {out:?}");
        assert!(
            out.contains("struct") && out.contains("Foo"),
            "got: {out:?}"
        );
        assert!(out.contains("impl"), "got: {out:?}");
        assert!(
            out.contains("1|fn|main"),
            "items must use line|kind|name format: {out:?}"
        );
        assert!(
            out.contains("2|struct|Foo") && out.contains("3|impl|Foo"),
            "items must be space-separated on one line: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_renders_markdown_headings() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::write(dir.join("README.md"), "# Title\n\n## Subtitle\n").unwrap();
        let args = serde_json::json!({"file_path": "README.md"}).to_string();
        let out = landscape_invoke(&c, &args).await;
        // .md is no longer a recognized extension; the tool shows the
        // filename instead of "(no outline items)".
        assert!(out.contains("README.md"), "got: {out:?}");
        assert!(!out.contains("heading"), "got: {out:?}");
    }

    #[tokio::test]
    async fn landscape_missing_file_is_error() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"file_path": "missing.rs"}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(out.starts_with("Error:"), "got: {out:?}");
    }

    #[tokio::test]
    async fn landscape_outside_sandbox_is_denied() {
        let dir = tmp();
        let outside = tmp();
        let c = ctx(&dir);
        std::fs::write(outside.join("outside.rs"), "fn outside() {}\n").unwrap();
        let args = serde_json::json!({"file_path": outside.join("outside.rs").to_str().unwrap()})
            .to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(out.contains("outside sandbox"), "got: {out:?}");
    }

    #[tokio::test]
    async fn landscape_directory_lists_files_with_items() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/lib.rs"), "fn a() {}\nstruct B;\nimpl B {}\n").unwrap();
        std::fs::write(dir.join("sub/notes.txt"), "hello").unwrap();
        let args = serde_json::json!({"file_path": ".", "depth": 2}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(
            out.contains("sub/") && out.contains("lib.rs:"),
            "dir listing missing lib.rs tree entry: {out:?}"
        );
        assert!(
            !out.contains("sub/lib.rs") && out.contains("notes.txt:"),
            "directory output must use block tree headers and list unrecognized files: {out:?}"
        );
        assert!(
            out.contains("1|fn|a") && out.contains("2|struct|B"),
            "items not rendered in line|kind|name format: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_directory_skips_hidden_and_vendor() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::create_dir(dir.join(".git")).unwrap();
        std::fs::create_dir(dir.join("target")).unwrap();
        std::fs::create_dir(dir.join("src")).unwrap();
        std::fs::write(dir.join(".git/config"), "x").unwrap();
        std::fs::write(dir.join("target/out"), "x").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "fn a() {}\nstruct B;\nimpl B {}\n").unwrap();
        let args = serde_json::json!({"file_path": ".", "depth": 2}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(!out.contains(".git"), "must skip .git: {out:?}");
        assert!(!out.contains("target"), "must skip target: {out:?}");
        assert!(
            out.contains("src/") && out.contains("lib.rs:"),
            "should include src/lib.rs tree entry: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_depth_limit_is_honored() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        std::fs::write(
            dir.join("a/b/c/deep.rs"),
            "fn deep() {}\nstruct D;\nimpl D {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("a/top.rs"), "fn top() {}\nstruct T;\nimpl T {}\n").unwrap();
        // depth=1: only a/ itself plus one level → a/top.rs visible, a/b/ skipped
        let args = serde_json::json!({"file_path": ".", "depth": 1}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(
            out.contains("a/") && out.contains("top.rs:"),
            "depth 1 should include a/top.rs: {out:?}"
        );
        assert!(
            !out.contains("deep"),
            "depth 1 must not reach a/b/c/deep.rs: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_empty_directory() {
        let dir = tmp();
        let sub = dir.join("empty");
        std::fs::create_dir(&sub).unwrap();
        let c = ctx(&dir);
        let args = serde_json::json!({"file_path": "empty"}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert_eq!(out, "(empty directory)");
    }

    #[tokio::test]
    async fn landscape_directory_with_only_small_files_lists_them() {
        let dir = tmp();
        let c = ctx(&dir);
        let sub = dir.join("shallow");
        std::fs::create_dir(&sub).unwrap();
        // Two recognized files, each with fewer than 3 outline items.
        // Tool mode must still list them (and their items) rather than
        // applying the injected-block three-item cutoff.
        std::fs::write(sub.join("a.rs"), "fn a() {}\nstruct A;\n").unwrap();
        std::fs::write(sub.join("b.rs"), "fn b() {}\n").unwrap();
        let args = serde_json::json!({"file_path": "shallow"}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(
            out.contains("a.rs:") && out.contains("1|fn|a") && out.contains("2|struct|A"),
            "small recognized file with two items must be listed: {out:?}"
        );
        assert!(
            out.contains("b.rs:") && out.contains("1|fn|b"),
            "small recognized file with one item must be listed: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_refuses_symlink_root() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::create_dir(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/lib.rs"), "fn f() {}\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let args = serde_json::json!({"file_path": "link"}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(
            out.contains("does not follow symlinks"),
            "symlink root must be refused: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_bad_depth_is_rejected() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"file_path": ".", "depth": "two"}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(
            out.starts_with("Error:") && out.contains("depth"),
            "bad depth must be rejected: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_bad_token_budget_is_rejected() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"file_path": ".", "token_budget": "two"}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(
            out.starts_with("Error:") && out.contains("token_budget"),
            "bad token_budget must be rejected: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_directory_matches_block_format() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/lib.rs"), "fn a() {}\nstruct B;\nimpl B {}\n").unwrap();

        let build = LandscapeExplorer
            .build_with_failures(&dir, usize::MAX)
            .unwrap();
        let expected =
            crate::landscape::format_landscape_block_tool(&build, &dir, Some(usize::MAX))
                .unwrap()
                .block;
        assert!(!expected.contains("<!-- BEGIN LANDSCAPE BLOCK -->"));
        assert!(!expected.contains("repository metadata"));

        let args = serde_json::json!({"file_path": ".", "depth": 2}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert_eq!(
            out, expected,
            "tool directory output must match the block format"
        );
    }

    #[tokio::test]
    async fn landscape_token_budget_limits_directory_output() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();

        let mut top = String::new();
        let mut mid = String::new();
        let mut deep = String::new();
        for i in 0..20 {
            top.push_str(&format!("fn top_{i:02}() {{}}\n"));
            mid.push_str(&format!("fn mid_{i:02}() {{}}\n"));
            deep.push_str(&format!("fn deep_{i:02}() {{}}\n"));
        }
        std::fs::write(dir.join("top.rs"), top).unwrap();
        std::fs::write(dir.join("src/mid.rs"), mid).unwrap();
        std::fs::write(dir.join("src/nested/deep.rs"), deep).unwrap();

        let build = LandscapeExplorer
            .build_with_failures(&dir, usize::MAX)
            .unwrap();
        let full = crate::landscape::format_landscape_block_tool(&build, &dir, Some(usize::MAX))
            .unwrap()
            .block;
        let full_tokens = crate::landscape::count_tokens(&full).unwrap();
        assert!(full_tokens > 1);
        let budget = full_tokens - 1;

        let args = serde_json::json!({"file_path": ".", "token_budget": budget}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(
            crate::landscape::count_tokens(&out).unwrap() <= budget,
            "tool output must respect token_budget: {out:?}"
        );
        assert!(out.contains("top.rs:"), "shallow file kept: {out:?}");
        assert!(out.contains("mid.rs:"), "depth-limited file kept: {out:?}");
        assert!(
            !out.contains("deep.rs:"),
            "deepest file must be trimmed by token_budget: {out:?}"
        );
    }

    #[tokio::test]
    async fn landscape_unreadable_file_is_annotated() {
        // Create a real file inside a subdirectory with outlineable extension
        // but revoke read permission so the block pipeline cannot parse it.
        // `build_with_failures` surfaces the path as an inline annotation.
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::create_dir(dir.join("sub")).unwrap();
        let broken = dir.join("sub/broken.rs");
        std::fs::write(&broken, "fn x() {}\nstruct X;\nimpl X {}\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o000);
            std::fs::set_permissions(&broken, perms).unwrap();
        }
        let args = serde_json::json!({"file_path": ".", "depth": 2}).to_string();
        let out = landscape_invoke(&c, &args).await;
        assert!(
            out.contains("broken.rs") && out.contains("(unreadable)"),
            "unreadable file must be annotated: {out:?}"
        );
    }

    #[tokio::test]
    async fn edit_replaces_unique() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::write(dir.join("b.txt"), "foo bar foo").unwrap();
        let dup = serde_json::json!({
            "file_path": "b.txt",
            "edits": [{"old_string": "foo", "new_string": "baz"}]
        })
        .to_string();
        assert!(edit_invoke(&c, &dup).await.contains("not unique"));
        std::fs::write(dir.join("b.txt"), "foo bar").unwrap();
        let ok = serde_json::json!({
            "file_path": "b.txt",
            "edits": [{"old_string": "foo", "new_string": "baz"}]
        })
        .to_string();
        assert_eq!(edit_invoke(&c, &ok).await, "OK");
        assert_eq!(
            std::fs::read_to_string(dir.join("b.txt")).unwrap(),
            "baz bar"
        );
    }

    #[tokio::test]
    async fn bash_echo_and_env() {
        let dir = tmp();
        let mut c = ctx(&dir);
        let args = serde_json::json!({"command": "echo hi"}).to_string();
        assert_eq!(bash_invoke(&c, &args, "tool-test-id").await.trim(), "hi");
        c.extra_env = Some(HashMap::from([("GREMLIN_TEST_TOKEN".into(), "abc".into())]));
        let env_args =
            serde_json::json!({"command": "printf %s \"$GREMLIN_TEST_TOKEN\""}).to_string();
        assert_eq!(bash_invoke(&c, &env_args, "tool-test-id").await, "abc");
        let fail = serde_json::json!({"command": "exit 7"}).to_string();
        assert!(bash_invoke(&c, &fail, "tool-test-id")
            .await
            .starts_with("[exit 7]"));
        // Process substitution is bash-only; `sh` would fail here, proving
        // the interpreter is `bash` rather than a POSIX shell.
        let bash_only = serde_json::json!({"command": "diff <(echo a) <(echo a)"}).to_string();
        assert_eq!(bash_invoke(&c, &bash_only, "tool-test-id").await.trim(), "");
    }

    #[tokio::test]
    async fn bash_streaming_emits_completed_lines_and_final_flush() {
        let dir = tmp();
        let mut c = ctx(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolOutputLine>(8);
        c.telemetry.tool_output = Some(tx);

        // No trailing newline: the final flush must emit the trailing partial
        // with an appended `\n`.
        let args = serde_json::json!({ "command": "printf 'a\\nbb\\nccc'" }).to_string();
        let out = bash_invoke(&c, &args, "tool-test-id").await;
        assert_eq!(out, "a\nbb\nccc");

        let mut chunks = Vec::new();
        while let Ok(line) = rx.try_recv() {
            assert_eq!(line.id, "tool-test-id");
            assert_eq!(line.name, "Bash");
            chunks.push(line.text);
        }
        let joined: String = chunks.concat();
        assert_eq!(
            joined, "a\nbb\nccc\n",
            "live chunks must preserve line boundaries and flush the trailing partial"
        );
    }

    #[tokio::test]
    async fn bash_interleaved_stdout_stderr_preserves_stdout_then_stderr() {
        let dir = tmp();
        let c = ctx(&dir);
        // Interleave stdout/stderr writes so completion order is not trivially
        // the emission order; the result string must still be stdout-then-stderr.
        let args = serde_json::json!(
            { "command": "printf out1; printf err1 >&2; printf out2; printf err2 >&2" }
        )
        .to_string();
        let out = bash_invoke(&c, &args, "tool-test-id").await;
        assert_eq!(out, "out1out2err1err2");
    }

    #[tokio::test]
    async fn bash_timeout_from_args() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"command": "echo ok", "timeout": 5}).to_string();
        let out = bash_invoke(&c, &args, "tool-test-id").await;
        assert_eq!(out.trim(), "ok");
        assert!(!out.contains("[timeout]"));
    }

    #[tokio::test]
    async fn bash_timeout_kills_slow_command() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"command": "sleep 10", "timeout": 1}).to_string();
        let out = bash_invoke(&c, &args, "tool-test-id").await;
        assert_eq!(out, "[timeout]");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn bash_timeout_kills_descendant_process_group() {
        let dir = tmp();
        let c = ctx(&dir);
        let child_pid = dir.join("child.pid");
        let parent_pid = dir.join("parent.pid");
        // `sh` records its own pid (group leader) and starts a descendant,
        // then waits. On timeout both must be killed, proving the process
        // group is terminated rather than just the direct `sh` child.
        let command = format!(
            "echo $$ > {parent}; sh -c 'echo $$ > {child}; sleep 10' & wait",
            parent = parent_pid.display(),
            child = child_pid.display(),
        );
        let args = serde_json::json!({ "command": command, "timeout": 1 }).to_string();
        let out = bash_invoke(&c, &args, "tool-test-id").await;
        assert_eq!(out, "[timeout]");

        let read_pid = |path: &std::path::Path| -> u32 {
            std::fs::read_to_string(path)
                .expect("pid file should exist")
                .trim()
                .parse()
                .expect("pid file should hold a valid pid")
        };
        let _parent = read_pid(&parent_pid);
        let child = read_pid(&child_pid);

        // Poll briefly: the timeout path must reap the group promptly. The
        // group leader can linger as a zombie while the shell that spawned
        // it waits, so only check the descendant is gone.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !process_alive(child) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant should be killed by the process-group timeout");
    }

    #[tokio::test]
    async fn bash_timeout_defaults_to_120() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"command": "echo ok"}).to_string();
        let out = bash_invoke(&c, &args, "tool-test-id").await;
        assert_eq!(out.trim(), "ok");
        assert!(!out.contains("[timeout]"));
    }

    #[tokio::test]
    async fn bash_timeout_zero_is_clamped() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"command": "echo ok", "timeout": 0}).to_string();
        let out = bash_invoke(&c, &args, "tool-test-id").await;
        assert_eq!(out.trim(), "ok");
        assert!(!out.contains("[timeout]"));
    }

    #[tokio::test]
    async fn bash_invoke_respects_cwd() {
        // Reproduction: pwd must report cwd, not the process working directory.
        let dir = tmp();
        let marker = "gremlins-cwd-test-marker";
        std::fs::write(dir.join(marker), "ok").unwrap();
        let c = AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let args = serde_json::json!({"command": "pwd; ls"}).to_string();
        let output = bash_invoke(&c, &args, "tool-test-id").await;
        assert!(
            output.contains(dir.to_str().unwrap()),
            "bash_invoke pwd must contain cwd={}, got: {output}",
            dir.display()
        );
        assert!(
            output.contains(marker),
            "bash_invoke ls must see marker file '{marker}', got: {output}"
        );
    }

    #[tokio::test]
    async fn bash_invoke_cwd_none_uses_project_root() {
        // When cwd is None, bash must use project_root() — the same
        // fallback that bash_check uses for path resolution. This keeps
        // validation and execution bases aligned.
        let _env = EnvGuard::lock();
        let dir = tmp();
        std::env::set_var("GREMLINS_PROJECT_ROOT", dir.to_str().unwrap());
        let c = AgentLoopRuntime {
            cwd: None,
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let expected = canonicalize_or_ancestor(&dir);
        let args = serde_json::json!({"command": "pwd"}).to_string();
        let output = bash_invoke(&c, &args, "tool-test-id").await;
        assert!(
            output.trim().contains(expected.to_str().unwrap()),
            "bash_invoke with cwd=None should use project_root={}, got: {output}",
            expected.display()
        );
    }

    #[tokio::test]
    async fn bash_invoke_cwd_none_diverges_from_process_cwd() {
        // Regression: when GREMLINS_PROJECT_ROOT differs from the daemon's
        // working directory, bash execution must still use project_root so
        // validation and execution agree on what `./file.txt` refers to.
        let _env = EnvGuard::lock();
        let project = tmp();
        let daemon = tmp();
        std::env::set_var("GREMLINS_PROJECT_ROOT", project.to_str().unwrap());
        let c = AgentLoopRuntime {
            cwd: None,
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![project.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let expected = canonicalize_or_ancestor(&project);
        // Ensure the daemon cwd is genuinely different from project root.
        assert_ne!(canonicalize_or_ancestor(&daemon), expected);
        let args = serde_json::json!({"command": "pwd"}).to_string();
        let output = bash_invoke(&c, &args, "tool-test-id").await;
        assert!(
            output.trim().contains(expected.to_str().unwrap()),
            "bash_invoke with cwd=None must use project_root={}, not process cwd={}, got: {output}",
            expected.display(),
            canonicalize_or_ancestor(&daemon).display()
        );
    }

    #[tokio::test]
    async fn grep_and_glob() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::write(dir.join("one.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(dir.join("two.py"), "def alpha():\n    pass\n").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/three.rs"), "fn alpha() {}\n").unwrap();
        let grep_args = serde_json::json!({"pattern": "alpha", "glob": "*.rs"}).to_string();
        let out = grep_invoke(&c, &grep_args).await;
        assert!(out.contains("one.rs:1:fn alpha() {}"));
        assert!(out.contains("three.rs:1:fn alpha() {}"));
        assert!(!out.contains("two.py"));
        let glob_args = serde_json::json!({"pattern": "**/*.rs"}).to_string();
        let found = glob_invoke(&c, &glob_args).await;
        assert!(found.contains("one.rs"));
        assert!(found.contains("three.rs"));
        assert!(!found.contains("two.py"));
    }

    #[tokio::test]
    async fn grep_context_expands_with_line_numbers_and_clips() {
        let dir = tmp();
        let c = ctx(&dir);
        let path = dir.join("ctx.txt");
        std::fs::write(&path, "lead\nzero\nalpha\nbeta\ngamma\n").unwrap();
        let args = serde_json::json!({
            "pattern": "alpha",
            "path": path.to_str().unwrap(),
            "context_before": 5,
            "context_after": 1
        })
        .to_string();
        let out = grep_invoke(&c, &args).await;
        let p = path.display();
        assert_eq!(
            out,
            format!("{p}-1-lead\n{p}-2-zero\n{p}:3:alpha\n{p}-4-beta")
        );
    }

    #[tokio::test]
    async fn grep_context_separates_non_contiguous_blocks() {
        let dir = tmp();
        let c = ctx(&dir);
        let path = dir.join("blocks.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\nf\ng\n").unwrap();
        let args = serde_json::json!({
            "pattern": "b|f",
            "path": path.to_str().unwrap(),
            "context_before": 1,
            "context_after": 1
        })
        .to_string();
        let out = grep_invoke(&c, &args).await;
        let p = path.display();
        assert_eq!(
            out,
            format!("{p}-1-a\n{p}:2:b\n{p}-3-c\n--\n{p}-5-e\n{p}:6:f\n{p}-7-g")
        );
    }

    #[tokio::test]
    async fn grep_context_merges_touching_windows() {
        let dir = tmp();
        let c = ctx(&dir);
        let path = dir.join("merge.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let args = serde_json::json!({
            "pattern": "b|d",
            "path": path.to_str().unwrap(),
            "context_before": 1,
            "context_after": 1
        })
        .to_string();
        let out = grep_invoke(&c, &args).await;
        let p = path.display();
        assert_eq!(out, format!("{p}-1-a\n{p}:2:b\n{p}-3-c\n{p}:4:d\n{p}-5-e"));
    }

    #[tokio::test]
    async fn grep_context_max_matches_counts_matches_not_lines() {
        let dir = tmp();
        let c = ctx(&dir);
        let path = dir.join("hits.txt");
        let body: String = (1..=10).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(&path, body).unwrap();
        let args = serde_json::json!({
            "pattern": "hit",
            "path": path.to_str().unwrap(),
            "max_matches": 3,
            "context_before": 1,
            "context_after": 1
        })
        .to_string();
        let out = grep_invoke(&c, &args).await;
        let p = path.display();
        // All ten lines match, but only three are admitted; the fourth match
        // must be excluded entirely rather than emitted as `-` context.
        assert_eq!(
            out,
            format!("{p}:1:hit 1\n{p}:2:hit 2\n{p}:3:hit 3\n[truncated at 3 matches]"),
        );
    }

    #[tokio::test]
    async fn grep_without_context_is_byte_identical_to_legacy_format() {
        let dir = tmp();
        let c = ctx(&dir);
        let path = dir.join("plain.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let args = serde_json::json!({
            "pattern": "b",
            "path": path.to_str().unwrap(),
            "context_before": 0,
            "context_after": 0
        })
        .to_string();
        let out = grep_invoke(&c, &args).await;
        assert_eq!(out, format!("{}:2:b", path.display()));
    }

    #[tokio::test]
    async fn grep_context_directory_walk_uses_same_matcher() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::write(dir.join("a.txt"), "1\n2 alpha\n3\n").unwrap();
        std::fs::write(dir.join("b.txt"), "x\ny alpha\nz\n").unwrap();
        let args = serde_json::json!({
            "pattern": "alpha",
            "context_before": 1,
            "context_after": 1
        })
        .to_string();
        let out = grep_invoke(&c, &args).await;
        let root = canonicalize_or_ancestor(&dir);
        let a = root.join(".").join("a.txt");
        let b = root.join(".").join("b.txt");
        assert_eq!(
            out,
            format!(
                "{}-1-1\n{}:2:2 alpha\n{}-3-3\n--\n{}-1-x\n{}:2:y alpha\n{}-3-z",
                a.display(),
                a.display(),
                a.display(),
                b.display(),
                b.display(),
                b.display()
            )
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn grep_and_glob_skip_symlink_escape() {
        let root = tmp();
        let worktree = root.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.rs"), "fn alpha() {}\n").unwrap();
        // Symlinked file and directory inside the worktree pointing out.
        std::os::unix::fs::symlink(outside.join("secret.rs"), worktree.join("leak.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, worktree.join("outdir")).unwrap();
        // A legitimate in-worktree file that should still be found.
        std::fs::write(worktree.join("ok.rs"), "fn alpha() {}\n").unwrap();

        let c = ctx(&worktree);
        let grep_args = serde_json::json!({"pattern": "alpha"}).to_string();
        let out = grep_invoke(&c, &grep_args).await;
        assert!(
            out.contains("ok.rs"),
            "in-worktree file should match: {out}"
        );
        assert!(
            !out.contains("leak.rs"),
            "symlinked file must be skipped: {out}"
        );
        assert!(
            !out.contains("secret.rs"),
            "escaped dir must be skipped: {out}"
        );

        let glob_args = serde_json::json!({"pattern": "**/*.rs"}).to_string();
        let found = glob_invoke(&c, &glob_args).await;
        assert!(
            found.contains("ok.rs"),
            "in-worktree file should glob: {found}"
        );
        assert!(
            !found.contains("leak.rs"),
            "symlinked file must be filtered: {found}"
        );
        assert!(
            !found.contains("secret.rs"),
            "escaped dir must be filtered: {found}"
        );
    }

    #[tokio::test]
    async fn grep_no_matches_and_bad_regex() {
        let dir = tmp();
        let c = ctx(&dir);
        std::fs::write(dir.join("z.txt"), "nothing").unwrap();
        let none = serde_json::json!({"pattern": "zzz"}).to_string();
        assert_eq!(grep_invoke(&c, &none).await, "(no matches)");
        let bad = serde_json::json!({"pattern": "["}).to_string();
        assert!(grep_invoke(&c, &bad)
            .await
            .starts_with("Error: invalid regex"));
    }

    #[tokio::test]
    async fn grep_max_matches_from_args() {
        let dir = tmp();
        let c = ctx(&dir);
        let body: String = (0..50).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.join("hits.txt"), body).unwrap();
        let args = serde_json::json!({"pattern": "hit", "max_matches": 10}).to_string();
        let out = grep_invoke(&c, &args).await;
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 11, "got: {out}");
        assert!(out.ends_with("[truncated at 10 matches]"));
    }

    #[tokio::test]
    async fn grep_exact_max_matches_is_not_truncated() {
        let dir = tmp();
        let c = ctx(&dir);
        let body: String = (0..10).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.join("hits.txt"), body).unwrap();
        let args = serde_json::json!({"pattern": "hit", "max_matches": 10}).to_string();
        let out = grep_invoke(&c, &args).await;
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 10, "got: {out}");
        assert!(!out.contains("[truncated"));
    }

    #[tokio::test]
    async fn grep_exact_max_matches_file_path_is_not_truncated() {
        let dir = tmp();
        let c = ctx(&dir);
        let body: String = (0..10).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.join("hits.txt"), body).unwrap();
        let args = serde_json::json!({
            "pattern": "hit",
            "path": "hits.txt",
            "max_matches": 10
        })
        .to_string();
        let out = grep_invoke(&c, &args).await;
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 10, "got: {out}");
        assert!(!out.contains("[truncated"));
    }

    #[tokio::test]
    async fn grep_max_matches_defaults_to_2000() {
        let dir = tmp();
        let c = ctx(&dir);
        let body: String = (0..2100).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.join("hits.txt"), body).unwrap();
        let args = serde_json::json!({"pattern": "hit"}).to_string();
        let out = grep_invoke(&c, &args).await;
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 2001, "got {} lines", lines.len());
        assert!(out.ends_with("[truncated at 2000 matches]"));
    }

    #[tokio::test]
    async fn grep_max_matches_larger_than_default() {
        let dir = tmp();
        let c = ctx(&dir);
        let body: String = (0..3000).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.join("hits.txt"), body).unwrap();
        let args = serde_json::json!({"pattern": "hit", "max_matches": 5000}).to_string();
        let out = grep_invoke(&c, &args).await;
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 3000, "got {} lines", lines.len());
        assert!(!out.contains("[truncated"));
    }

    fn spawn_http_server(
        body: String,
        include_content_length: bool,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let headers = if include_content_length {
                format!("Content-Length: {}\r\n", body.len())
            } else {
                String::new()
            };
            let response = format!("HTTP/1.1 200 OK\r\n{headers}Connection: close\r\n\r\n{body}");
            let _ = stream.write_all(response.as_bytes());
        });
        (url, handle)
    }

    fn unused_local_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn websearch_fetches_url() {
        let _env = EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        let (url, server) = spawn_http_server("hello from local server\n".into(), true);
        let args = serde_json::json!({ "url": url }).to_string();
        let out = websearch_invoke(&c, &args, "tool-test-id").await;
        server.join().unwrap();
        assert_eq!(out, "hello from local server");
    }

    #[tokio::test]
    async fn websearch_omitted_timeout_uses_default() {
        let _env = EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        let (url, server) = spawn_http_server("ok\n".into(), true);
        // `timeout` is optional; an args object with only `url` must dispatch.
        let args = serde_json::json!({ "url": url }).to_string();
        let out = websearch_invoke(&c, &args, "tool-test-id").await;
        server.join().unwrap();
        assert_eq!(out, "ok");
        assert!(!out.contains("[timeout]"));
    }

    #[tokio::test]
    async fn websearch_preserves_curl_diagnostics() {
        let _env = EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        // Bind and immediately release a port so the connection is refused.
        let url = format!("http://127.0.0.1:{}/", unused_local_port());
        let args = serde_json::json!({ "url": url, "timeout": 5 }).to_string();
        let out = websearch_invoke(&c, &args, "tool-test-id").await;
        assert!(out.starts_with("[exit"), "got: {out:?}");
        assert!(out.contains("curl:"), "got: {out:?}");
        assert!(out.contains("Failed to connect"), "got: {out:?}");
    }

    #[tokio::test]
    async fn websearch_oversized_response_kept_within_cap() {
        let _env = EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        let body = "0123456789".repeat(30);
        // Omit Content-Length so curl streams the response and our read-side
        // cap (rather than curl's `--max-filesize` check) truncates it.
        let (url, server) = spawn_http_server(body, false);
        let cap = 64;
        let args = serde_json::json!({ "url": url, "timeout": 5 }).to_string();
        let out = websearch_invoke_with_cap(&c, &args, cap, "tool-test-id").await;
        server.join().unwrap();
        assert!(out.starts_with("[truncated at 64 bytes]\n"), "got: {out:?}");
        assert!(out.contains("0123456789"), "got: {out:?}");
        assert!(out.len() as u64 <= cap, "got len {}: {out:?}", out.len());
        assert!(!out.contains("Tool output exceeded"), "got: {out:?}");
    }

    #[test]
    fn tool_definitions_filter() {
        let all = tool_definitions(None);
        assert_eq!(all.len(), 11);
        let all_names: Vec<_> = all.iter().map(|t| t.name.as_str()).collect();
        assert!(all_names.contains(&"Landscape"), "got: {all_names:?}");
        assert!(all_names.contains(&"WebSearch"), "got: {all_names:?}");
        let filtered = tool_definitions(Some(&["Read".into(), "Bash".into()]));
        let names: Vec<_> = filtered.iter().map(|t| t.name.as_str()).collect();
        // Task, Done, and Mischief are always available regardless of the filter.
        assert_eq!(names, ["Read", "Bash", "Mischief", "Done", "Task"]);
        assert!(!names.contains(&"WebSearch"), "got: {names:?}");
    }

    #[test]
    fn task_and_done_always_available() {
        let filtered = tool_definitions(Some(&["Read".to_string()]));
        let names: Vec<_> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"Task"), "got: {names:?}");
        assert!(names.contains(&"Done"), "got: {names:?}");
        assert!(names.contains(&"Mischief"), "got: {names:?}");
        assert!(names.contains(&"Read"), "got: {names:?}");
    }

    #[test]
    fn validate_tool_args_missing_required() {
        let err = validate_tool_args("Edit", &serde_json::json!({"file_path": "foo.py"})).unwrap();
        assert!(err.contains("missing 'edits'"));
        assert!(err.contains("Edit"));
        assert!(err.contains("file_path: string"));
    }

    #[test]
    fn validate_tool_args_optional_field_absent_passes() {
        assert!(validate_tool_args(
            "WebSearch",
            &serde_json::json!({"url": "https://example.com"})
        )
        .is_none());
    }

    #[test]
    fn validate_tool_args_optional_field_wrong_type() {
        let err = validate_tool_args(
            "WebSearch",
            &serde_json::json!({"url": "https://example.com", "timeout": "30"}),
        )
        .unwrap();
        assert!(err.contains("'timeout'"));
        assert!(err.contains("must be integer"));
    }

    #[test]
    fn validate_tool_args_wrong_type() {
        let err = validate_tool_args(
            "Edit",
            &serde_json::json!({"file_path": "foo.py", "edits": "not-an-array"}),
        )
        .unwrap();
        assert!(err.contains("'edits'"));
        assert!(err.contains("must be array"));
    }

    #[test]
    fn validate_tool_args_valid_passes() {
        assert!(validate_tool_args(
            "Edit",
            &serde_json::json!({"file_path": "foo.py", "edits": [{"old_string": "a", "new_string": "b"}]})
        )
        .is_none());
        assert!(validate_tool_args("Read", &serde_json::json!({"file_path": "foo.py"})).is_none());
        assert!(validate_tool_args("Bash", &serde_json::json!({"command": "ls"})).is_none());
        assert!(validate_tool_args(
            "Write",
            &serde_json::json!({"file_path": "f", "content": "x"})
        )
        .is_none());
        assert!(validate_tool_args("Grep", &serde_json::json!({"pattern": "fn"})).is_none());
        assert!(validate_tool_args("Glob", &serde_json::json!({"pattern": "*.rs"})).is_none());
        assert!(validate_tool_args(
            "Task",
            &serde_json::json!({"kind": "Scout", "prompt": "do it"})
        )
        .is_none());
    }

    #[test]
    fn validate_tool_args_not_an_object() {
        let err = validate_tool_args("Read", &serde_json::json!("not an object")).unwrap();
        assert!(err.contains("missing 'file_path'"));
        assert!(err.contains("<not an object>"));
    }

    #[test]
    fn validate_tool_args_edit_missing_file_path_no_hint() {
        // file_path missing, but no edit item has file_path either — no hint.
        let err = validate_tool_args(
            "Edit",
            &serde_json::json!({"edits": [{"old_string": "a", "new_string": "b"}]}),
        )
        .unwrap();
        assert!(err.contains("missing 'file_path'"));
        assert!(!err.contains("Note:"));
    }

    #[test]
    fn validate_tool_args_edit_file_path_nested_hint() {
        // file_path missing at top level but present inside an edit item — hint fires.
        let err = validate_tool_args(
            "Edit",
            &serde_json::json!({"edits": [{"file_path": "x.py", "old_string": "a", "new_string": "b"}]}),
        )
        .unwrap();
        assert!(err.contains("missing 'file_path'"));
        assert!(err.contains("Note: file_path belongs at the top level"));
    }

    #[test]
    fn validate_tool_args_edit_empty_array() {
        let err = validate_tool_args("Edit", &serde_json::json!({"file_path": "x", "edits": []}))
            .unwrap();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn validate_tool_args_edit_missing_nested_fields() {
        let err = validate_tool_args(
            "Edit",
            &serde_json::json!({"file_path": "x", "edits": [{}]}),
        )
        .unwrap();
        assert!(err.contains("missing 'old_string'"));
        assert!(err.contains("edit 1"));
    }

    #[test]
    fn validate_tool_args_edit_item_not_object() {
        let err = validate_tool_args(
            "Edit",
            &serde_json::json!({"file_path": "x", "edits": ["not-an-object"]}),
        )
        .unwrap();
        assert!(err.contains("must be an object"));
    }

    #[test]
    fn validate_tool_args_task_missing_prompt() {
        let err = validate_tool_args("Task", &serde_json::json!({"kind": "Scout"})).unwrap();
        assert!(err.contains("missing 'prompt'"));
        assert!(err.contains("Task"));
    }

    #[test]
    fn resolve_relative_against_cwd() {
        let cwd = tmp();
        assert_eq!(
            resolve("a.txt", Some(&cwd)),
            canonicalize_or_ancestor(&cwd).join("a.txt")
        );
        assert_eq!(resolve("/abs", Some(&cwd)), PathBuf::from("/abs"));
    }

    #[test]
    fn resolve_relative_with_cwd_is_absolute() {
        let cwd = tmp();
        let resolved = resolve("sub/file.txt", Some(&cwd));
        assert!(resolved.is_absolute(), "got: {resolved:?}");
        assert_eq!(
            resolved,
            canonicalize_or_ancestor(&cwd).join("sub/file.txt")
        );
    }

    #[test]
    fn resolve_relative_without_cwd_uses_project_root() {
        let mut env = EnvGuard::lock();
        let root = tmp();
        env.set("GREMLINS_PROJECT_ROOT", root.to_str().unwrap());
        let resolved = resolve("sub/file.txt", None);
        assert!(resolved.is_absolute(), "got: {resolved:?}");
        assert_eq!(
            resolved,
            canonicalize_or_ancestor(&root).join("sub/file.txt")
        );
    }

    #[test]
    fn within_worktree_relative_path_with_cwd() {
        let dir = tmp();
        std::fs::create_dir(dir.join("sub")).unwrap();
        assert!(within_worktree(
            Path::new("sub/file.txt"),
            std::slice::from_ref(&dir),
            Some(&dir)
        ));
    }

    #[test]
    fn within_worktree_relative_path_without_cwd_uses_project_root() {
        let mut env = EnvGuard::lock();
        let dir = tmp();
        std::fs::create_dir(dir.join("sub")).unwrap();
        env.set("GREMLINS_PROJECT_ROOT", dir.to_str().unwrap());
        assert!(within_worktree(
            Path::new("sub/file.txt"),
            std::slice::from_ref(&dir),
            None
        ));
    }

    #[test]
    fn glob_check_tool_resolves_relative_base_against_cwd() {
        let dir = tmp();
        std::fs::create_dir(dir.join("src")).unwrap();
        let c = ctx(&dir);
        let args = serde_json::json!({"path": "./src", "pattern": "**/*.rs"});
        assert!(check_tool("Glob", &c, &args).is_none());
    }

    #[test]
    fn glob_check_tool_rejects_relative_escape() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"path": "../outside", "pattern": "*.txt"});
        let err = check_tool("Glob", &c, &args).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn within_worktree_same_dir() {
        let dir = tmp();
        assert!(within_worktree(&dir, std::slice::from_ref(&dir), None));
    }

    #[test]
    fn within_worktree_child() {
        let dir = tmp();
        assert!(within_worktree(
            &dir.join("sub").join("file.txt"),
            std::slice::from_ref(&dir),
            None
        ));
    }

    #[test]
    fn within_worktree_outside() {
        let dir = tmp();
        assert!(!within_worktree(&dir, &[dir.join("sub")], None));
    }

    #[test]
    fn within_worktree_second_root() {
        let dir1 = tmp();
        let dir2 = tmp();
        assert!(within_worktree(&dir2.join("file.txt"), &[dir1, dir2], None));
    }

    #[test]
    fn within_worktree_sibling() {
        let dir = tmp();
        let sibling = dir.parent().unwrap().join("other");
        assert!(!within_worktree(&sibling, std::slice::from_ref(&dir), None));
    }

    #[test]
    #[cfg(unix)]
    fn within_worktree_symlink_traversal() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = worktree.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        // Canonical containment: symlink resolves outside the worktree.
        assert!(!within_worktree(
            &link,
            std::slice::from_ref(&worktree),
            None
        ));
    }

    #[test]
    fn enforce_inside_worktree() {
        let dir = tmp();
        let inside = dir.join("file.txt");
        assert!(enforce(std::slice::from_ref(&dir), inside.to_str().unwrap(), None).is_none());
    }

    #[test]
    fn enforce_outside_worktree() {
        let dir = tmp();
        let outside = dir.parent().unwrap().join("secret.txt");
        let err = enforce(std::slice::from_ref(&dir), outside.to_str().unwrap(), None).unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn enforce_relative_path_with_cwd() {
        let dir = tmp();
        assert!(enforce(std::slice::from_ref(&dir), "file.txt", Some(&dir)).is_none());
    }

    #[test]
    fn enforce_relative_path_escapes_via_cwd() {
        let dir = tmp();
        let err = enforce(
            std::slice::from_ref(&dir),
            "../../../etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_safe_command() {
        let dir = tmp();
        assert!(bash_check(std::slice::from_ref(&dir), "ls -la", Some(&dir)).is_none());
    }

    #[test]
    fn bash_check_absolute_outside() {
        let dir = tmp();
        let err = bash_check(std::slice::from_ref(&dir), "cat /etc/passwd", Some(&dir)).unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_absolute_inside() {
        let dir = tmp();
        let inside = dir.join("file.txt");
        let cmd = format!("cat {}", inside.display());
        assert!(bash_check(std::slice::from_ref(&dir), &cmd, Some(&dir)).is_none());
    }

    #[test]
    fn bash_check_tilde_expansion_outside() {
        let dir = tmp();
        let prev = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", "/nonexistent-home") };
        let err = bash_check(std::slice::from_ref(&dir), "cat ~/.ssh/id_rsa", Some(&dir)).unwrap();
        assert!(err.contains("outside sandbox"));
        match prev {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn bash_check_dotdot_escapes() {
        let dir = tmp();
        let err = bash_check(std::slice::from_ref(&dir), "cat ../secret", Some(&dir)).unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_intermediate_traversal() {
        let dir = tmp();
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cat subdir/../../etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_empty_command() {
        let dir = tmp();
        assert!(bash_check(std::slice::from_ref(&dir), "", Some(&dir)).is_none());
        assert!(bash_check(std::slice::from_ref(&dir), "   ", Some(&dir)).is_none());
    }

    #[test]
    fn bash_check_subshell_dollar_paren_ignored() {
        let dir = tmp();
        // Paths inside $(...) are runtime-expanded, not literal — skip them.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "for f in $(grep -rl pattern /); do echo $f; done",
            Some(&dir),
        )
        .is_none());
        // Nested $(...) works too.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "echo $(cat $(find /tmp -name '*.rs'))",
            Some(&dir),
        )
        .is_none());
    }

    #[test]
    fn bash_check_subshell_backtick_ignored() {
        let dir = tmp();
        // Paths inside backticks are runtime-expanded — skip them.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "for f in `grep -rl pattern /`; do echo $f; done",
            Some(&dir),
        )
        .is_none());
    }

    #[test]
    fn bash_check_heredoc_body_ignored() {
        let dir = tmp();
        // Path-looking tokens in a heredoc body are file content, not
        // path arguments. None of these may trigger the per-token check.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cat <<'EOF'\n//\n///\n//comment\n//example.com/path\n/\n~\n..\nEOF\n",
            Some(&dir),
        )
        .is_none());
        // Unquoted delimiter and `<<-` (leading tabs stripped from the
        // terminator) work too.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cat <<-EOF\n\t//\n\tEOF\n",
            Some(&dir),
        )
        .is_none());
        // Quoted double-quote delimiter.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cat <<\"EOF\"\n//\nEOF\n",
            Some(&dir),
        )
        .is_none());
        // Backslash-quoted delimiter.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cat <<\\EOF\n//\nEOF\n",
            Some(&dir),
        )
        .is_none());
    }

    #[test]
    fn bash_check_heredoc_first_line_redirect_still_checked() {
        let dir = tmp();
        // The remainder of the heredoc's first line is shell, not body.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cat <<EOF > /etc/passwd\nhello\nEOF\n",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_quoted_heredoc_operator_not_treated_as_heredoc() {
        let dir = tmp();
        // A quoted `<<` must not swallow the rest of the command, or real
        // path tokens would escape checking.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "echo \"a << b\" /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_command_after_heredoc_terminator_checked() {
        let dir = tmp();
        // A path command after a closed heredoc must still be denied.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cat <<EOF\nhello\nEOF\ncat /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_multiple_heredocs_all_bodies_ignored() {
        let dir = tmp();
        // Two heredocs on one command line: both bodies are stripped, and a
        // path command after both terminators is still checked.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cat <<A <<B\n//bodyA\nA\n//bodyB\nB\n",
            Some(&dir),
        )
        .is_none());
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cat <<A <<B\n//bodyA\nA\n//bodyB\nB\ncat /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_here_string_not_treated_as_heredoc() {
        let dir = tmp();
        // `<<<` is a here-string; a path after it must still be checked.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cat <<< /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_shift_operator_not_treated_as_heredoc() {
        let dir = tmp();
        // `<<` inside arithmetic and parameter expansions is a bitwise shift,
        // not a heredoc; a following path command must still be checked.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "echo $((1<<2))\ncat /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
        let err = bash_check(
            std::slice::from_ref(&dir),
            "echo ${arr[1<<2]}\ncat /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_escaped_and_comment_heredoc_operator_not_heredoc() {
        let dir = tmp();
        // A backslash-escaped `<<` and a word-initial `# <<` comment must not
        // swallow the rest of the command.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "echo \\<<EOF\ncat /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
        let err = bash_check(
            std::slice::from_ref(&dir),
            "echo hi # <<EOF\ncat /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_ansi_c_quoted_delimiter() {
        let dir = tmp();
        // `$'EOF'` and mixed-quote delimiters resolve to the real terminator.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cat <<$'EOF'\n//\nEOF\n",
            Some(&dir),
        )
        .is_none());
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cat <<EOF'x'\n//\nEOFx\n",
            Some(&dir),
        )
        .is_none());
    }

    #[test]
    fn bash_check_literal_path_still_checked() {
        let dir = tmp();
        // A literal /etc/passwd outside a subshell is still denied.
        let err = bash_check(std::slice::from_ref(&dir), "cat /etc/passwd", Some(&dir)).unwrap();
        assert!(err.contains("outside sandbox"));
        // Path outside subshell with $() still caught.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cat /etc/passwd $(echo foo)",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn bash_check_quoted_paren_in_subshell() {
        let dir = tmp();
        // Quoted parentheses inside $() must not affect the depth counter.
        // The literal /etc/passwd after the subshell is still checked.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cat $(printf '(') /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(
            err.contains("outside sandbox"),
            "literal /etc/passwd after subshell must be caught: {err}"
        );
    }

    #[test]
    fn strip_heredocs_copied_dollar_param_region_skips_escaped_close() {
        // A backslash-escaped close delimiter must not terminate the
        // ${...} copy. Previously `${unset:-\} << x}` returned at the
        // escaped `}`, classified the inner `<<` as a heredoc, and swallowed
        // the following path-bearing command.
        let dir = tmp();
        let err = bash_check(
            std::slice::from_ref(&dir),
            "echo ${unset:-\\} << x}\ncat /etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(
            err.contains("outside sandbox"),
            "trailing /etc/passwd after escaped-close parameter expansion must be caught: {err}"
        );
    }

    #[test]
    fn audit_writes_jsonl() {
        let dir = tmp();
        let log = dir.join("audit.jsonl");
        audit(Some(&log), None, "Read", "/some/file", "ok");
        let lines: Vec<_> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines.len(), 1);
        let entry: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(entry["tool"], "Read");
        assert_eq!(entry["key_arg"], "/some/file");
        assert_eq!(entry["status"], "ok");
        assert!(entry.get("ts").is_some());
    }

    #[test]
    fn audit_appends_multiple() {
        let dir = tmp();
        let log = dir.join("audit.jsonl");
        audit(Some(&log), None, "Read", "a", "ok");
        audit(Some(&log), None, "Bash", "b", "denied");
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let entry: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry["status"], "denied");
    }

    #[test]
    fn audit_none_log_is_noop() {
        audit(None, None, "Read", "/file", "ok");
    }

    #[test]
    fn audit_truncates_key_arg() {
        let dir = tmp();
        let log = dir.join("audit.jsonl");
        let long_arg = "x".repeat(300);
        audit(Some(&log), None, "Read", &long_arg, "ok");
        let entry: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(entry["key_arg"].as_str().unwrap().len(), 200);
    }

    #[tokio::test]
    async fn invoke_denied_writes_audit() {
        let dir = tmp();
        let log = dir.join("audit.jsonl");
        let mut c = ctx(&dir);
        c.audit_log = Some(log.clone());
        let outside = dir.parent().unwrap().join("secret.txt");
        let args = serde_json::json!({"file_path": outside.to_str().unwrap()}).to_string();
        let result = invoke("tool-test-id", "Read", &c, &args).await;
        assert!(result.contains("outside sandbox"));
        let entry: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(entry["status"], "denied");
        assert_eq!(entry["tool"], "Read");
    }

    #[tokio::test]
    async fn invoke_ok_writes_audit() {
        let dir = tmp();
        let log = dir.join("audit.jsonl");
        let target = dir.join("hello.txt");
        std::fs::write(&target, "hi").unwrap();
        let mut c = ctx(&dir);
        c.audit_log = Some(log.clone());
        let args = serde_json::json!({"file_path": target.to_str().unwrap()}).to_string();
        let result = invoke("tool-test-id", "Read", &c, &args).await;
        assert_eq!(result, "1|hi");
        let entry: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(entry["status"], "ok");
    }

    #[tokio::test]
    async fn invoke_invalid_json_writes_error() {
        let dir = tmp();
        let log = dir.join("audit.jsonl");
        let mut c = ctx(&dir);
        c.audit_log = Some(log.clone());
        let result = invoke("tool-test-id", "Read", &c, "not-json{{{").await;
        assert!(result.contains("Error"));
        let entry: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(entry["status"], "error");
    }

    #[tokio::test]
    async fn invoke_bash_denied_writes_audit() {
        let dir = tmp();
        let log = dir.join("audit.jsonl");
        let mut c = ctx(&dir);
        c.audit_log = Some(log.clone());
        let args = serde_json::json!({"command": "cat /etc/passwd"}).to_string();
        let result = invoke("tool-test-id", "Bash", &c, &args).await;
        assert!(result.contains("outside sandbox"));
        let entry: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(entry["status"], "denied");
        assert_eq!(entry["tool"], "Bash");
    }

    #[tokio::test]
    async fn filtered_tool_does_not_mutate_or_spawn() {
        let dir = tmp();
        let target = dir.join("must-not-exist.txt");
        let c = AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: Some(vec!["Read".into()]),
            task_fn: None,
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let write_args =
            serde_json::json!({"file_path": target.to_str().unwrap(), "content": "nope"})
                .to_string();
        let write_out = invoke("tool-test-id", "Write", &c, &write_args).await;
        assert!(write_out.contains("unknown tool"));
        assert!(!target.exists());

        let marker = dir.join("bash-ran");
        let bash_args = serde_json::json!({
            "command": format!("touch {}", marker.display())
        })
        .to_string();
        let bash_out = invoke("tool-test-id", "Bash", &c, &bash_args).await;
        assert!(bash_out.contains("unknown tool"));
        assert!(!marker.exists());
    }

    #[test]
    fn audit_log_path_sibling() {
        let raw = PathBuf::from("/tmp/run.jsonl");
        assert_eq!(audit_log_path(&raw), PathBuf::from("/tmp/run.audit.jsonl"));
    }

    // --- Part 1: Lexical containment tests ---

    #[test]
    fn bash_check_blocks_ln_s_flag() {
        let dir = tmp();
        let err = bash_check(std::slice::from_ref(&dir), "ln -s /tmp/foo bar", Some(&dir)).unwrap();
        assert!(err.contains("symlinks is not allowed"));
    }

    #[test]
    fn bash_check_blocks_ln_sf_flag() {
        let dir = tmp();
        let err = bash_check(
            std::slice::from_ref(&dir),
            "ln -sf /tmp/foo bar",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("symlinks is not allowed"));
    }

    #[test]
    fn bash_check_blocks_ln_symbolic_flag() {
        let dir = tmp();
        let err = bash_check(
            std::slice::from_ref(&dir),
            "ln --symbolic foo bar",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("symlinks is not allowed"));
    }

    #[test]
    fn bash_check_blocks_bin_ln_s() {
        let dir = tmp();
        let err = bash_check(std::slice::from_ref(&dir), "/bin/ln -s foo bar", Some(&dir)).unwrap();
        assert!(err.contains("symlinks is not allowed"));
    }

    #[test]
    fn bash_check_allows_column_s() {
        let dir = tmp();
        // "column -s , file.csv" is not an ln invocation.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "column -s , file.csv",
            Some(&dir)
        )
        .is_none());
    }

    #[test]
    fn bash_check_allows_echo_ln_s() {
        let dir = tmp();
        // echo "ln -s" is an echo, not ln.
        assert!(bash_check(std::slice::from_ref(&dir), "echo \"ln -s\"", Some(&dir)).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn bash_check_blocks_cd_through_symlink_directory() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = worktree.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        // cd link — bare name, no /, caught by cd check in bash_check.
        let err = bash_check(
            std::slice::from_ref(&worktree),
            "cd link; cat secret",
            Some(&worktree),
        )
        .unwrap();
        assert!(err.contains("cd target outside sandbox"), "got: {err}");
    }

    // ANTI-REGRESSION: agent self-verify needs to execute the bootstrap venv,
    // whose `bin/python` is a symlink to a base interpreter outside the
    // worktree. bash_check must allow a final-component symlink whose node is
    // in-worktree. This was silently broken by #1197 (which deleted the old
    // `enforce_allows_venv_symlink_in_cwd` test); do not "harden" bash_check to
    // canonicalize the leaf, or you re-break self-verify for zero real safety
    // (execution isn't gated here — see within_worktree_lexical).
    #[test]
    #[cfg(unix)]
    fn bash_check_allows_venv_symlink_leaf() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir_all(worktree.join(".venv/bin")).unwrap();
        let brew = dir.join("brew/bin");
        std::fs::create_dir_all(&brew).unwrap();
        let real_python = brew.join("python3.14");
        std::fs::write(&real_python, "#!/bin/sh").unwrap();
        // .venv/bin/python -> <outside>/brew/bin/python3.14, exactly like uv/venv.
        let venv_python = worktree.join(".venv/bin/python");
        std::os::unix::fs::symlink(&real_python, &venv_python).unwrap();
        assert!(
            bash_check(
                std::slice::from_ref(&worktree),
                ".venv/bin/python -c 'print(1)'",
                Some(&worktree),
            )
            .is_none(),
            "bash_check must allow executing the in-worktree venv python symlink"
        );
    }

    // The data boundary stays strict: reading/writing THROUGH the same leaf
    // symlink via the file tools is still blocked by io_enforce.
    #[test]
    #[cfg(unix)]
    fn io_enforce_still_blocks_venv_symlink_leaf() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir_all(worktree.join(".venv/bin")).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let secret = outside.join("secret");
        std::fs::write(&secret, "x").unwrap();
        let link = worktree.join(".venv/bin/python");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let err = io_enforce(&link, std::slice::from_ref(&worktree)).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn bash_check_blocks_cd_through_dangling_symlink() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        // Dangling symlink to outside/new — target doesn't exist yet.
        let link = worktree.join("link");
        std::os::unix::fs::symlink(outside.join("new"), &link).unwrap();
        let err = bash_check(
            std::slice::from_ref(&worktree),
            "cd link; cat secret",
            Some(&worktree),
        )
        .unwrap();
        assert!(err.contains("cd target outside sandbox"), "got: {err}");
    }

    #[test]
    fn bash_check_cd_no_arg_passes() {
        let dir = tmp();
        // cd with no argument (goes HOME) — can't statically check, skip.
        assert!(bash_check(std::slice::from_ref(&dir), "cd", Some(&dir)).is_none());
    }

    #[test]
    fn bash_check_cd_dash_passes() {
        let dir = tmp();
        // cd - (previous dir) — can't statically check, skip.
        assert!(bash_check(std::slice::from_ref(&dir), "cd -", Some(&dir)).is_none());
    }

    #[test]
    fn bash_check_cd_inside_worktree_passes() {
        let dir = tmp();
        std::fs::create_dir(dir.join("sub")).unwrap();
        assert!(bash_check(std::slice::from_ref(&dir), "cd sub; ls", Some(&dir)).is_none());
    }

    #[test]
    fn bash_check_cd_tracks_cwd_for_relative_paths() {
        let dir = tmp();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("file.txt"), "ok").unwrap();
        // cd into subdir, then reference ../file.txt — resolves inside
        // worktree only from the new cwd.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cd sub && cat ../file.txt",
            Some(&dir),
        )
        .is_none());
    }

    #[test]
    fn bash_check_cd_then_escape_still_denied() {
        let dir = tmp();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        // cd sub && cat ../../../etc/passwd — escapes even from new cwd.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cd sub && cat ../../../etc/passwd",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn bash_check_multiple_cd_chained() {
        let dir = tmp();
        std::fs::create_dir_all(dir.join("a").join("b")).unwrap();
        std::fs::write(dir.join("file.txt"), "ok").unwrap();
        // cd a && cd b && cat ../../file.txt — resolves inside worktree.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cd a && cd b && cat ../../file.txt",
            Some(&dir),
        )
        .is_none());
    }

    #[test]
    fn bash_check_cd_dash_does_not_update_cwd() {
        let dir = tmp();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        // cd - doesn't update sim_cwd, so ../secret resolves from original
        // cwd and should be denied.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cd - && cat ../secret",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn bash_check_bare_cd_does_not_update_cwd() {
        let dir = tmp();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        // bare cd doesn't update sim_cwd, so ../secret resolves from
        // original cwd and should be denied.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cd && cat ../secret",
            Some(&dir),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn bash_check_cd_dotdot_inside_worktree_passes() {
        let dir = tmp();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("file.txt"), "ok").unwrap();
        // cd .. from a direct child of the sandbox root stays inside; the
        // `..` operand must resolve from the pre-cd cwd (sub), not the
        // post-cd cwd (dir).
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cd .. && cat file.txt",
            Some(&sub),
        )
        .is_none());
    }

    #[test]
    fn bash_check_cd_dotdot_dotdot_passes() {
        let dir = tmp();
        let b = dir.join("a").join("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(dir.join("file.txt"), "ok").unwrap();
        // cd ../.. from two levels down resolves to the root; still inside.
        assert!(bash_check(
            std::slice::from_ref(&dir),
            "cd ../.. && cat file.txt",
            Some(&b),
        )
        .is_none());
    }

    #[test]
    fn bash_check_path_inside_before_cd_but_outside_after_denied() {
        let dir = tmp();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        // From the original cwd (sub), ../secret resolves to dir/secret —
        // inside. After `cd ..` (now at dir), ../secret escapes; cd tracking
        // must still deny it.
        let err = bash_check(
            std::slice::from_ref(&dir),
            "cd .. && cat ../secret",
            Some(&sub),
        )
        .unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn enforce_rejects_symlink_escape() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = worktree.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        // Canonical containment: symlink resolves outside the worktree.
        let err = enforce(
            std::slice::from_ref(&worktree),
            link.to_str().unwrap(),
            None,
        )
        .unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    #[cfg(unix)]
    fn enforce_rejects_dangling_symlink_escape() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        // Dangling symlink: target doesn't exist, so canonicalize() walks
        // past it. The containment check must still read_link() and reject.
        let link = worktree.join("link");
        std::os::unix::fs::symlink(outside.join("new.txt"), &link).unwrap();
        let err = enforce(
            std::slice::from_ref(&worktree),
            link.to_str().unwrap(),
            None,
        )
        .unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn within_worktree_lexical_child() {
        let dir = tmp();
        let child = dir.join("sub").join("file.txt");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(&child, "x").unwrap();
        assert!(within_worktree(&child, std::slice::from_ref(&dir), None));
    }

    // --- Part 2: Edit diagnostics tests ---

    #[test]
    fn edit_not_found_shows_first_line() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "hello\nworld\n").unwrap();
        let args = serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"old_string": "fn alpha() {}", "new_string": ""}]
        })
        .to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("Error: old_string not found"));
        assert!(result.contains("fn alpha() {}"));
    }

    #[test]
    fn edit_not_found_shows_line_hint() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "hello world\nfn alpha() {}\n").unwrap();
        let args = serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"old_string": "fn alpha() {\n  x = 1\n}", "new_string": ""}]
        })
        .to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("did you mean to match"));
        assert!(result.contains("fn alpha"));
    }

    #[test]
    fn edit_empty_old_string() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
        let args = serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"old_string": "", "new_string": ""}]
        })
        .to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("old_string is empty"));
    }

    #[test]
    fn edit_not_found_whitespace_only_first_line() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
        // old_string starts with a blank line, so first_line trims to empty.
        let args = serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"old_string": "\n  fn alpha() {}", "new_string": ""}]
        })
        .to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("first line is empty"));
    }

    #[test]
    fn edit_not_found_multibyte_truncation() {
        let dir = tmp();
        std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
        // First line > 80 chars with multibyte content — must not panic.
        let long_line = "é".repeat(100);
        let old = format!("{long_line}\nbody");
        let args = serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"old_string": old, "new_string": ""}]
        })
        .to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("old_string not found"));
        // The needle should be truncated to 80 chars, not 80 bytes.
        // 100 é's is 200 bytes, so untruncated needle would be 200 bytes.
        assert!(!result.contains(&long_line));
    }

    #[test]
    fn edit_not_found_multibyte_context() {
        let dir = tmp();
        // File content with multibyte characters near the match.
        let content = "é".repeat(300) + "\nfn alpha() {}\nbar";
        std::fs::write(dir.join("f.txt"), &content).unwrap();
        let args = serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"old_string": "fn alpha() {\n  x = 1\n}", "new_string": ""}]
        })
        .to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("did you mean to match"));
        // Must not panic on byte-slice boundary.
    }

    #[test]
    fn edit_batch_three_non_overlapping() {
        let dir = tmp();
        std::fs::write(dir.join("x.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
        let args = serde_json::json!({
            "file_path": "x.txt",
            "edits": [
                {"old_string": "alpha", "new_string": "A"},
                {"old_string": "gamma", "new_string": "C"},
                {"old_string": "delta", "new_string": "D"}
            ]
        })
        .to_string();
        assert_eq!(
            edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args),
            "OK"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("x.txt")).unwrap(),
            "A\nbeta\nC\nD\n"
        );
    }

    #[test]
    fn edit_batch_overlap_rejected() {
        let dir = tmp();
        std::fs::write(dir.join("x.txt"), "hello world\n").unwrap();
        let args = serde_json::json!({
            "file_path": "x.txt",
            "edits": [
                {"old_string": "hello", "new_string": "hi"},
                {"old_string": "hello world", "new_string": "hi earth"}
            ]
        })
        .to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("overlap"));
    }

    #[test]
    fn edit_batch_empty_rejected() {
        let dir = tmp();
        std::fs::write(dir.join("x.txt"), "x\n").unwrap();
        let args = serde_json::json!({"file_path": "x.txt", "edits": []}).to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("no edits provided"));
    }

    #[test]
    fn edit_batch_missing_edits_key() {
        let dir = tmp();
        std::fs::write(dir.join("x.txt"), "x\n").unwrap();
        let args = serde_json::json!({"file_path": "x.txt"}).to_string();
        let result = edit_sync(Some(&dir), Some(std::slice::from_ref(&dir)), &args);
        assert!(result.contains("missing 'edits'"));
    }

    // --- Part 3: Task tests ---

    #[tokio::test]
    async fn task_no_callback_returns_error() {
        let dir = tmp();
        let c = AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let args = serde_json::json!({"kind": "Scout", "prompt": "do something"}).to_string();
        let result = invoke("tool-test-id", "Task", &c, &args).await;
        assert!(result.contains("Task not available"));
    }

    #[tokio::test]
    async fn task_callback_called_when_set() {
        let dir = tmp();
        let called = Arc::new(std::sync::Mutex::new(false));
        let called2 = called.clone();
        let task_fn: TaskFn = Arc::new(move |kind, prompt, _history| {
            *called2.lock().unwrap() = true;
            Box::pin(async move { format!("task result: {kind} / {prompt}") })
        });
        let c = AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: Some(task_fn),
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let args = serde_json::json!({"kind": "Scout", "prompt": "do something"}).to_string();
        let result = invoke("tool-test-id", "Task", &c, &args).await;
        assert!(*called.lock().unwrap());
        assert!(result.contains("task result: Scout / do something"));
    }

    #[tokio::test]
    async fn task_empty_prompt_returns_error() {
        let dir = tmp();
        let task_fn: TaskFn = Arc::new(|_kind, _prompt, _history| {
            Box::pin(async move { "should not be called".to_string() })
        });
        let c = AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: Some(task_fn),
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let args = serde_json::json!({"kind": "Scout", "prompt": ""}).to_string();
        let result = invoke("tool-test-id", "Task", &c, &args).await;
        assert!(result.contains("Task prompt is required"));

        // Missing prompt key entirely — caught by schema validation.
        let args2 = serde_json::json!({"kind": "Scout"}).to_string();
        let result2 = invoke("tool-test-id", "Task", &c, &args2).await;
        assert!(result2.contains("missing 'prompt'"), "got: {result2}");
    }

    #[tokio::test]
    async fn task_unknown_kind_rejected_before_callback() {
        let dir = tmp();
        let called = Arc::new(std::sync::Mutex::new(false));
        let called2 = called.clone();
        let task_fn: TaskFn = Arc::new(move |_kind, _prompt, _history| {
            *called2.lock().unwrap() = true;
            Box::pin(async move { "should not be called".to_string() })
        });
        let c = AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: Some(task_fn),
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let args = serde_json::json!({"kind": "Bogus", "prompt": "go"}).to_string();
        let result = invoke("tool-test-id", "Task", &c, &args).await;
        assert!(result.contains("unknown Task kind"), "got: {result}");
        assert!(
            !*called.lock().unwrap(),
            "callback must not run for unknown kind"
        );
    }

    #[tokio::test]
    async fn task_result_is_headered_with_kind() {
        let dir = tmp();
        let task_fn: TaskFn =
            Arc::new(|_k, _p, _history| Box::pin(async move { "body".to_string() }));
        let c = AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: Some(task_fn),
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let args = serde_json::json!({"kind": "Scout", "prompt": "go"}).to_string();
        assert_eq!(
            invoke("tool-test-id", "Task", &c, &args).await,
            "# Scout\n\nbody"
        );
    }

    #[tokio::test]
    async fn done_invoke_returns_summary() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"summary": "wrote three files and fixed tests"}).to_string();
        let result = invoke("tool-test-id", "Done", &c, &args).await;
        assert_eq!(result, "Done. wrote three files and fixed tests");
    }

    #[tokio::test]
    async fn done_invoke_empty_summary() {
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"summary": ""}).to_string();
        let result = invoke("tool-test-id", "Done", &c, &args).await;
        assert_eq!(result, "Done. ");
    }

    // --- Part 4: IO containment tests (symlink-aware) ---

    #[test]
    fn io_enforce_passes_for_lexical_child() {
        let dir = tmp();
        let f = dir.join("a.txt");
        std::fs::write(&f, "hi").unwrap();
        assert!(io_enforce(&f, std::slice::from_ref(&dir)).is_none());
    }

    #[test]
    fn io_enforce_passes_for_new_file() {
        let dir = tmp();
        let f = dir.join("new.txt");
        // Non-existent file passes — can't be a bad symlink if it doesn't exist.
        assert!(io_enforce(&f, std::slice::from_ref(&dir)).is_none());
    }

    #[test]
    fn io_enforce_passes_for_non_existent_root() {
        let dir = tmp();
        let root = dir.join("does-not-exist-yet");
        let target = root.join("new.txt");
        // A root that doesn't exist yet (e.g. a not-yet-created scratch dir)
        // must not be dropped from containment; its partial-canonical path is still used.
        assert!(io_enforce(&target, std::slice::from_ref(&root)).is_none());
        // A target that escapes the non-existent root via `..` must be rejected.
        let escape = root.join("..").join("outside.txt");
        let err = io_enforce(&escape, std::slice::from_ref(&root)).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn io_enforce_rejects_dotdot_escape_through_non_existent_root() {
        let dir = tmp();
        let root = dir.join("gap");
        // Lexically under the (missing) root, but `..` resolves outside.
        let escape = root.join("..").join("other").join("file.txt");
        let err = io_enforce(&escape, std::slice::from_ref(&root)).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn io_enforce_passes_for_lexical_sibling() {
        let dir = tmp();
        let sibling = dir.parent().unwrap().join(format!(
            "gremlins-io-sibling-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        let target = sibling.join("out.txt");
        std::fs::write(&target, "x").unwrap();
        // target is outside the worktree (sibling, not child).
        let err = io_enforce(&target, std::slice::from_ref(&dir)).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn io_enforce_blocks_symlink_escape() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "sensitive").unwrap();
        let link = worktree.join("link.txt");
        std::os::unix::fs::symlink(outside.join("secret.txt"), &link).unwrap();
        // Lexically inside the worktree, but symlink resolves outside.
        let err = io_enforce(&link, std::slice::from_ref(&worktree)).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn io_enforce_blocks_write_through_symlinked_parent() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        // A pre-existing symlinked directory inside the worktree pointing out.
        let link_dir = worktree.join("venv-link");
        std::os::unix::fs::symlink(&outside, &link_dir).unwrap();
        // The leaf doesn't exist yet, but its parent resolves outside.
        let target = link_dir.join("newfile.txt");
        let err = io_enforce(&target, std::slice::from_ref(&worktree)).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn io_enforce_allows_new_file_under_in_worktree_symlink() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let real = worktree.join("real");
        std::fs::create_dir(&real).unwrap();
        // A symlink that stays inside the worktree is fine.
        let link_dir = worktree.join("link");
        std::os::unix::fs::symlink(&real, &link_dir).unwrap();
        let target = link_dir.join("newfile.txt");
        assert!(io_enforce(&target, std::slice::from_ref(&worktree)).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn io_enforce_blocks_dangling_symlink_escape() {
        let dir = tmp();
        let worktree = dir.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        // Dangling symlink: target doesn't exist yet, so canonicalize()
        // fails on the symlink itself. We must still resolve the symlink
        // target for containment.
        let link = worktree.join("link");
        std::os::unix::fs::symlink(outside.join("new.txt"), &link).unwrap();
        let err = io_enforce(&link, std::slice::from_ref(&worktree)).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    // Integration: multi-root containment with worktree + scratch.
    // Regression guard for the executor wiring that sets GREMLINS_SCRATCH_DIR
    // and assembles the sandbox roots = [worktree, scratch].
    #[test]
    fn multi_root_io_enforce_allows_scratch_denies_elsewhere() {
        let worktree = tmp();
        let scratch = tmp();
        // Simulate a file the agent writes into the scratch dir.
        let scratch_file = scratch.join("cached.dat");
        std::fs::write(&scratch_file, "hello").unwrap();
        // A file outside both roots.
        let outside = tmp();
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "x").unwrap();

        let roots = [worktree.clone(), scratch.clone()];

        // Scratch file is allowed.
        assert!(io_enforce(&scratch_file, &roots).is_none());
        // Outside-both file is denied.
        let err = io_enforce(&outside_file, &roots).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
        // Worktree file is still allowed (regression check).
        let wt_file = worktree.join("main.rs");
        std::fs::write(&wt_file, "fn main() {}").unwrap();
        assert!(io_enforce(&wt_file, &roots).is_none());
    }

    #[test]
    fn multi_root_enforce_allows_scratch_denies_elsewhere() {
        let worktree = tmp();
        let scratch = tmp();
        let outside = tmp();

        let roots = [worktree.clone(), scratch.clone()];

        let scratch_file = scratch.join("build-cache.log");
        assert!(enforce(&roots, scratch_file.to_str().unwrap(), None).is_none());

        let outside_file = outside.join("bad.txt");
        let err = enforce(&roots, outside_file.to_str().unwrap(), None).unwrap();
        assert!(err.contains("outside sandbox"));
    }

    #[test]
    fn within_worktree_relative_root_with_different_agent_cwd() {
        // Regression: when a sandbox root was assembled relative to the
        // daemon's cwd but the agent has a different cwd, within_worktree
        // must still correctly resolve the root. The fix canonicalizes
        // roots at assembly time, so they arrive here as absolute paths.
        // This test simulates that: the root is pre-canonicalized (as the
        // assembly step now does), and the agent cwd is unrelated.
        let daemon_like = tmp();
        let scratch = daemon_like.join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let scratch_file = scratch.join("data.txt");
        std::fs::write(&scratch_file, "ok").unwrap();

        let agent_cwd = tmp();
        // The root is pre-canonicalized (absolute), as the assembly step
        // now guarantees.
        let roots = [canonicalize_or_ancestor(&scratch)];

        // within_worktree must allow the scratch file even when agent cwd
        // is somewhere entirely different.
        assert!(within_worktree(&scratch_file, &roots, Some(&agent_cwd)));
        // io_enforce must agree.
        assert!(io_enforce(&scratch_file, &roots).is_none());

        // A file outside the root is still denied by both.
        let outside = agent_cwd.join("outside.txt");
        std::fs::write(&outside, "x").unwrap();
        assert!(!within_worktree(&outside, &roots, Some(&agent_cwd)));
        assert!(io_enforce(&outside, &roots).is_some());
    }

    // Independent of `normalize_path`: for existing dirs it must agree with
    // plain canonicalization, so tests comparing against it are not tautological.
    fn canon(p: &Path) -> String {
        std::fs::canonicalize(p).unwrap().display().to_string()
    }

    fn sorted_canon(roots: &[PathBuf]) -> String {
        let mut v: Vec<String> = roots.iter().map(|r| canon(r)).collect();
        v.sort();
        v.join(", ")
    }

    #[test]
    fn roots_suffix_sorted_and_empty() {
        // Creation order is deliberately `b`, `a`; the names make `a` sort
        // first, so this fails if the sort is dropped.
        let parent = tmp();
        let (a, b) = (parent.join("aaa"), parent.join("bbb"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        assert_eq!(
            roots_suffix(&[b.clone(), a.clone()], None),
            format!("\n→ sandbox roots: {}, {}", canon(&a), canon(&b))
        );
        assert_eq!(roots_suffix(&[], None), "");
    }

    #[test]
    #[cfg(unix)]
    fn roots_suffix_dedups_symlink_alias() {
        let dir = tmp();
        let link = tmp().join("alias");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert_eq!(
            roots_suffix(&[dir.clone(), link], None),
            format!("\n→ sandbox roots: {}", canon(&dir))
        );
    }

    #[test]
    fn enforce_denial_appends_sorted_roots() {
        let worktree = tmp();
        let scratch = tmp();
        let outside = tmp().join("bad.txt");
        let err = enforce(
            &[worktree.clone(), scratch.clone()],
            outside.to_str().unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(
            err,
            format!(
                "Error: path outside sandbox: {}\n→ sandbox roots: {}",
                outside.display(),
                sorted_canon(&[worktree, scratch])
            )
        );
    }

    #[test]
    fn bash_check_denial_appends_sorted_roots() {
        let worktree = tmp();
        let err = bash_check(std::slice::from_ref(&worktree), "cat /etc/passwd", None).unwrap();
        assert_eq!(
            err,
            format!(
                "Error: path outside sandbox (from /etc/passwd): /etc/passwd\n→ sandbox roots: {}",
                canon(&worktree)
            )
        );
    }

    #[test]
    #[cfg(unix)]
    fn multi_root_bash_check_allows_scratch_path() {
        let worktree = tmp();
        let scratch = tmp();
        let scratch_file = scratch.join("downloaded.tar.gz");
        std::fs::write(&scratch_file, "binary").unwrap();

        let roots = [worktree.clone(), scratch.clone()];

        // A bash command referencing a file in scratch should pass.
        assert!(bash_check(&roots, "cat downloaded.tar.gz", Some(&scratch)).is_none());
        // A bare absolute path outside both should be denied.
        let err = bash_check(&roots, "cat /etc/passwd", Some(&worktree)).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    // --- Part 5: Output size cap tests ---

    #[tokio::test]
    async fn invoke_caps_oversized_bash_result() {
        let _env = crate::test_support::EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        // Generate output that is ~200 bytes — well over a 10-byte cap.
        let args = serde_json::json!({"command": "i=0; while [ $i -lt 200 ]; do printf x; i=$((i+1)); done"}).to_string();
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "10");
        let result = invoke("tool-test-id", "Bash", &c, &args).await;
        assert!(
            result.contains("Tool output exceeded the limit"),
            "oversized Bash must be capped, got: {result}"
        );
        assert!(
            !result.contains("xxxxxxxxxx"),
            "output must not leak through cap message"
        );
    }

    #[tokio::test]
    async fn invoke_caps_oversized_read_result() {
        let _env = crate::test_support::EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        let big = "x".repeat(500);
        std::fs::write(dir.join("big.txt"), &big).unwrap();
        let args = serde_json::json!({"file_path": "big.txt"}).to_string();
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "10");
        let result = invoke("tool-test-id", "Read", &c, &args).await;
        assert!(
            result.contains("Tool output exceeded the limit"),
            "oversized Read must be capped, got: {result}"
        );
        assert!(
            !result.contains("xxx"),
            "file content must not leak through cap message"
        );
    }

    #[tokio::test]
    async fn invoke_caps_oversized_landscape_result() {
        let _env = crate::test_support::EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        // Generate a Rust file with many long-named functions so the landscape
        // output comfortably exceeds a 10-byte cap.
        let mut src = String::new();
        for i in 0..20 {
            src.push_str(&format!("fn very_long_function_name_{i:03}() {{}}\n"));
        }
        std::fs::write(dir.join("big.rs"), &src).unwrap();
        let args = serde_json::json!({"file_path": "big.rs"}).to_string();
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "10");
        let result = invoke("tool-test-id", "Landscape", &c, &args).await;
        assert!(
            result.contains("Tool output exceeded the limit"),
            "oversized Landscape must be capped, got: {result}"
        );
        assert!(
            !result.contains("very_long_function_name"),
            "landscape content must not leak through cap message"
        );
    }

    #[tokio::test]
    async fn invoke_zero_cap_disables_limit() {
        let _env = crate::test_support::EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        let args = serde_json::json!({"command": "i=0; while [ $i -lt 200 ]; do printf x; i=$((i+1)); done"}).to_string();
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "0");
        let result = invoke("tool-test-id", "Bash", &c, &args).await;
        assert!(
            result.contains('x'),
            "cap=0 must return actual output, got: {result}"
        );
        assert!(
            !result.contains("Tool output exceeded the limit"),
            "cap=0 must not trigger cap message"
        );
    }

    #[tokio::test]
    async fn invoke_does_not_cap_done() {
        let _env = crate::test_support::EnvGuard::lock();
        let dir = tmp();
        let c = ctx(&dir);
        let long_summary = "x".repeat(500);
        let args = serde_json::json!({"summary": &long_summary}).to_string();
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "10");
        let result = invoke("tool-test-id", "Done", &c, &args).await;
        assert!(
            result.starts_with("Done. "),
            "Done must not be capped, got: {result}"
        );
        assert!(
            result.contains(&long_summary),
            "Done summary must be preserved"
        );
    }

    #[tokio::test]
    async fn invoke_does_not_cap_task() {
        let _env = crate::test_support::EnvGuard::lock();
        let dir = tmp();
        let task_fn: TaskFn = Arc::new(|_k, _p, _history| Box::pin(async move { "x".repeat(500) }));
        let c = AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::Roots(vec![dir.clone()]),
            audit_log: None,
            allowed_tools: None,
            task_fn: Some(task_fn),
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        };
        let args = serde_json::json!({"kind": "Scout", "prompt": "go"}).to_string();
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "10");
        let result = invoke("tool-test-id", "Task", &c, &args).await;
        assert!(
            result.contains('x'),
            "Task must not be capped, got: {result}"
        );
        assert!(
            !result.contains("Tool output exceeded the limit"),
            "Task must not trigger cap message"
        );
    }

    // --- Part 6: SandboxMode (AllowAll) tests ---

    fn allow_all_ctx(cwd: &Path) -> AgentLoopRuntime {
        AgentLoopRuntime {
            cwd: Some(cwd.to_path_buf()),
            extra_env: None,
            base_env: None,
            sandbox: SandboxMode::AllowAll,
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
        }
    }

    #[tokio::test]
    async fn allow_all_permits_out_of_roots_read() {
        let dir = tmp();
        let outside = dir.parent().unwrap().join(format!(
            "gremlins-allowall-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();

        let c = allow_all_ctx(&dir);
        let args = serde_json::json!({"file_path": secret.to_str().unwrap()}).to_string();
        let result = invoke("tool-test-id", "Read", &c, &args).await;
        assert_eq!(
            result, "1|top secret",
            "AllowAll must permit out-of-roots read"
        );
    }

    #[tokio::test]
    async fn allow_all_permits_out_of_roots_write() {
        let dir = tmp();
        let outside = dir.parent().unwrap().join(format!(
            "gremlins-allowall-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("new.txt");

        let c = allow_all_ctx(&dir);
        let args =
            serde_json::json!({"file_path": target.to_str().unwrap(), "content": "hi"}).to_string();
        let result = invoke("tool-test-id", "Write", &c, &args).await;
        assert_eq!(result, "OK", "AllowAll must permit out-of-roots write");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
    }

    #[tokio::test]
    async fn allow_all_permits_out_of_roots_bash() {
        let dir = tmp();
        let c = allow_all_ctx(&dir);
        // /etc/passwd is outside every root; bash_check must not deny it.
        let args = serde_json::json!({"command": "cat /etc/passwd"}).to_string();
        let result = invoke("tool-test-id", "Bash", &c, &args).await;
        assert!(
            !result.contains("outside sandbox"),
            "AllowAll must bypass bash_check, got: {result}"
        );
        // The command must actually run and succeed, not merely fail in some
        // other way (e.g. "invalid sandbox root" from empty roots).
        assert!(
            result.contains("root:"),
            "AllowAll bash must read /etc/passwd, got: {result}"
        );
        assert!(
            !result.starts_with("[exit"),
            "AllowAll bash must exit 0, got: {result}"
        );
    }

    #[test]
    fn empty_roots_still_deny_io_enforce() {
        let dir = tmp();
        let f = dir.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        // Empty roots must continue to mean "deny all", not "allow all".
        let err = io_enforce(&f, &[]).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn empty_roots_still_deny_enforce() {
        let dir = tmp();
        let err = enforce(&[], dir.join("a.txt").to_str().unwrap(), None).unwrap();
        assert!(err.contains("outside sandbox"), "got: {err}");
    }

    #[test]
    fn empty_roots_still_deny_bash_check() {
        let dir = tmp();
        // Empty roots fail closed in bash_check ("invalid sandbox root"),
        // never "allow all".
        let err = bash_check(&[], "cat /etc/passwd", Some(&dir)).unwrap();
        assert!(err.contains("invalid sandbox root"), "got: {err}");
    }
}
