//! Chat repository-context gathering.
//!
//! Owned by the TUI frontend: the executor no longer runs `git` or reads
//! directory listings. This module assembles a bounded, read-only snapshot of
//! the operator's project that the TUI prepends to the chat prompt.
//!
//! The snapshot is composed from an ordered list of independent section
//! providers. Each provider degrades to "no section" on any failure, so a
//! missing tool, an unreadable directory, or a hung subprocess can only omit
//! that provider's output — never fail the chat turn.

/// Hard deadline for a single context subprocess. A stalled `git` (index lock,
/// fsmonitor, huge status) is killed and its section omitted rather than
/// hanging the chat turn.
const SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Per-section line and byte caps, plus a top-level listing entry cap. These
/// keep the snapshot small enough that it cannot crowd out the operator's
/// actual message.
const MAX_SECTION_LINES: usize = 40;
const MAX_SECTION_BYTES: usize = 4000;
const MAX_LISTING_ENTRIES: usize = 200;

/// Static, presence-only toolchain manifest allowlist. Each row maps a
/// top-level filename to the human-readable toolchain label reported when the
/// file is present. Detection never reads file contents and never runs
/// version probes; adding a toolchain is a one-row change.
const TOOLCHAIN_MANIFESTS: &[(&str, &str)] = &[
    ("rust-toolchain.toml", "Rust (rustup)"),
    ("rust-toolchain", "Rust (rustup)"),
    ("Cargo.toml", "Rust (Cargo)"),
    ("package.json", "Node.js (npm)"),
    (".python-version", "Python (pyenv)"),
    ("go.mod", "Go"),
];

/// A single context section: a title plus already-rendered body text.
///
/// `inline` keeps short single-value sections on one rendered line
/// (`Current branch: main`); multi-line sections render as a title line
/// followed by their body.
struct ChatSection {
    title: &'static str,
    body: String,
    inline: bool,
}

impl ChatSection {
    fn new(title: &'static str, body: String) -> Self {
        Self {
            title,
            body,
            inline: false,
        }
    }

    fn inline(title: &'static str, body: String) -> Self {
        Self {
            title,
            body,
            inline: true,
        }
    }
}

/// The assembled inputs handed to every section provider. Keeping this small
/// and explicit prevents providers from reaching for process-global state.
struct ContextInputs<'a> {
    git_exe: &'a std::path::Path,
    cwd: &'a str,
}

/// A section provider: consumes the context inputs and returns zero or more
/// sections. Providers must never panic and must degrade to an empty result
/// on any failure.
type SectionProvider = fn(&ContextInputs<'_>) -> Vec<ChatSection>;

/// Providers run in this exact order, so the snapshot layout is stable and
/// future providers are additive rather than another special case in the
/// assembly step.
const PROVIDERS: &[SectionProvider] = &[
    environment_section,
    toolchain_hints_section,
    git_sections,
    listing_section,
];

/// Run a subprocess with `cwd` and return its trimmed stdout, or `None` on
/// any failure (missing binary, non-zero exit, permission problem, timeout).
/// Never panics, never surfaces raw subprocess errors. A successful command
/// with empty stdout returns `Some("")` so callers can distinguish "clean"
/// from "failed".
///
/// Stdout is drained on a background thread while the child is waited on, so
/// a command that fills the OS pipe buffer cannot deadlock against the wait
/// loop. The captured output is capped at [`MAX_SECTION_BYTES`] during the
/// drain, so a pathological command cannot balloon memory. When the cap is
/// hit the child is killed (it would otherwise block on a full pipe or die
/// of SIGPIPE) and the capped bytes are still returned so [`cap_section`] can
/// mark them truncated rather than silently dropping the section.
fn run_output(program: &std::path::Path, cwd: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() >= MAX_SECTION_BYTES {
                        break;
                    }
                }
            }
        }
        buf
    });

    let deadline = std::time::Instant::now() + SUBPROCESS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };

    // Always reap the child and join the drain thread, whether the child
    // succeeded, timed out, or was killed after hitting the output cap.
    match status {
        Some(status) if status.success() => {
            let buf = drain.join().unwrap_or_default();
            Some(String::from_utf8_lossy(&buf).trim().to_string())
        }
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = drain.join();
            None
        }
    }
}

