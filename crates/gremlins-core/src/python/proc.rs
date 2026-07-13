use std::path::PathBuf;

use pyo3::exceptions::PyOSError;
use pyo3::prelude::*;

use crate::core::proc;

#[pyfunction]
pub fn run_ok(cmd: Vec<String>, cwd: Option<PathBuf>) -> PyResult<bool> {
    proc::run_ok(&cmd, cwd.as_deref()).map_err(|e| PyOSError::new_err(e.to_string()))
}
