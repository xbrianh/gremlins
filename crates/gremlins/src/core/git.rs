//! Git operations, shelled out to the system `git` CLI.
//!
//! Companion to [`crate::core::proc`]: exit codes, timeouts, and process
//! groups live there, and this module contributes only the argument assembly
//! and the `git`-specific error vocabulary. Keeping the plumbing in one place
//! is what gives [`run_git`] its timeout and process-group semantics for free.
//!
//! Three return conventions are in play, mirroring the Python API this module
//! replaces:
//!
//! - *Predicates* (`in_git_repo`, `is_ancestor`, `has_commits`, …) return a
//!   bool and never raise; a missing `git` binary reads as `false`.
//! - *Best-effort readers* (`head_sha`, `status_porcelain`, `log_oneline`, …)
//!   return a string and yield `""` on failure.
//! - *Fallible operations* return [`GitError`], raised on a non-zero exit
//!   (when `check` semantics apply), a spawn failure, or a timeout.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::core::proc;

/// Failure modes of a git invocation.
///
/// [`Exit`](GitError::Exit) is reserved for non-zero exits and is raised only
/// when a caller asks for `check` semantics. [`Io`](GitError::Io) covers spawn
/// and filesystem failures, and [`Timeout`](GitError::Timeout) a run that
/// outlived its deadline. The pyext layer maps them to a Python `GitError`,
/// `OSError`, and `TimeoutError` respectively.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git exited {0}: {1}")]
    Exit(i32, String),
    #[error("failed to run git: {0}")]
    Io(String),
    #[error("git timed out after {0:.3}s")]
    Timeout(f64),
}

