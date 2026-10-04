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

use std::path::Path;

use crate::core::proc;

/// Failure modes of a git invocation.
///
/// [`Exit`](GitError::Exit) is reserved for non-zero exits and is raised only
/// when a caller asks for `check` semantics. [`Io`](GitError::Io) covers spawn
/// and filesystem failures, and [`Timeout`](GitError::Timeout) a run that
/// outlived its deadline.
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
pub(crate) struct GitOutput {
    returncode: i32,
    stdout: Vec<u8>,
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
pub(crate) fn run_git(
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

/// Raw `git status --porcelain` output, or `""` when git cannot be run or
/// exits non-zero.
pub fn status_porcelain(cwd: Option<&Path>) -> String {
    match run_git(&["status", "--porcelain"], cwd, false, None) {
        Ok(output) if output.returncode == 0 => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => String::new(),
    }
}

/// The async counterpart of [`status_porcelain`].
pub async fn status_porcelain_async(cwd: Option<&Path>) -> String {
    match run_git_async(&["status", "--porcelain"], cwd, None).await {
        Ok(output) if output.returncode == 0 => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => String::new(),
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
}