/// Run a git command with `cwd`. The `--no-pager` prefix is a git-specific
/// caller concern, so it is supplied here rather than by the generic helper.
fn git_command(inputs: &ContextInputs<'_>, args: &[&str]) -> Option<String> {
    let mut full = vec!["--no-pager"];
    full.extend_from_slice(args);
    run_output(inputs.git_exe, inputs.cwd, &full)
}

/// Whether `cwd` is inside a git work tree. A missing `git` binary is treated
/// exactly like a non-git directory: `false`, so every git section is skipped.
fn is_git_repo(git_exe: &std::path::Path, cwd: &str) -> bool {
    run_output(
        git_exe,
        cwd,
        &["--no-pager", "rev-parse", "--is-inside-work-tree"],
    )
    .as_deref()
        == Some("true")
}

/// Truncate a multi-line section to `max_lines` lines and `max_bytes` bytes,
/// appending a marker when anything was dropped. Returns the (possibly
/// truncated) text.
fn cap_section(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let mut truncated = false;

    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = lines;
    if kept.len() > max_lines {
        kept.truncate(max_lines);
        truncated = true;
    }

    let mut out = kept.join("\n");
    if out.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        truncated = true;
    }

    if truncated {
        out.push_str("\n[truncated]");
    }
    out
}

/// A bounded, read-only snapshot of the project context for a chat turn.
///
/// Providers run in a fixed order: environment, toolchain hints, git block,
/// then the top-level listing. Only the git block is gated on
/// `rev-parse --is-inside-work-tree`; every other provider is attempted for
/// any readable directory. Any provider returning no section simply leaves
/// that section out, so the assembled context can never fail the chat turn.
pub(crate) fn build_chat_context(cwd: &str) -> String {
    build_chat_context_with_git(std::path::Path::new("git"), cwd)
}

/// [`build_chat_context`] with an injectable git executable, so tests can
/// substitute a stub without mutating the process-global `PATH`.
fn build_chat_context_with_git(git_exe: &std::path::Path, cwd: &str) -> String {
    let inputs = ContextInputs { git_exe, cwd };
    assemble_context(&inputs)
}

/// Run every provider in [`PROVIDERS`] order, cap each surviving section
/// body, and join the rendered sections with blank lines.
fn assemble_context(inputs: &ContextInputs<'_>) -> String {
    let mut rendered: Vec<String> = Vec::new();

    for provider in PROVIDERS {
        for section in provider(inputs) {
            if section.body.trim().is_empty() {
                continue;
            }
            let body = cap_section(&section.body, MAX_SECTION_LINES, MAX_SECTION_BYTES);
            if section.inline {
                rendered.push(format!("{}: {}", section.title, body));
            } else {
                rendered.push(format!("{}:\n{}", section.title, body));
            }
        }
    }

    rendered.join("\n\n")
}

/// Static, in-process execution-environment facts only: operating system,
/// architecture, and shell. No subprocesses are run, no `PATH` is emitted,
/// and no executables are enumerated — the agent probes commands on demand.
fn environment_section(_inputs: &ContextInputs<'_>) -> Vec<ChatSection> {
    let shell = if cfg!(windows) {
        std::env::var("COMSPEC").ok()
    } else {
        std::env::var("SHELL").ok()
    };
    vec![ChatSection::new(
        "Environment",
        environment_body(shell.as_deref()),
    )]
}

/// Render the environment body, used by [`environment_section`]. Split out so
/// tests can assert both the with-shell and without-shell forms without
/// mutating process-global environment state.
fn environment_body(shell: Option<&str>) -> String {
    let mut body = format!("OS: {}", std::env::consts::OS);
    body.push_str(&format!("\nArchitecture: {}", std::env::consts::ARCH));
    if let Some(shell) = shell {
        body.push_str(&format!("\nShell: {}", shell));
    }
    body
}

