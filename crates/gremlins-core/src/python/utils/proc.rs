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
#[pyo3(signature = (cmd, cwd=None))]
pub fn run_quiet(py: Python<'_>, cmd: Vec<String>, cwd: Option<PathBuf>) -> PyResult<Py<PyAny>> {
    let result = py.detach(|| proc::run_quiet(&cmd, cwd.as_deref()));

    match result {
        Ok(r) => {
            let ty = subprocess_type(py, "CompletedProcess")?;
            let obj = ty.call1((cmd, r.returncode))?;
            Ok(obj.into_any().unbind())
        }
        Err(proc::ProcError::Io(e)) => Err(map_io_error(e)),
        Err(proc::ProcError::EmptyCommand) => Err(PyValueError::new_err("empty command")),
        Err(_) => unreachable!(), // CalledProcessError and TimeoutExpired never produced by run_quiet
    }
}

#[pyfunction]
#[pyo3(signature = (cmd, cwd=None))]
pub fn run_or_raise(py: Python<'_>, cmd: Vec<String>, cwd: Option<PathBuf>) -> PyResult<String> {
    let result = py.detach(|| proc::run_or_raise(&cmd, cwd.as_deref()));

    match result {
        Ok(s) => Ok(s),
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
        Err(proc::ProcError::InvalidTimeout(t)) => Err(PyValueError::new_err(format!(
            "timeout must be a finite non-negative number, got {t}"
        ))),
    }
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
        Err(proc::ProcError::InvalidTimeout(t)) => Err(PyValueError::new_err(format!(
            "timeout must be a finite non-negative number, got {t}"
        ))),
    }
}

#[pyfunction]
#[pyo3(signature = (cmd, cwd=None))]
pub fn run_ok_async(
    py: Python<'_>,
    cmd: Vec<String>,
    cwd: Option<PathBuf>,
) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let result = proc::run_ok_async(&cmd, cwd.as_deref()).await;
        result.map_err(map_io_error)
    })
}

#[cfg(unix)]
#[pyfunction]
#[pyo3(signature = (pid, *, grace_s=10.0))]
pub fn terminate_with_grace(py: Python<'_>, pid: u32, grace_s: f64) -> PyResult<Bound<'_, PyAny>> {
    if pid > i32::MAX as u32 {
        return Err(PyValueError::new_err(format!(
            "pid {pid} exceeds the maximum supported value {}",
            i32::MAX
        )));
    }
    if !grace_s.is_finite() || grace_s < 0.0 {
        return Err(PyValueError::new_err(format!(
            "grace_s must be a finite non-negative number, got {grace_s}"
        )));
    }
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        proc::terminate_with_grace(pid, grace_s).await;
        Ok(())
    })
}

#[cfg(not(unix))]
#[pyfunction]
#[pyo3(signature = (pid, *, grace_s=10.0))]
pub fn terminate_with_grace(py: Python<'_>, pid: u32, grace_s: f64) -> PyResult<Bound<'_, PyAny>> {
    if pid > i32::MAX as u32 {
        return Err(PyValueError::new_err(format!(
            "pid {pid} exceeds the maximum supported value {}",
            i32::MAX
        )));
    }
    if !grace_s.is_finite() || grace_s < 0.0 {
        return Err(PyValueError::new_err(format!(
            "grace_s must be a finite non-negative number, got {grace_s}"
        )));
    }
    pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(()) })
}

#[pyfunction]
#[pyo3(signature = (cmd, cwd=None, check=false, text=true, timeout=None))]
pub fn run_async(
    py: Python<'_>,
    cmd: Vec<String>,
    cwd: Option<PathBuf>,
    check: bool,
    text: bool,
    timeout: Option<f64>,
) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let result = proc::run_async(&cmd, cwd.as_deref(), check, timeout, text).await;
        Python::attach(|py| match result {
            Ok(r) => {
                let ty = subprocess_type(py, "CompletedProcess")?;
                if text {
                    let stdout = String::from_utf8_lossy(&r.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&r.stderr).into_owned();
                    let obj = ty.call1((cmd, r.returncode, stdout, stderr))?;
                    Ok(obj.into_any().unbind())
                } else {
                    let obj = ty.call1((cmd, r.returncode, r.stdout, r.stderr))?;
                    Ok(obj.into_any().unbind())
                }
            }
            Err(proc::ProcError::CalledProcessError(rc, stdout, stderr)) => {
                let ty = subprocess_type(py, "CalledProcessError")?;
                let obj = if text {
                    let stdout = String::from_utf8_lossy(&stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&stderr).into_owned();
                    ty.call1((rc, cmd, stdout, stderr))?
                } else {
                    ty.call1((rc, cmd, stdout, stderr))?
                };
                Err(PyErr::from_value(obj))
            }
            Err(proc::ProcError::TimeoutExpired(t, stdout, stderr)) => {
                let ty = subprocess_type(py, "TimeoutExpired")?;
                let obj = if text {
                    let stdout = String::from_utf8_lossy(&stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&stderr).into_owned();
                    ty.call1((cmd, t, stdout, stderr))?
                } else {
                    ty.call1((cmd, t, stdout, stderr))?
                };
                Err(PyErr::from_value(obj))
            }
            Err(proc::ProcError::Io(e)) => Err(map_io_error(e)),
            Err(proc::ProcError::EmptyCommand) => Err(PyValueError::new_err("empty command")),
            Err(proc::ProcError::InvalidTimeout(t)) => Err(PyValueError::new_err(format!(
                "timeout must be a finite non-negative number, got {t}"
            ))),
        })
    })
}
