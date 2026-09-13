use std::collections::HashMap;
use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyFileNotFoundError, PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyList, PyString};

use gremlins::core::proc;

create_exception!(_gremlins_core.utils.proc, CalledProcessError, PyException);
create_exception!(_gremlins_core.utils.proc, TimeoutExpired, PyException);

fn map_io_error(e: std::io::Error) -> PyErr {
    match e.raw_os_error() {
        Some(_) if e.kind() == std::io::ErrorKind::NotFound => {
            PyFileNotFoundError::new_err(e.to_string())
        }
        Some(errno) => PyOSError::new_err((errno, e.to_string())),
        None => PyOSError::new_err(e.to_string()),
    }
}

// --- Return / exception types ---

#[pyclass(name = "ProcResult", module = "_gremlins_core.utils.proc")]
pub struct ProcResult {
    #[pyo3(get)]
    args: Py<PyAny>,
    #[pyo3(get)]
    returncode: i32,
    #[pyo3(get)]
    stdout: Py<PyAny>,
    #[pyo3(get)]
    stderr: Py<PyAny>,
}

#[pymethods]
impl ProcResult {
    #[new]
    fn new(args: Py<PyAny>, returncode: i32, stdout: Py<PyAny>, stderr: Py<PyAny>) -> Self {
        ProcResult {
            args,
            returncode,
            stdout,
            stderr,
        }
    }
}

// --- Constructors ---

fn cmd_list(py: Python<'_>, cmd: &[String]) -> PyResult<Py<PyAny>> {
    Ok(PyList::new(py, cmd)?.into_any().unbind())
}

fn bytes_obj(py: Python<'_>, b: &[u8]) -> Py<PyAny> {
    PyBytes::new(py, b).into_any().unbind()
}

fn str_obj(py: Python<'_>, s: &str) -> Py<PyAny> {
    PyString::new(py, s).into_any().unbind()
}

fn proc_result(
    py: Python<'_>,
    args: Py<PyAny>,
    returncode: i32,
    stdout: Py<PyAny>,
    stderr: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    Ok(Py::new(
        py,
        ProcResult {
            args,
            returncode,
            stdout,
            stderr,
        },
    )?
    .into_any())
}

fn called_process_error(
    py: Python<'_>,
    returncode: i32,
    cmd: Py<PyAny>,
    stdout: Py<PyAny>,
    stderr: Py<PyAny>,
) -> PyResult<PyErr> {
    let err = PyErr::new::<CalledProcessError, _>(format!(
        "Command returned non-zero exit status {returncode}."
    ));
    let val = err.value(py);
    val.setattr("returncode", returncode)?;
    val.setattr("cmd", cmd)?;
    val.setattr("stdout", stdout)?;
    val.setattr("stderr", stderr)?;
    Ok(err)
}

fn timeout_expired(
    py: Python<'_>,
    cmd: Py<PyAny>,
    timeout: f64,
    stdout: Py<PyAny>,
    stderr: Py<PyAny>,
) -> PyResult<PyErr> {
    let err = PyErr::new::<TimeoutExpired, _>(format!("Command timed out after {timeout} seconds"));
    let val = err.value(py);
    val.setattr("cmd", cmd)?;
    val.setattr("timeout", timeout)?;
    val.setattr("stdout", stdout)?;
    val.setattr("stderr", stderr)?;
    Ok(err)
}

#[pyfunction]
#[pyo3(signature = (cmd, cwd=None))]
pub fn run_ok(cmd: Vec<String>, cwd: Option<PathBuf>) -> PyResult<bool> {
    proc::run_ok(&cmd, cwd.as_deref()).map_err(map_io_error)
}