/// Presence-only toolchain manifest detection against [`TOOLCHAIN_MANIFESTS`].
/// Scans the top level of `cwd` for allowlisted filenames; no file contents
/// are read and no version commands are run. Omitted when nothing matches.
fn toolchain_hints_section(inputs: &ContextInputs<'_>) -> Vec<ChatSection> {
    let Ok(entries) = std::fs::read_dir(inputs.cwd) else {
        return Vec::new();
    };

    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        names.push(name);
    }

    let mut rows: Vec<String> = Vec::new();
    for (filename, label) in TOOLCHAIN_MANIFESTS {
        if names.iter().any(|name| name == filename) {
            rows.push(format!("{filename}  {label}"));
        }
    }

    if rows.is_empty() {
        Vec::new()
    } else {
        vec![ChatSection::new("Toolchain hints", rows.join("\n"))]
    }
}

/// The multi-section git block: branch, remotes, status, recent commits, and
/// diff stat. Gated behind a single `rev-parse --is-inside-work-tree` check.
fn git_sections(inputs: &ContextInputs<'_>) -> Vec<ChatSection> {
    if !is_git_repo(inputs.git_exe, inputs.cwd) {
        return Vec::new();
    }

    let mut sections = Vec::new();

    if let Some(branch) = git_command(inputs, &["branch", "--show-current"]) {
        if !branch.is_empty() {
            sections.push(ChatSection::inline("Current branch", branch));
        }
    }

    // Remote names only: `git remote -v` URLs can embed PATs or other
    // userinfo, so we never copy URLs into the prompt.
    if let Some(remotes) = git_command(inputs, &["remote"]) {
        if !remotes.is_empty() {
            sections.push(ChatSection::new("Remotes", remotes));
        }
    }

    // `--branch` always emits a `##` line (including ahead/behind), so a
    // clean tree still shows a status line. `--no-optional-locks` (a
    // global option, before the subcommand) keeps the command read-only:
    // it never takes the index lock or rewrites the index.
    if let Some(status) = git_command(
        inputs,
        &["--no-optional-locks", "status", "--short", "--branch"],
    ) {
        if !status.is_empty() {
            sections.push(ChatSection::new("Git status (short)", status));
        }
    }

    if let Some(log) = git_command(inputs, &["log", "--oneline", "-n", "5"]) {
        if !log.is_empty() {
            sections.push(ChatSection::new("Recent commits", log));
        }
    }

    if let Some(stat) = diff_stat(inputs) {
        sections.push(ChatSection::new("Diff stat vs base", stat));
    }

    sections
}

/// Diff stat against a base ref. Prefers the branch's upstream (the common
/// "push this and open a PR" case has none yet), then falls back to
/// `origin/HEAD`, `main`, and `master` in order. Returns the first non-empty
/// stat, or `None` when no base ref exists.
fn diff_stat(inputs: &ContextInputs<'_>) -> Option<String> {
    for base in ["@{upstream}", "origin/HEAD", "main", "master"] {
        if let Some(stat) = git_command(inputs, &["diff", "--stat", base]) {
            if !stat.is_empty() {
                return Some(stat);
            }
        }
    }
    None
}

/// The top-level listing provider. Produced for any readable directory,
/// independently of git.
fn listing_section(inputs: &ContextInputs<'_>) -> Vec<ChatSection> {
    match top_level_listing(inputs.cwd) {
        Some(listing) => vec![ChatSection::new("Top-level directory listing", listing)],
        None => Vec::new(),
    }
}

