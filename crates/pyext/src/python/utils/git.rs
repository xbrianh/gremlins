//! Python bindings for [`gremlins::core::git`], exposed as
//! `_gremlins_core.utils.git`.
//!
//! Every binding mirrors the Python signature the module it replaces offered:
//! `cwd` is keyword-only, `ref`/`remote`/`fetch` keep their defaulted values,
//! and the three error shapes map onto a Python `GitError` (an `Exception`
//! carrying `returncode` and `stderr`), `OSError`, and `TimeoutError`.
//!
//! Blocking calls release the GIL through `py.detach`, matching the
//! conventions in [`crate::python::utils::proc`]. The pure predicates are
//! cheap enough that they stay attached, as `proc::run_ok` does.
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
pub(crate) async fn on_runtime<T, F>(fut: F) -> PyResult<T>
where
    F: Future<Output = Result<T, CoreGitError>> + Send + 'static,
    T: Send + 'static,
{
    pyo3_async_runtimes::tokio::get_runtime()
        .spawn(fut)
        .await
        .map_err(|e| PyRuntimeError::new_err(format!("tokio task failed: {e}")))?
        .map_err(map_git_error)
}

// --- Predicates and best-effort readers ---

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn in_git_repo(cwd: Option<PathBuf>) -> bool {
    git::in_git_repo(cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn head_sha(cwd: Option<PathBuf>) -> String {
    git::head_sha(cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn status_porcelain(cwd: Option<PathBuf>) -> String {
    git::status_porcelain(cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn has_dirty_worktree(cwd: Option<PathBuf>) -> bool {
    git::has_dirty_worktree(cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn has_commits(cwd: Option<PathBuf>) -> bool {
    git::has_commits(cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn current_branch(cwd: Option<PathBuf>) -> String {
    git::current_branch(cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (ref_a, ref_b, *, cwd=None))]
pub fn is_ancestor(ref_a: String, ref_b: String, cwd: Option<PathBuf>) -> bool {
    git::is_ancestor(&ref_a, &ref_b, cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (rev_range, *, cwd=None))]
pub fn log_oneline(rev_range: String, cwd: Option<PathBuf>) -> String {
    git::log_oneline(&rev_range, cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (rev_range, *, cwd=None))]
pub fn diff_stat(rev_range: String, cwd: Option<PathBuf>) -> String {
    git::diff_stat(&rev_range, cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (*, cwd=None))]
pub fn ls_others(cwd: Option<PathBuf>) -> String {
    git::ls_others(cwd.as_deref())
}

#[pyfunction]
#[pyo3(signature = (remote="origin".to_string(), *, cwd=None, timeout=None))]
pub fn try_fetch_all(
    py: Python<'_>,
    remote: String,
    cwd: Option<PathBuf>,
    timeout: Option<f64>,
) -> bool {
    py.detach(|| git::try_fetch_all(&remote, cwd.as_deref(), timeout))
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
#[pyo3(signature = (r, *, cwd=None))]
pub fn squash_merge(py: Python<'_>, r: String, cwd: Option<PathBuf>) -> PyResult<()> {
    detached(py, || git::squash_merge(&r, cwd.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (r#ref="HEAD".to_string(), *, cwd=None))]
pub fn reset_hard(py: Python<'_>, r#ref: String, cwd: Option<PathBuf>) -> PyResult<()> {
    detached(py, || git::reset_hard(&r#ref, cwd.as_deref()))
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
#[pyo3(signature = (r, *, cwd=None))]
pub fn ff_merge(py: Python<'_>, r: String, cwd: Option<PathBuf>) -> PyResult<()> {
    detached(py, || git::ff_merge(&r, cwd.as_deref()))
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
    project_root: String,
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
pub fn remove_worktree(py: Python<'_>, project_root: String, workdir: String) {
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
    project_root: String,
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
pub async fn remove_worktree_async(project_root: String, workdir: String) -> PyResult<()> {
    on_runtime(async move {
        git::remove_worktree_async(&project_root, &workdir).await;
        Ok(())
    })
    .await
}

#[pyfunction]
#[pyo3(signature = (project_root,))]
pub async fn prune_worktrees_async(project_root: String) -> PyResult<()> {
    on_runtime(async move {
        git::prune_worktrees_async(&project_root).await;
        Ok(())
    })
    .await
}

#[pyfunction]
#[pyo3(signature = (project_root, paths))]
pub async fn remove_worktrees_async(project_root: String, paths: Vec<String>) -> PyResult<()> {
    on_runtime(async move {
        git::remove_worktrees_async(&project_root, &paths).await;
        Ok(())
    })
    .await
}