#[pyfunction]
#[pyo3(signature = (cmd, cwd=None))]
pub fn run_quiet(py: Python<'_>, cmd: Vec<String>, cwd: Option<PathBuf>) -> PyResult<Py<PyAny>> {
    let result = py.detach(|| proc::run_quiet(&cmd, cwd.as_deref()));

    match result {
        Ok(r) => proc_result(py, cmd_list(py, &cmd)?, r.returncode, py.None(), py.None()),
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
        Err(proc::ProcError::CalledProcessError(rc, stdout, stderr)) => Err(called_process_error(
            py,
            rc,
            cmd_list(py, &cmd)?,
            bytes_obj(py, &stdout),
            bytes_obj(py, &stderr),
        )?),
        Err(proc::ProcError::TimeoutExpired(t, stdout, stderr)) => Err(timeout_expired(
            py,
            cmd_list(py, &cmd)?,
            t,
            bytes_obj(py, &stdout),
            bytes_obj(py, &stderr),
        )?),
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
        Ok(r) => proc_result(
            py,
            cmd_list(py, &cmd)?,
            r.returncode,
            bytes_obj(py, &r.stdout),
            bytes_obj(py, &r.stderr),
        ),
        Err(proc::ProcError::CalledProcessError(rc, stdout, stderr)) => Err(called_process_error(
            py,
            rc,
            cmd_list(py, &cmd)?,
            bytes_obj(py, &stdout),
            bytes_obj(py, &stderr),
        )?),
        Err(proc::ProcError::TimeoutExpired(t, stdout, stderr)) => Err(timeout_expired(
            py,
            cmd_list(py, &cmd)?,
            t,
            bytes_obj(py, &stdout),
            bytes_obj(py, &stderr),
        )?),
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
#[pyo3(signature = (cmd, cwd=None, check=false, text=true, timeout=None, env=None))]
pub fn run_async(
    py: Python<'_>,
    cmd: Vec<String>,
    cwd: Option<PathBuf>,
    check: bool,
    text: bool,
    timeout: Option<f64>,
    env: Option<HashMap<String, String>>,
) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let result =
            proc::run_async(&cmd, cwd.as_deref(), check, timeout, text, env.as_ref()).await;
        Python::attach(|py| match result {
            Ok(r) => {
                let args = cmd_list(py, &cmd)?;
                if text {
                    let stdout = String::from_utf8_lossy(&r.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&r.stderr).into_owned();
                    proc_result(
                        py,
                        args,
                        r.returncode,
                        str_obj(py, &stdout),
                        str_obj(py, &stderr),
                    )
                } else {
                    proc_result(
                        py,
                        args,
                        r.returncode,
                        bytes_obj(py, &r.stdout),
                        bytes_obj(py, &r.stderr),
                    )
                }
            }
            Err(proc::ProcError::CalledProcessError(rc, stdout, stderr)) => {
                let args = cmd_list(py, &cmd)?;
                let (stdout, stderr) = if text {
                    (
                        str_obj(py, &String::from_utf8_lossy(&stdout)),
                        str_obj(py, &String::from_utf8_lossy(&stderr)),
                    )
                } else {
                    (bytes_obj(py, &stdout), bytes_obj(py, &stderr))
                };
                Err(called_process_error(py, rc, args, stdout, stderr)?)
            }
            Err(proc::ProcError::TimeoutExpired(t, stdout, stderr)) => {
                let args = cmd_list(py, &cmd)?;
                let (stdout, stderr) = if text {
                    (
                        str_obj(py, &String::from_utf8_lossy(&stdout)),
                        str_obj(py, &String::from_utf8_lossy(&stderr)),
                    )
                } else {
                    (bytes_obj(py, &stdout), bytes_obj(py, &stderr))
                };
                Err(timeout_expired(py, args, t, stdout, stderr)?)
            }
            Err(proc::ProcError::Io(e)) => Err(map_io_error(e)),
            Err(proc::ProcError::EmptyCommand) => Err(PyValueError::new_err("empty command")),
            Err(proc::ProcError::InvalidTimeout(t)) => Err(PyValueError::new_err(format!(
                "timeout must be a finite non-negative number, got {t}"
            ))),
        })
    })
}

#[pyfunction]
#[pyo3(signature = (cmd, cwd=None, env=None, timeout=None))]
pub fn run_shell_async(
    py: Python<'_>,
    cmd: String,
    cwd: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    timeout: Option<f64>,
) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let result = proc::run_shell_async(&cmd, cwd.as_deref(), env.as_ref(), timeout).await;
        Python::attach(|py| match result {
            Ok(r) => {
                let stdout = String::from_utf8_lossy(&r.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&r.stderr).into_owned();
                proc_result(
                    py,
                    str_obj(py, &cmd),
                    r.returncode,
                    str_obj(py, &stdout),
                    str_obj(py, &stderr),
                )
            }
            Err(proc::ProcError::TimeoutExpired(t, stdout, stderr)) => {
                let stdout = String::from_utf8_lossy(&stdout).into_owned();
                let stderr = format!(
                    "{}timed out after {}s\n",
                    String::from_utf8_lossy(&stderr),
                    t
                );
                proc_result(
                    py,
                    str_obj(py, &cmd),
                    124,
                    str_obj(py, &stdout),
                    str_obj(py, &stderr),
                )
            }
            Err(proc::ProcError::Io(e)) => Err(map_io_error(e)),
            Err(proc::ProcError::EmptyCommand) => Err(PyValueError::new_err("empty command")),
            Err(proc::ProcError::InvalidTimeout(t)) => Err(PyValueError::new_err(format!(
                "timeout must be a finite non-negative number, got {t}"
            ))),
            Err(_) => unreachable!(), // CalledProcessError never produced by run_shell_async
        })
    })
}