/// A bounded listing of the top-level entries in `cwd`, each tagged by file
/// type (directory / file / symlink). Every visible entry is included; no
/// source-code or known-language filtering is applied. The entry count is
/// capped so a huge directory cannot crowd out the operator's message.
fn top_level_listing(cwd: &str) -> Option<String> {
    let entries = std::fs::read_dir(cwd).ok()?;
    let mut rows: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type().ok();
        let tag = if ft.as_ref().is_some_and(|t| t.is_dir()) {
            "dir"
        } else if ft.as_ref().is_some_and(|t| t.is_symlink()) {
            "symlink"
        } else {
            "file"
        };
        rows.push(format!("{tag}  {name}"));
    }
    if rows.is_empty() {
        return None;
    }
    rows.sort();
    if rows.len() > MAX_LISTING_ENTRIES {
        let dropped = rows.len() - MAX_LISTING_ENTRIES;
        rows.truncate(MAX_LISTING_ENTRIES);
        rows.push(format!("[truncated: {dropped} more entries]"));
    }
    Some(rows.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `git` in `dir`, returning an error on failure (e.g. git missing).
    /// `git_exe` overrides the executable so tests can inject a stub without
    /// mutating the process-global `PATH`.
    fn git(
        git_exe: Option<&std::path::Path>,
        dir: &std::path::Path,
        args: &[&str],
    ) -> std::io::Result<()> {
        let mut cmd =
            std::process::Command::new(git_exe.unwrap_or_else(|| std::path::Path::new("git")));
        cmd.args(args).current_dir(dir);
        let status = cmd.status()?;
        assert!(status.success(), "git {args:?} failed in {dir:?}");
        Ok(())
    }

    #[test]
    fn chat_context_in_git_repo_includes_git_sections_and_listing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // Fail (not skip) when git is unavailable: this test requires git.
        git(None, &repo, &["init", "-q", "-b", "main"]).unwrap();
        git(None, &repo, &["config", "user.email", "t@t"]).unwrap();
        git(None, &repo, &["config", "user.name", "t"]).unwrap();
        std::fs::write(repo.join("README.md"), "# hi\n").unwrap();
        std::fs::write(repo.join("notes.txt"), "notes\n").unwrap();
        git(None, &repo, &["add", "-A"]).unwrap();
        git(None, &repo, &["commit", "-qm", "init"]).unwrap();
        git(
            None,
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/org/repo.git",
            ],
        )
        .unwrap();
        // Uncommitted change so `git diff --stat` against the base is non-empty.
        std::fs::write(repo.join("README.md"), "# hi\nchanged\n").unwrap();

        let cwd = repo.to_string_lossy().into_owned();
        let ctx = build_chat_context(&cwd);

        assert!(ctx.contains("Current branch: main"), "ctx: {ctx}");
        assert!(ctx.contains("Remotes:\norigin"), "ctx: {ctx}");
        assert!(ctx.contains("Git status (short):"), "ctx: {ctx}");
        assert!(ctx.contains("## main"), "ctx: {ctx}");
        assert!(ctx.contains("Recent commits:"), "ctx: {ctx}");
        assert!(ctx.contains("Diff stat vs base:"), "ctx: {ctx}");
        assert!(ctx.contains("README.md"), "ctx: {ctx}");
        assert!(ctx.contains("Top-level directory listing:"), "ctx: {ctx}");
        // Non-code files are listed by name, not filtered out.
        assert!(ctx.contains("notes.txt"), "ctx: {ctx}");
        assert!(ctx.contains("README.md"), "ctx: {ctx}");
        // Remote URLs must never leak into the prompt (only names are emitted).
        assert!(!ctx.contains("https://example.com"), "ctx: {ctx}");
    }

    #[test]
    fn chat_context_outside_git_repo_omits_git_sections() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("plain");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "a").unwrap();
        std::fs::write(dir.join("b.csv"), "b").unwrap();
        std::fs::write(dir.join("image.png"), b"\x89PNG").unwrap();
        std::fs::create_dir_all(dir.join("subdir")).unwrap();

        let cwd = dir.to_string_lossy().into_owned();
        let ctx = build_chat_context(&cwd);

        assert!(!ctx.contains("Current branch:"), "ctx: {ctx}");
        assert!(!ctx.contains("Recent commits:"), "ctx: {ctx}");
        assert!(!ctx.contains("Git status"), "ctx: {ctx}");
        assert!(!ctx.contains("Remotes:"), "ctx: {ctx}");
        assert!(!ctx.contains("Diff stat"), "ctx: {ctx}");
        // Environment and listing are still produced for a non-git directory.
        assert!(ctx.contains("Environment:\n"), "ctx: {ctx}");
        assert!(ctx.contains("Top-level directory listing:"), "ctx: {ctx}");
        assert!(ctx.contains("a.md"), "ctx: {ctx}");
        assert!(ctx.contains("b.csv"), "ctx: {ctx}");
        assert!(ctx.contains("image.png"), "ctx: {ctx}");
        assert!(ctx.contains("subdir"), "ctx: {ctx}");
    }

    #[test]
    fn chat_context_nonexistent_dir_has_only_environment() {
        let ctx = build_chat_context("/nonexistent/gremlins/chat/root");

        // The environment provider is static and always attempted, but every
        // directory-dependent section degrades to "no section".
        assert!(ctx.contains("Environment:\n"), "ctx: {ctx}");
        assert!(!ctx.contains("Toolchain hints:"), "ctx: {ctx}");
        assert!(!ctx.contains("Top-level directory listing:"), "ctx: {ctx}");
        assert!(!ctx.contains("Current branch:"), "ctx: {ctx}");
        assert!(!ctx.contains("Recent commits:"), "ctx: {ctx}");
    }

    /// A failing `git` (or a missing one) must degrade exactly like a
    /// non-git directory: every git section omitted, listing still produced.
    /// The stub is injected via the git executable parameter rather than by
    /// mutating the process-global `PATH`, which would race other tests.
    #[test]
    fn chat_context_stubbed_failing_git_omits_git_sections() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.md"), "n").unwrap();

        // A `git` stub that always fails.
        let stub = tmp.path().join("git-stub");
        std::fs::write(&stub, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cwd = dir.to_string_lossy().into_owned();
        let ctx = build_chat_context_with_git(&stub, &cwd);

        assert!(!ctx.contains("Current branch:"), "ctx: {ctx}");
        assert!(!ctx.contains("Remotes:"), "ctx: {ctx}");
        assert!(!ctx.contains("Git status"), "ctx: {ctx}");
        assert!(!ctx.contains("Recent commits:"), "ctx: {ctx}");
        assert!(!ctx.contains("Diff stat"), "ctx: {ctx}");
        assert!(ctx.contains("Environment:\n"), "ctx: {ctx}");
        assert!(ctx.contains("Top-level directory listing:"), "ctx: {ctx}");
        assert!(ctx.contains("note.md"), "ctx: {ctx}");
    }

    #[test]
    fn environment_body_reports_os_arch_and_optional_shell() {
        let with_shell = environment_body(Some("/bin/zsh"));
        assert!(with_shell.contains(std::env::consts::OS), "{with_shell}");
        assert!(with_shell.contains(std::env::consts::ARCH), "{with_shell}");
        assert!(with_shell.contains("Shell: /bin/zsh"), "{with_shell}");

        let without_shell = environment_body(None);
        assert!(
            without_shell.contains(std::env::consts::OS),
            "{without_shell}"
        );
        assert!(
            without_shell.contains(std::env::consts::ARCH),
            "{without_shell}"
        );
        assert!(!without_shell.contains("Shell:"), "{without_shell}");
    }

    #[test]
    fn toolchain_hints_list_present_manifests_and_omits_when_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("project");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("package.json"), "{}\n").unwrap();
        std::fs::write(dir.join("README.md"), "readme\n").unwrap();

        let cwd = dir.to_string_lossy().into_owned();
        let ctx = build_chat_context(&cwd);

        assert!(ctx.contains("Toolchain hints:"), "ctx: {ctx}");
        assert!(ctx.contains("Cargo.toml  Rust (Cargo)"), "ctx: {ctx}");
        assert!(ctx.contains("package.json  Node.js (npm)"), "ctx: {ctx}");
        // Unlisted files are not reported as toolchain hints.
        assert!(!ctx.contains("README.md  "), "ctx: {ctx}");
        // Detection is presence-only: no subprocess version probes.
        assert!(!ctx.contains("rustc --version"), "ctx: {ctx}");
        assert!(!ctx.contains("node --version"), "ctx: {ctx}");
    }

    #[test]
    fn toolchain_hints_absent_when_no_allowlisted_manifests() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("empty-project");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "hello\n").unwrap();

        let cwd = dir.to_string_lossy().into_owned();
        let ctx = build_chat_context(&cwd);

        assert!(!ctx.contains("Toolchain hints:"), "ctx: {ctx}");
    }

    #[test]
    fn assembled_context_never_enumerates_commands_or_dumps_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let cwd = tmp.path().to_string_lossy().into_owned();
        let ctx = build_chat_context(&cwd);

        assert!(!ctx.contains("PATH="), "ctx: {ctx}");
        assert!(!ctx.contains("command -v"), "ctx: {ctx}");
        assert!(!ctx.contains("compgen"), "ctx: {ctx}");
        assert!(!ctx.contains("which "), "ctx: {ctx}");
    }
}
