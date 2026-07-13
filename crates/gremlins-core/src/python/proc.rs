use std::path::PathBuf;

use pyo3::exceptions::{PyFileNotFoundError, PyOSError};
use pyo3::prelude::*;

use crate::core::proc;

#[pyfunction]
pub fn run_ok(cmd: Vec<String>, cwd: Option<PathBuf>) -> PyResult<bool> {
    proc::run_ok(&cmd, cwd.as_deref()).map_err(|e| {
        match e.raw_os_error() {
            Some(_) if e.kind() == std::io::ErrorKind::NotFound => {
                PyFileNotFoundError::new_err(e.to_string())
            }
            Some(errno) => PyOSError::new_err((errno, e.to_string())),
            None => PyOSError::new_err(e.to_string()),
        }
    })
}
