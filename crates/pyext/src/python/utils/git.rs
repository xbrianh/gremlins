//! Python bindings for [`gremlins::core::git`], exposed as
//! `_gremlins_core.utils.git`.
//!
//! Every binding mirrors the Python signature the module it replaces offered:
//! `cwd` is keyword-only, `ref`/`remote`/`fetch` keep their defaulted values,
//! and the three error shapes map onto a Python `GitError` (an `Exception`
//! carrying `returncode` and `stderr`), `OSError`, and `TimeoutError`.
//!
//! Blocking calls release the GIL through `py.detach`, matching the
//! conventions in [`crate::python::utils::proc`], including the predicates
//! and best-effort readers that spawn git processes.
//!
//! The async bindings are genuine `async fn` coroutine functions rather than
//! `pyo3_async_runtimes::tokio::future_into_py` futures. That choice is
//! deliberate: `future_into_py` attaches its `asyncio.Future` to the *current*
//! event loop, so a call such as `asyncio.run(remove_worktrees_async(...))`
//! would fail while creating the argument, before a loop exists. A plain
//! coroutine function is loop-independent. Its body then has to drive the
//! Tokio-backed work itself, which is what [`on_runtime`] does.

use std::future::Future;
use std::path::PathBuf;

use gremlins::core::git::{self, GitError as CoreGitError};
use pyo3::exceptions::{PyException, PyOSError, PyRuntimeError, PyTimeoutError};
use pyo3::prelude::*;

/// The Python exception raised for a non-zero git exit.
#[pyclass(name = "GitError", module = "_gremlins_core.utils.git", extends = PyException)]
pub struct GitError {
    #[pyo3(get)]
    returncode: i32,
    #[pyo3(get)]
    stderr: String,
}

#[pymethods]
impl GitError {
    #[new]
    #[pyo3(signature = (returncode, stderr))]
    fn new(returncode: i32, stderr: String) -> Self {
        GitError { returncode, stderr }
    }

    fn __str__(&self) -> String {
        format!("git exited {}: {}", self.returncode, self.stderr)
    }
}

/// Translate a core git failure into the Python exception Python callers expect.
pub(crate) fn map_git_error(error: CoreGitError) -> PyErr {
    match error {
        CoreGitError::Exit(rc, stderr) => PyErr::new::<GitError, _>((rc, stderr)),
        CoreGitError::Io(msg) => PyOSError::new_err(msg),
        CoreGitError::Timeout(t) => PyTimeoutError::new_err(format!("git timed out after {t}s")),
    }
}