/// The output of a git invocation, carrying the exit code so callers can
/// branch on it instead of guessing from an empty stdout.
struct GitOutput {
    returncode: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn git_argv(args: &[&str]) -> Vec<String> {
    std::iter::once("git".to_string())
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect()
}

fn map_proc_error(error: proc::ProcError) -> GitError {
    match error {
        proc::ProcError::Io(e) => GitError::Io(e.to_string()),
        proc::ProcError::TimeoutExpired(t, _, _) => GitError::Timeout(t),
        proc::ProcError::CalledProcessError(rc, _, stderr) => {
            GitError::Exit(rc, String::from_utf8_lossy(&stderr).trim().to_string())
        }
        proc::ProcError::EmptyCommand => GitError::Io("empty command".to_string()),
        proc::ProcError::InvalidTimeout(t) => GitError::Io(format!("invalid timeout {t}")),
    }
}

fn stdout_text(output: &GitOutput) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Run `git <args>`, raising on a non-zero exit only when `check` is set.
fn run_git(
    args: &[&str],
    cwd: Option<&Path>,
    check: bool,
    timeout: Option<f64>,
) -> Result<GitOutput, GitError> {
    let output = proc::run(&git_argv(args), cwd, false, timeout).map_err(map_proc_error)?;
    if check && output.returncode != 0 {
        return Err(GitError::Exit(
            output.returncode,
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(GitOutput {
        returncode: output.returncode,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// The async counterpart of [`run_git`], always in `check: false` mode: the
/// async call sites inspect `returncode` themselves (and the ones that do not
/// are best-effort).
async fn run_git_async(
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Option<f64>,
) -> Result<GitOutput, GitError> {
    let output = proc::run_async(&git_argv(args), cwd, false, timeout, false, None)
        .await
        .map_err(map_proc_error)?;
    Ok(GitOutput {
        returncode: output.returncode,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Trimmed stdout of a best-effort call, or `""` on any failure.
fn stdout_or_empty(args: &[&str], cwd: Option<&Path>) -> String {
    match run_git(args, cwd, false, None) {
        Ok(output) => stdout_text(&output),
        Err(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Predicates and best-effort readers
// ---------------------------------------------------------------------------

/// Whether `cwd` is inside a git repository (or worktree).
pub fn in_git_repo(cwd: Option<&Path>) -> bool {
    matches!(
        run_git(&["rev-parse", "--git-dir"], cwd, false, None),
        Ok(output) if output.returncode == 0
    )
}

/// The async counterpart of [`in_git_repo`].
pub async fn in_git_repo_async(cwd: Option<&Path>) -> bool {
    matches!(
        run_git_async(&["rev-parse", "--git-dir"], cwd, None).await,
        Ok(output) if output.returncode == 0
    )
}

/// The SHA of `HEAD`, or `""` when it cannot be resolved.
pub fn head_sha(cwd: Option<&Path>) -> String {
    match run_git(&["rev-parse", "HEAD"], cwd, false, None) {
        Ok(output) if output.returncode == 0 => stdout_text(&output),
        _ => String::new(),
    }
}

/// The async counterpart of [`head_sha`].
pub async fn head_sha_async(cwd: Option<&Path>) -> String {
    match run_git_async(&["rev-parse", "HEAD"], cwd, None).await {
        Ok(output) if output.returncode == 0 => stdout_text(&output),
        _ => String::new(),
    }
}

/// Raw `git status --porcelain` output, or `""` when git cannot be run.
pub fn status_porcelain(cwd: Option<&Path>) -> String {
    match run_git(&["status", "--porcelain"], cwd, false, None) {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// The async counterpart of [`status_porcelain`].
pub async fn status_porcelain_async(cwd: Option<&Path>) -> String {
    match run_git_async(&["status", "--porcelain"], cwd, None).await {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Whether the worktree at `cwd` has staged or unstaged changes.
pub fn has_dirty_worktree(cwd: Option<&Path>) -> bool {
    !status_porcelain(cwd).trim().is_empty()
}

/// Whether `HEAD` exists and has at least one commit behind it.
pub fn has_commits(cwd: Option<&Path>) -> bool {
    match run_git(&["rev-list", "--count", "HEAD"], cwd, false, None) {
        Ok(output) if output.returncode == 0 => stdout_text(&output)
            .parse::<u64>()
            .is_ok_and(|count| count > 0),
        _ => false,
    }
}

/// The current branch name, or `""` for a detached `HEAD` or on failure.
pub fn current_branch(cwd: Option<&Path>) -> String {
    match run_git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd, false, None) {
        Ok(output) if output.returncode == 0 => {
            let branch = stdout_text(&output);
            if branch == "HEAD" {
                String::new()
            } else {
                branch
            }
        }
        _ => String::new(),
    }
}

/// Whether `ref_a` is an ancestor of `ref_b`. A failure reads as `false`.
pub fn is_ancestor(ref_a: &str, ref_b: &str, cwd: Option<&Path>) -> bool {
    matches!(
        run_git(&["merge-base", "--is-ancestor", ref_a, ref_b], cwd, false, None),
        Ok(output) if output.returncode == 0
    )
}

/// `git log --oneline <rev_range>`, or `""` on failure.
pub fn log_oneline(rev_range: &str, cwd: Option<&Path>) -> String {
    stdout_or_empty(&["log", "--oneline", rev_range], cwd)
}

/// `git diff --stat <rev_range>`, or `""` on failure.
pub fn diff_stat(rev_range: &str, cwd: Option<&Path>) -> String {
    stdout_or_empty(&["diff", "--stat", rev_range], cwd)
}

/// Untracked, non-ignored files, one per line, or `""` on failure.
pub fn ls_others(cwd: Option<&Path>) -> String {
    stdout_or_empty(&["ls-files", "--others", "--exclude-standard"], cwd)
}

/// Best-effort `git fetch <remote>`: `false` on a non-zero exit, a spawn
/// failure, or a timeout.
pub fn try_fetch_all(remote: &str, cwd: Option<&Path>, timeout: Option<f64>) -> bool {
    match run_git(&["fetch", remote], cwd, false, timeout) {
        Ok(output) => output.returncode == 0,
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Fallible operations
// ---------------------------------------------------------------------------

/// Resolve a symbolic ref name to `(sym_name, sha)`.
///
/// `"current"` is the running branch (falling back to the SHA for a detached
/// `HEAD`); anything else is tried as a local branch, a remote-tracking ref, a
/// tag, and finally a raw object name.
pub fn resolve_base_ref(name: &str, cwd: Option<&Path>) -> Result<(String, String), GitError> {
    if name == "current" {
        let sha = head_sha(cwd);
        if sha.is_empty() {
            return Err(GitError::Exit(
                128,
                "could not resolve HEAD: no commits".to_string(),
            ));
        }
        let branch = current_branch(cwd);
        let sym_name = if branch.is_empty() {
            sha.clone()
        } else {
            branch
        };
        return Ok((sym_name, sha));
    }

    for refpath in [
        format!("refs/heads/{name}"),
        format!("refs/remotes/{name}"),
        format!("refs/tags/{name}"),
        name.to_string(), // raw SHA or other direct ref
    ] {
        let output = run_git(&["rev-parse", "--verify", &refpath], cwd, false, None)?;
        if output.returncode == 0 {
            return Ok((name.to_string(), stdout_text(&output)));
        }
    }

    Err(GitError::Exit(
        128,
        format!("base_ref {name:?} does not resolve to a branch, tag, or commit"),
    ))
}

/// The merge base of two refs.
pub fn merge_base(ref_a: &str, ref_b: &str, cwd: Option<&Path>) -> Result<String, GitError> {
    let output = run_git(&["merge-base", ref_a, ref_b], cwd, true, None)?;
    Ok(stdout_text(&output))
}

/// How many commits `rev_range` spans.
pub fn rev_list_count(rev_range: &str, cwd: Option<&Path>) -> Result<usize, GitError> {
    let output = run_git(&["rev-list", "--count", rev_range], cwd, true, None)?;
    let count = stdout_text(&output);
    count
        .parse()
        .map_err(|_| GitError::Io(format!("could not parse rev-list count {count:?}")))
}

/// The absolute path of the git toplevel.
pub fn toplevel(cwd: Option<&Path>) -> Result<String, GitError> {
    let output = run_git(&["rev-parse", "--show-toplevel"], cwd, true, None)?;
    Ok(stdout_text(&output))
}

/// Squash-merge `r` into the index without committing.
pub fn squash_merge(r: &str, cwd: Option<&Path>) -> Result<(), GitError> {
    run_git(&["merge", "--squash", r], cwd, true, None).map(|_| ())
}

/// Discard all changes, moving the worktree back to `r` (normally `"HEAD"`).
pub fn reset_hard(r: &str, cwd: Option<&Path>) -> Result<(), GitError> {
    run_git(&["reset", "--hard", r], cwd, true, None).map(|_| ())
}

/// Commit the index with `message`.
pub fn commit(message: &str, cwd: Option<&Path>) -> Result<(), GitError> {
    run_git(&["commit", "-m", message], cwd, true, None).map(|_| ())
}

/// Fast-forward merge `r` into the current branch.
pub fn ff_merge(r: &str, cwd: Option<&Path>) -> Result<(), GitError> {
    run_git(&["merge", "--ff-only", r], cwd, true, None).map(|_| ())
}

/// Point `branch` at `target` even when that discards commits.
pub fn force_update_branch(branch: &str, target: &str, cwd: Option<&Path>) -> Result<(), GitError> {
    run_git(&["branch", "-f", branch, target], cwd, true, None).map(|_| ())
}

/// Remove untracked files and directories. Best-effort; never raises.
pub fn clean_fd(cwd: Option<&Path>) {
    let _ = run_git(&["clean", "-fd"], cwd, false, None);
}

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

/// Add a detached worktree at `base_ref` and return its path.
///
/// The worktree lands in `worktree_parent`, or the process work root when none
/// is given, under a unique `aibg-gremlin.<token>` name.
pub fn setup_detached_worktree(
    project_root: &str,
    base_ref: &str,
    fetch: bool,
    worktree_parent: Option<&Path>,
) -> Result<String, GitError> {
    let root = Path::new(project_root);
    let effective_ref = fetch_then_head(root, base_ref, fetch)?;
    let workdir = new_worktree_path(worktree_parent)?;
    let workdir_str = workdir.to_string_lossy().into_owned();
    run_git(
        &["worktree", "add", "--detach", &workdir_str, &effective_ref],
        Some(root),
        true,
        None,
    )?;
    Ok(workdir_str)
}

/// The async counterpart of [`setup_detached_worktree`].
pub async fn setup_detached_worktree_async(
    project_root: &str,
    base_ref: &str,
    fetch: bool,
    worktree_parent: Option<&Path>,
) -> Result<String, GitError> {
    let root = Path::new(project_root);
    let effective_ref = if fetch {
        let output = run_git_async(&["fetch", "origin", "--", base_ref], Some(root), None).await?;
        if output.returncode != 0 {
            return Err(GitError::Exit(
                output.returncode,
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        "FETCH_HEAD"
    } else {
        base_ref
    };
    let workdir = new_worktree_path(worktree_parent)?;
    let workdir_str = workdir.to_string_lossy().into_owned();
    let output = run_git_async(
        &["worktree", "add", "--detach", &workdir_str, effective_ref],
        Some(root),
        None,
    )
    .await?;
    if output.returncode != 0 {
        return Err(GitError::Exit(
            output.returncode,
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(workdir_str)
}

/// Fetch `base_ref` from `origin`, returning the ref the worktree should use.
fn fetch_then_head(root: &Path, base_ref: &str, fetch: bool) -> Result<String, GitError> {
    if !fetch {
        return Ok(base_ref.to_string());
    }
    run_git(&["fetch", "origin", "--", base_ref], Some(root), true, None)?;
    Ok("FETCH_HEAD".to_string())
}

/// Create `worktree_parent` if needed and mint a unique worktree path inside it.
fn new_worktree_path(worktree_parent: Option<&Path>) -> Result<PathBuf, GitError> {
    let parent = worktree_parent
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::config::work_root);
    std::fs::create_dir_all(&parent).map_err(|e| GitError::Io(e.to_string()))?;
    Ok(parent.join(format!("aibg-gremlin.{}", random_worktree_token(6))))
}

/// 6 bytes of entropy rendered as 12 hex characters, from `/dev/urandom` when
/// it is available and a time/pid mix otherwise.
fn random_worktree_token(nbytes: usize) -> String {
    let mut buf = vec![0u8; nbytes];
    let filled = File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut buf))
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        for (i, byte) in buf.iter_mut().enumerate() {
            let shift = (i * 8) as u32;
            *byte = ((nanos >> shift) as u8) ^ (pid as u8).wrapping_mul((i as u8).wrapping_add(1));
        }
    }
    let mut token = String::with_capacity(nbytes * 2);
    for byte in &buf {
        let _ = std::fmt::Write::write_fmt(&mut token, format_args!("{byte:02x}"));
    }
    token
}

/// Remove a worktree and prune stale entries. Best-effort; never raises.
pub fn remove_worktree(project_root: &str, workdir: &str) {
    let cwd = Some(Path::new(project_root));
    let _ = run_git(
        &["worktree", "remove", "--force", workdir],
        cwd,
        false,
        None,
    );
    let _ = run_git(&["worktree", "prune"], cwd, false, None);
}

/// The async counterpart of [`remove_worktree`]. Best-effort; never raises.
pub async fn remove_worktree_async(project_root: &str, workdir: &str) {
    let _ = run_git_async(
        &["worktree", "remove", "--force", workdir],
        Some(Path::new(project_root)),
        None,
    )
    .await;
}

/// Prune stale worktree entries. No-op outside a repository; never raises.
pub async fn prune_worktrees_async(project_root: &str) {
    let root = Path::new(project_root);
    if !in_git_repo_async(Some(root)).await {
        return;
    }
    let _ = run_git_async(&["worktree", "prune"], Some(root), None).await;
}

/// Remove worktrees in bulk and prune stale entries. No-op outside a
/// repository; never raises.
pub async fn remove_worktrees_async(project_root: &str, paths: &[String]) {
    let root = Path::new(project_root);
    if !in_git_repo_async(Some(root)).await {
        return;
    }
    for path in paths {
        remove_worktree_async(project_root, path).await;
    }
    prune_worktrees_async(project_root).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::proc::ProcError;
    use std::io;

    #[test]
    fn non_zero_exit_maps_to_exit_with_trimmed_stderr() {
        let error = map_proc_error(ProcError::CalledProcessError(
            128,
            vec![],
            b" bad ref\n".to_vec(),
        ));
        assert!(
            matches!(error, GitError::Exit(128, ref stderr) if stderr == "bad ref"),
            "unexpected {error:?}"
        );
    }

    #[test]
    fn io_failure_maps_to_io() {
        let error = map_proc_error(ProcError::Io(io::Error::other("no such binary")));
        assert!(
            matches!(error, GitError::Io(ref msg) if msg == "no such binary"),
            "unexpected {error:?}"
        );
    }

    #[test]
    fn timeout_maps_to_timeout() {
        let error = map_proc_error(ProcError::TimeoutExpired(2.5, vec![], vec![]));
        assert!(
            matches!(error, GitError::Timeout(t) if t == 2.5),
            "unexpected {error:?}"
        );
    }

    #[test]
    fn malformed_invocations_map_to_io() {
        for error in [
            map_proc_error(ProcError::EmptyCommand),
            map_proc_error(ProcError::InvalidTimeout(-1.0)),
        ] {
            assert!(matches!(error, GitError::Io(_)), "unexpected {error:?}");
        }
    }

    #[test]
    fn git_argv_prefixes_the_binary() {
        assert_eq!(
            git_argv(&["rev-parse", "HEAD"]),
            vec![
                "git".to_string(),
                "rev-parse".to_string(),
                "HEAD".to_string()
            ]
        );
    }

    #[test]
    fn worktree_token_is_hex_of_twice_the_requested_bytes() {
        let token = random_worktree_token(6);
        assert_eq!(token.len(), 12, "token {token:?}");
        assert!(
            token.bytes().all(|b| b.is_ascii_hexdigit()),
            "token {token:?}"
        );
    }

    #[test]
    fn worktree_tokens_do_not_repeat() {
        let tokens: std::collections::HashSet<String> =
            (0..64).map(|_| random_worktree_token(6)).collect();
        assert_eq!(tokens.len(), 64);
    }
}
