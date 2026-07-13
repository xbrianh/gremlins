use std::path::PathBuf;

use pyo3::exceptions::{PyFileNotFoundError, PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyType;

use crate::core::proc;

fn map_io_error(e: std::io::Error) -> PyErr {
    match e.raw_os_error() {
        Some(_) if e.kind() == std::io::ErrorKind::NotFound => {
            PyFileNotFoundError::new_err(e.to_string())
        }
        Some(errno) => PyOSError::new_err((errno, e.to_string())),
        None => PyOSError::new_err(e.to_string()),
    }
}

#[pyfunction]
#[pyo3(signature = (cmd, cwd=None))]
pub fn run_ok(cmd: Vec<String>, cwd: Option<PathBuf>) -> PyResult<bool> {
    proc::run_ok(&cmd, cwd.as_deref()).map_err(map_io_error)
}

fn subprocess_type<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyType>> {
    let ty = py.import("subprocess")?.getattr(name)?;
    Ok(ty.cast::<PyType>()?.clone())
}

#[pyfunction]
#[pyo3(signature = (cmd, cwd=None, check=false, timeout=None))]
pub fn run(
    py: Python<'_>,
    cmd: Vec<String>,
    cwd: Option<PathBuf>,
    check: bool,
    timeout: Option<f64>,
) -> PyResult<Py<PyAny>> {
    // Release the GIL so other Python threads can run while the child process
    // executes (matching subprocess.run behavior).
    let result = py.detach(|| proc::run(&cmd, cwd.as_deref(), check, timeout));

    match result {
        Ok(r) => {
            let ty = subprocess_type(py, "CompletedProcess")?;
            let obj = ty.call1((cmd, r.returncode, r.stdout, r.stderr))?;
            Ok(obj.into_any().unbind())
        }
        Err(proc::ProcError::CalledProcessError(rc, stdout, stderr)) => {
            let ty = subprocess_type(py, "CalledProcessError")?;
            let obj = ty.call1((rc, cmd, stdout, stderr))?;
            Err(PyErr::from_value(obj))
        }
        Err(proc::ProcError::TimeoutExpired(t, stdout, stderr)) => {
            let ty = subprocess_type(py, "TimeoutExpired")?;
            let obj = ty.call1((cmd, t, stdout, stderr))?;
            Err(PyErr::from_value(obj))
        }
        Err(proc::ProcError::Io(e)) => Err(map_io_error(e)),
        Err(proc::ProcError::EmptyCommand) => Err(PyValueError::new_err("empty command")),
        Err(proc::ProcError::InvalidTimeout(t)) => {
            Err(PyValueError::new_err(format!(
                "timeout must be a finite non-negative number, got {t}"
            )))
        }
    }
}