/// Run a blocking git call with the GIL released, mapping its error.
fn detached<T, F>(py: Python<'_>, call: F) -> PyResult<T>
where
    F: FnOnce() -> Result<T, CoreGitError> + Send,
    T: Send,
{
    py.detach(call).map_err(map_git_error)
}

/// Drive `fut` on the Tokio runtime the async bindings share.
///
/// An async binding is awaited by *asyncio*, which knows nothing of Tokio, so
/// the runtime-owning futures it depends on (process spawning, in
/// [`gremlins::core::proc`]) must be spawned onto Tokio explicitly. This is the
/// same runtime `future_into_py` hands work to, reached by name instead.
///
/// The spawned task is aborted when this future is dropped, so cancelling the
/// Python coroutine cancels the git operation rather than detaching it. Because
/// `proc` spawns children with `kill_on_drop`, the abort also reaps the child.
pub(crate) async fn on_runtime<T, F>(fut: F) -> PyResult<T>
where
    F: Future<Output = Result<T, CoreGitError>> + Send + 'static,
    T: Send + 'static,
{
    let handle = pyo3_async_runtimes::tokio::get_runtime().spawn(fut);
    let _abort_on_drop = AbortOnDrop(handle.abort_handle());
    handle
        .await
        .map_err(|e| PyRuntimeError::new_err(format!("tokio task failed: {e}")))?
        .map_err(map_git_error)
}

/// Aborts the wrapped task when dropped, propagating cancellation from the
/// Python coroutine to the Tokio task it drives.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// --- Predicates and best-effort readers ---

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn in_git_repo(py: Python<'_>, cwd: Option<PathBuf>) -> bool {
    py.detach(|| git::in_git_repo(cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn head_sha(py: Python<'_>, cwd: Option<PathBuf>) -> String {
    py.detach(|| git::head_sha(cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn status_porcelain(py: Python<'_>, cwd: Option<PathBuf>) -> String {
    py.detach(|| git::status_porcelain(cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn has_dirty_worktree(py: Python<'_>, cwd: Option<PathBuf>) -> bool {
    py.detach(|| git::has_dirty_worktree(cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn has_commits(py: Python<'_>, cwd: Option<PathBuf>) -> bool {
    py.detach(|| git::has_commits(cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn current_branch(py: Python<'_>, cwd: Option<PathBuf>) -> String {
    py.detach(|| git::current_branch(cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (ref_a, ref_b, *, cwd=None))]
pub fn is_ancestor(py: Python<'_>, ref_a: String, ref_b: String, cwd: Option<PathBuf>) -> bool {
    py.detach(|| git::is_ancestor(&ref_a, &ref_b, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (rev_range, *, cwd=None))]
pub fn log_oneline(py: Python<'_>, rev_range: String, cwd: Option<PathBuf>) -> String {
    py.detach(|| git::log_oneline(&rev_range, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (rev_range, *, cwd=None))]
pub fn diff_stat(py: Python<'_>, rev_range: String, cwd: Option<PathBuf>) -> String {
    py.detach(|| git::diff_stat(&rev_range, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn ls_others(py: Python<'_>, cwd: Option<PathBuf>) -> String {
    py.detach(|| git::ls_others(cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (remote="origin", *, cwd=None, timeout=None))]
pub fn try_fetch_all(
    py: Python<'_>,
    remote: &str,
    cwd: Option<PathBuf>,
    timeout: Option<f64>,
) -> bool {
    py.detach(|| git::try_fetch_all(remote, cwd.as_deref(), timeout))
}

// --- Fallible operations ---

#[pyfunction]
#[pyo3(signature = (name, *, cwd=None))]
pub fn resolve_base_ref(
    py: Python<'_>,
    name: String,
    cwd: Option<PathBuf>,
) -> PyResult<(String, String)> {
    detached(py, || git::resolve_base_ref(&name, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (ref_a, ref_b, *, cwd=None))]
pub fn merge_base(
    py: Python<'_>,
    ref_a: String,
    ref_b: String,
    cwd: Option<PathBuf>,
) -> PyResult<String> {
    detached(py, || git::merge_base(&ref_a, &ref_b, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (rev_range, *, cwd=None))]
pub fn rev_list_count(py: Python<'_>, rev_range: String, cwd: Option<PathBuf>) -> PyResult<usize> {
    detached(py, || git::rev_list_count(&rev_range, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn toplevel(py: Python<'_>, cwd: Option<PathBuf>) -> PyResult<String> {
    detached(py, || git::toplevel(cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (r#ref, *, cwd=None))]
pub fn squash_merge(py: Python<'_>, r#ref: String, cwd: Option<PathBuf>) -> PyResult<()> {
    detached(py, || git::squash_merge(&r#ref, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (r#ref="HEAD", *, cwd=None))]
pub fn reset_hard(py: Python<'_>, r#ref: &str, cwd: Option<PathBuf>) -> PyResult<()> {
    detached(py, || git::reset_hard(r#ref, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn clean_fd(py: Python<'_>, cwd: Option<PathBuf>) {
    py.detach(|| git::clean_fd(cwd.as_deref()));
}

#[pyfunction]
#[pyo3(signature = (message, *, cwd=None))]
pub fn commit(py: Python<'_>, message: String, cwd: Option<PathBuf>) -> PyResult<()> {
    detached(py, || git::commit(&message, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (r#ref, *, cwd=None))]
pub fn ff_merge(py: Python<'_>, r#ref: String, cwd: Option<PathBuf>) -> PyResult<()> {
    detached(py, || git::ff_merge(&r#ref, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (branch, target, *, cwd=None))]
pub fn force_update_branch(
    py: Python<'_>,
    branch: String,
    target: String,
    cwd: Option<PathBuf>,
) -> PyResult<()> {
    detached(py, || {
        git::force_update_branch(&branch, &target, cwd.as_deref())
    })
}

// --- Worktrees ---

#[pyfunction]
#[pyo3(signature = (project_root, base_ref, *, fetch=false, worktree_parent=None))]
pub fn setup_detached_worktree(
    py: Python<'_>,
    project_root: PathBuf,
    base_ref: String,
    fetch: bool,
    worktree_parent: Option<PathBuf>,
) -> PyResult<String> {
    detached(py, || {
        git::setup_detached_worktree(&project_root, &base_ref, fetch, worktree_parent.as_deref())
    })
}

#[pyfunction]
#[pyo3(signature = (project_root, workdir))]
pub fn remove_worktree(py: Python<'_>, project_root: PathBuf, workdir: String) {
    py.detach(|| git::remove_worktree(&project_root, &workdir));
}

// --- Async ---

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub async fn in_git_repo_async(cwd: Option<PathBuf>) -> bool {
    on_runtime(async move { Ok(git::in_git_repo_async(cwd.as_deref()).await) })
        .await
        .unwrap_or(false)
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub async fn head_sha_async(cwd: Option<PathBuf>) -> String {
    on_runtime(async move { Ok(git::head_sha_async(cwd.as_deref()).await) })
        .await
        .unwrap_or_default()
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub async fn status_porcelain_async(cwd: Option<PathBuf>) -> String {
    on_runtime(async move { Ok(git::status_porcelain_async(cwd.as_deref()).await) })
        .await
        .unwrap_or_default()
}

#[pyfunction]
#[pyo3(signature = (project_root, base_ref, *, fetch=false, worktree_parent=None))]
pub async fn setup_detached_worktree_async(
    project_root: PathBuf,
    base_ref: String,
    fetch: bool,
    worktree_parent: Option<PathBuf>,
) -> PyResult<String> {
    on_runtime(async move {
        git::setup_detached_worktree_async(
            &project_root,
            &base_ref,
            fetch,
            worktree_parent.as_deref(),
        )
        .await
    })
    .await
}

#[pyfunction]
#[pyo3(signature = (project_root, workdir))]
pub async fn remove_worktree_async(project_root: PathBuf, workdir: String) -> PyResult<()> {
    on_runtime(async move {
        git::remove_worktree_async(&project_root, &workdir).await;
        Ok(())
    })
    .await
}

#[pyfunction]
#[pyo3(signature = (project_root,))]
pub async fn prune_worktrees_async(project_root: PathBuf) -> PyResult<()> {
    on_runtime(async move {
        git::prune_worktrees_async(&project_root).await;
        Ok(())
    })
    .await
}

#[pyfunction]
#[pyo3(signature = (project_root, paths))]
pub async fn remove_worktrees_async(project_root: PathBuf, paths: Vec<String>) -> PyResult<()> {
    on_runtime(async move {
        git::remove_worktrees_async(&project_root, &paths).await;
        Ok(())
    })
    .await
}
