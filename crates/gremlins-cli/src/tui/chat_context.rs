//! Chat repository-context gathering.
//!
//! Owned by the TUI frontend: the executor no longer runs `git` or reads
//! directory listings. This module assembles a bounded, read-only snapshot of
//! the operator's project (git branch, status, recent commits, diff stat, and
//! a top-level directory listing) that the TUI prepends to the chat prompt.

/// Hard deadline for a single git subprocess. A stalled `git` (index lock,
/// fsmonitor, huge status) is killed and its section omitted rather than
/// hanging the chat turn.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Per-section line and byte caps, plus a top-level listing entry cap. These
/// keep the snapshot small enough that it cannot crowd out the operator's
/// actual message.
const MAX_SECTION_LINES: usize = 40;
const MAX_SECTION_BYTES: usize = 4000;
const MAX_LISTING_ENTRIES: usize = 200;

/// Run a git command with `cwd` and return its trimmed stdout, or `None` on
/// any failure (missing binary, not a repo, detached/broken state, permission
/// problem, timeout). Never panics, never surfaces raw git errors. A
/// successful command with empty stdout returns `Some("")` so callers can
/// distinguish "clean" from "failed".
///
/// Stdout is drained on a background thread while the child is waited on, so
/// a command that fills the OS pipe buffer cannot deadlock against the wait
/// loop. The captured output is capped at [`MAX_SECTION_BYTES`] during the
/// drain, so a pathological command cannot balloon memory. When the cap is
/// hit the child is killed (it would otherwise block on a full pipe or die
/// of SIGPIPE) and the capped bytes are still returned so [`cap_section`] can
/// mark them truncated rather than silently dropping the section.
fn git_output(git_exe: &std::path::Path, cwd: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new(git_exe)
        .arg("--no-pager")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Drain stdout concurrently so the child never blocks writing once the
    // pipe buffer fills. The cap is enforced here (not just in `cap_section`)
    // so a huge `git diff --stat` cannot grow the buffer without bound.
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

    let deadline = std::time::Instant::now() + GIT_TIMEOUT;
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

/// Whether `cwd` is inside a git work tree. A missing `git` binary is treated
/// exactly like a non-git directory: `false`, so every git section is skipped.
fn is_git_repo(git_exe: &std::path::Path, cwd: &str) -> bool {
    git_output(git_exe, cwd, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
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

/// A bounded, read-only snapshot of the repository context for a chat turn.
///
/// Git sections are gated behind a single `rev-parse --is-inside-work-tree`
/// check and are omitted (never an error) when the directory is not a git
/// repository, `git` is missing, or any individual command fails or times
/// out. The top-level directory/file listing is produced independently of git
/// and includes every visible entry — non-code files are listed by name,
/// never read into the prompt.
pub(crate) fn build_chat_context(cwd: &str) -> String {
    build_chat_context_with_git(std::path::Path::new("git"), cwd)
}

/// [`build_chat_context`] with an injectable git executable, so tests can
/// substitute a stub without mutating the process-global `PATH`.
fn build_chat_context_with_git(git_exe: &std::path::Path, cwd: &str) -> String {
    let mut sections: Vec<String> = Vec::new();

    if is_git_repo(git_exe, cwd) {
        if let Some(branch) = git_output(git_exe, cwd, &["branch", "--show-current"]) {
            if !branch.is_empty() {
                sections.push(format!("Current branch: {branch}"));
            }
        }

        // Remote names only: `git remote -v` URLs can embed PATs or other
        // userinfo, so we never copy URLs into the prompt.
        if let Some(remotes) = git_output(git_exe, cwd, &["remote"]) {
            if !remotes.is_empty() {
                sections.push(format!(
                    "Remotes:\n{}",
                    cap_section(&remotes, MAX_SECTION_LINES, MAX_SECTION_BYTES)
                ));
            }
        }

        // `--branch` always emits a `##` line (including ahead/behind), so a
        // clean tree still shows a status line. `--no-optional-locks` (a
        // global option, before the subcommand) keeps the command read-only:
        // it never takes the index lock or rewrites the index.
        if let Some(status) = git_output(
            git_exe,
            cwd,
            &["--no-optional-locks", "status", "--short", "--branch"],
        ) {
            if !status.is_empty() {
                sections.push(format!(
                    "Git status (short):\n{}",
                    cap_section(&status, MAX_SECTION_LINES, MAX_SECTION_BYTES)
                ));
            }
        }

        if let Some(log) = git_output(git_exe, cwd, &["log", "--oneline", "-n", "5"]) {
            if !log.is_empty() {
                sections.push(format!(
                    "Recent commits:\n{}",
                    cap_section(&log, MAX_SECTION_LINES, MAX_SECTION_BYTES)
                ));
            }
        }

        if let Some(stat) = diff_stat(git_exe, cwd) {
            sections.push(format!(
                "Diff stat vs base:\n{}",
                cap_section(&stat, MAX_SECTION_LINES, MAX_SECTION_BYTES)
            ));
        }
    }

    if let Some(listing) = top_level_listing(cwd) {
        sections.push(format!(
            "Top-level directory listing:\n{}",
            cap_section(&listing, MAX_SECTION_LINES, MAX_SECTION_BYTES)
        ));
    }

    sections.join("\n\n")
}

/// Diff stat against a base ref. Prefers the branch's upstream (the common
/// "push this and open a PR" case has none yet), then falls back to
/// `origin/HEAD`, `main`, and `master` in order. Returns the first non-empty
/// stat, or `None` when no base ref exists.
fn diff_stat(git_exe: &std::path::Path, cwd: &str) -> Option<String> {
    for base in ["@{upstream}", "origin/HEAD", "main", "master"] {
        if let Some(stat) = git_output(git_exe, cwd, &["diff", "--stat", base]) {
            if !stat.is_empty() {
                return Some(stat);
            }
        }
    }
    None
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
        // The listing is still produced, with non-code files included.
        assert!(ctx.contains("Top-level directory listing:"), "ctx: {ctx}");
        assert!(ctx.contains("a.md"), "ctx: {ctx}");
        assert!(ctx.contains("b.csv"), "ctx: {ctx}");
        assert!(ctx.contains("image.png"), "ctx: {ctx}");
        assert!(ctx.contains("subdir"), "ctx: {ctx}");
    }

    #[test]
    fn chat_context_nonexistent_dir_is_empty() {
        let ctx = build_chat_context("/nonexistent/gremlins/chat/root");
        assert!(ctx.is_empty(), "ctx should be empty, got: {ctx}");
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
        assert!(ctx.contains("Top-level directory listing:"), "ctx: {ctx}");
        assert!(ctx.contains("note.md"), "ctx: {ctx}");
    }
}
