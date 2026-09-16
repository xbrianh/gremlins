//! Python bindings for [`gremlins::core::env_file`], exposed as
//! `_gremlins_core.utils.env_file`.
//!
//! Both functions mirror the Python signatures the module they replace
//! offered: `base_env` and `cwd` are keyword-only, and `cwd` is optional.
//! Every failure — a missing `bash`, a script that exits non-zero — surfaces
//! as a `RuntimeError`, which is exactly what the Python call sites already
//! catch and what the old module raised. The message carries the detail, so no
//! bespoke exception type is needed.
//!
//! Sourcing shells out, so it is blocking work; each call releases the GIL
//! through `py.detach` for as long as `bash` runs.

use std::collections::HashMap;
use std::path::PathBuf;

use gremlins::core::env_file::{self, EnvFileError};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// Translate a core sourcing failure into the Python exception callers expect.
///
/// [`EnvFileError`]'s `Display` output is already the message the Python module
/// produced, so the mapping is the message and nothing more.
fn map_env_file_error(error: EnvFileError) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

/// Run a blocking loading call with the GIL released, mapping its error.
fn detached<T, F>(py: Python<'_>, call: F) -> PyResult<T>
where
    F: FnOnce() -> Result<T, EnvFileError> + Send,
    T: Send,
{
    py.detach(call).map_err(map_env_file_error)
}

#[pyfunction]
#[pyo3(signature = (path, *, base_env, cwd=None))]
pub fn load_env_file_isolated(
    py: Python<'_>,
    path: PathBuf,
    base_env: HashMap<String, String>,
    cwd: Option<PathBuf>,
) -> PyResult<HashMap<String, String>> {
    detached(py, || {
        env_file::load_env_file_isolated(&path, &base_env, cwd.as_deref())
    })
}

#[pyfunction]
#[pyo3(signature = (script, *, base_env, cwd=None))]
pub fn source_env_string(
    py: Python<'_>,
    script: String,
    base_env: HashMap<String, String>,
    cwd: Option<PathBuf>,
) -> PyResult<HashMap<String, String>> {
    detached(py, || {
        env_file::source_env_string(&script, &base_env, cwd.as_deref())
    })
}
