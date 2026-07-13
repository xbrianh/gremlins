use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString};

use crate::core::proc::{self, ProcError, ProcResult};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn to_py_completed_process<'py>(
    py: Python<'py>,
    cmd: &[String],
    result: &ProcResult,
    text: bool,
) -> PyResult<Py<PyAny>> {
    let subprocess = PyModule::import(py, "subprocess")?;
    let completed_process = subprocess.getattr("CompletedProcess")?;

    let args: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();

    let py_stdout: Py<PyAny> = match (&result.stdout, text) {
        (Some(data), true) => String::from_utf8_lossy(data)
            .into_owned()
            .into_pyobject(py)?
            .into(),
        (Some(data), false) => PyBytes::new(py, data).into_pyobject(py)?.into(),
        (None, _) => py.None().into_pyobject(py)?.into(),
    };

    let py_stderr: Py<PyAny> = match (&result.stderr, text) {
        (Some(data), true) => String::from_utf8_lossy(data)
            .into_owned()
            .into_pyobject(py)?
            .into(),
        (Some(data), false) => PyBytes::new(py, data).into_pyobject(py)?.into(),
        (None, _) => py.None().into_pyobject(py)?.into(),
    };

    let obj = completed_process.call1((args, result.returncode, py_stdout, py_stderr))?;
    Ok(obj.unbind())
}

fn raise_proc_error(py: Python<'_>, cmd: &[String], err: &ProcError) -> PyErr {
    match err {
        ProcError::CalledProcessError {
            returncode,
            stdout,
            stderr,
        } => {
            let subprocess = PyModule::import(py, "subprocess").unwrap();
            let exc = subprocess.getattr("CalledProcessError").unwrap();
            let args: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            // Always pass output as string for CalledProcessError (matches Python convention)
            let stdout_s = String::from_utf8_lossy(stdout).into_owned();
            let stderr_s = String::from_utf8_lossy(stderr).into_owned();
            PyErr::from_value(
                exc.call1((*returncode, args, stdout_s, stderr_s))
                    .unwrap(),
            )
        }
        ProcError::TimeoutExpired { cmd, timeout } => {
            let subprocess = PyModule::import(py, "subprocess").unwrap();
            let exc = subprocess.getattr("TimeoutExpired").unwrap();
            let args: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            PyErr::from_value(exc.call1((args, *timeout)).unwrap())
        }
        ProcError::Io(e) => {
            if let Some(code) = e.raw_os_error() {
                let os = PyModule::import(py, "os").unwrap();
                let msg = os.call_method1("strerror", (code,)).unwrap();
                PyErr::new::<pyo3::exceptions::PyOSError, _>(msg.to_string())
            } else {
                PyErr::new::<pyo3::exceptions::PyOSError, _>(e.to_string())
            }
        }
        ProcError::ProcessLookup => {
            PyErr::new::<pyo3::exceptions::PyProcessLookupError, _>("ProcessLookupError")
        }
    }
}

fn parse_cmd(cmd: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    if let Ok(s) = cmd.extract::<String>() {
        return Ok(vec![s]);
    }
    let seq: Vec<String> = cmd.extract()?;
    Ok(seq)
}

fn parse_cwd(cwd: Option<&Bound<'_, PyAny>>) -> PyResult<Option<PathBuf>> {
    match cwd {
        None => Ok(None),
        Some(c) => {
            if c.is_none() {
                Ok(None)
            } else if let Ok(s) = c.extract::<String>() {
                Ok(Some(PathBuf::from(s)))
            } else {
                let path: PathBuf = c.extract()?;
                Ok(Some(path))
            }
        }
    }
}

fn validate_timeout(timeout: Option<f64>) -> PyResult<Option<Duration>> {
    match timeout {
        None => Ok(None),
        Some(t) => {
            if t.is_nan() || t.is_infinite() || t < 0.0 {
                return Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
                    "timeout must be a non-negative finite number, got {t:?}"
                )));
            }
            Ok(Some(Duration::from_secs_f64(t)))
        }
    }
}

// ---------------------------------------------------------------------------
// Sync functions
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None, check=false, text=true, timeout=None))]
fn run(
    py: Python<'_>,
    cmd: &Bound<'_, PyAny>,
    cwd: Option<&Bound<'_, PyAny>>,
    check: bool,
    text: bool,
    timeout: Option<f64>,
) -> PyResult<Py<PyAny>> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;
    let timeout = validate_timeout(timeout)?;

    let result = py.detach(|| proc::spawn_sync(&cmd, cwd.as_deref(), true, check, timeout));

    Python::try_attach(|py| match result {
        Ok(result) => to_py_completed_process(py, &cmd, &result, text),
        Err(err) => Err(raise_proc_error(py, &cmd, &err)),
    })
    .ok_or_else(|| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("GIL not held"))?
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn run_ok(py: Python<'_>, cmd: &Bound<'_, PyAny>, cwd: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    let result = py.detach(|| proc::spawn_sync(&cmd, cwd.as_deref(), false, false, None));

    match result {
        Ok(result) => Ok(result.returncode == 0),
        Err(_) => Ok(false),
    }
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn run_quiet(
    py: Python<'_>,
    cmd: &Bound<'_, PyAny>,
    cwd: Option<&Bound<'_, PyAny>>,
) -> PyResult<Py<PyAny>> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    let result = py.detach(|| proc::spawn_sync(&cmd, cwd.as_deref(), false, false, None));

    Python::try_attach(|py| match result {
        Ok(result) => {
            let subprocess = PyModule::import(py, "subprocess")?;
            let completed_process = subprocess.getattr("CompletedProcess")?;
            let args: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            let obj = completed_process.call1((args, result.returncode))?;
            Ok(obj.unbind())
        }
        Err(err) => Err(raise_proc_error(py, &cmd, &err)),
    })
    .ok_or_else(|| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("GIL not held"))?
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn run_or_raise(
    py: Python<'_>,
    cmd: &Bound<'_, PyAny>,
    cwd: Option<&Bound<'_, PyAny>>,
) -> PyResult<String> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    let result = py.detach(|| proc::spawn_sync(&cmd, cwd.as_deref(), true, true, None));

    match result {
        Ok(result) => {
            let stdout = String::from_utf8_lossy(result.stdout.as_ref().unwrap_or(&vec![]))
                .trim()
                .to_string();
            Ok(stdout)
        }
        Err(err) => Python::try_attach(|py| Err(raise_proc_error(py, &cmd, &err)))
            .ok_or_else(|| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("GIL not held"))?,
    }
}

// ---------------------------------------------------------------------------
// Async functions (underscore-prefixed raw exports, wrapped in Python)
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None, check=false, text=true, timeout=None))]
fn _run_async<'py>(
    py: Python<'py>,
    cmd: &Bound<'py, PyAny>,
    cwd: Option<&Bound<'py, PyAny>>,
    check: bool,
    text: bool,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;
    let timeout = validate_timeout(timeout)?;

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        match proc::spawn_async(&cmd, cwd.as_deref(), check, timeout).await {
            Ok(result) => Python::attach(|py| to_py_completed_process(py, &cmd, &result, text)),
            Err(err) => Python::attach(|py| Err(raise_proc_error(py, &cmd, &err))),
        }
    })
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None, env=None, timeout=None))]
fn _run_shell_async<'py>(
    py: Python<'py>,
    cmd: &Bound<'py, PyString>,
    cwd: Option<&Bound<'py, PyAny>>,
    env: Option<&Bound<'py, PyDict>>,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let cmd_str = cmd.to_string();
    let cwd = parse_cwd(cwd)?;
    let timeout = validate_timeout(timeout)?;

    let env_map: Option<HashMap<String, String>> = env.map(|d| {
        let mut map = HashMap::new();
        for (k, v) in d.iter() {
            if let (Ok(k), Ok(v)) = (k.extract::<String>(), v.extract::<String>()) {
                map.insert(k, v);
            }
        }
        map
    });

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        match proc::spawn_shell_async(&cmd_str, cwd.as_deref(), env_map.as_ref(), timeout).await {
            Ok(result) => {
                let cmd_vec = vec![cmd_str.clone()];
                Python::attach(|py| to_py_completed_process(py, &cmd_vec, &result, true))
            }
            Err(err) => {
                let cmd_vec = vec![cmd_str.clone()];
                Python::attach(|py| Err(raise_proc_error(py, &cmd_vec, &err)))
            }
        }
    })
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn _run_ok_async<'py>(
    py: Python<'py>,
    cmd: &Bound<'py, PyAny>,
    cwd: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        match proc::spawn_async_nullio(&cmd, cwd.as_deref()).await {
            Ok(Some(code)) => Ok(code == 0),
            Ok(None) => Ok(false),
            Err(_) => Ok(false),
        }
    })
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn _run_quiet_async<'py>(
    py: Python<'py>,
    cmd: &Bound<'py, PyAny>,
    cwd: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        match proc::spawn_async_nullio(&cmd, cwd.as_deref()).await {
            Ok(Some(code)) => Ok(code),
            Ok(None) => Ok(-1),
            Err(_) => Ok(-1),
        }
    })
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn _run_or_raise_async<'py>(
    py: Python<'py>,
    cmd: &Bound<'py, PyAny>,
    cwd: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        match proc::spawn_async(&cmd, cwd.as_deref(), true, None).await {
            Ok(result) => {
                let stdout = String::from_utf8_lossy(result.stdout.as_ref().unwrap_or(&vec![]))
                    .trim()
                    .to_string();
                Ok(stdout)
            }
            Err(err) => Python::attach(|py| Err(raise_proc_error(py, &cmd, &err))),
        }
    })
}

#[pyfunction]
#[pyo3(signature = (pid, *, grace_s=10.0))]
fn _terminate_with_grace<'py>(
    py: Python<'py>,
    pid: i32,
    grace_s: f64,
) -> PyResult<Bound<'py, PyAny>> {
    if grace_s.is_nan() || grace_s.is_infinite() || grace_s < 0.0 {
        return Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
            "grace_s must be a non-negative finite number, got {grace_s:?}"
        )));
    }
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        proc::terminate_with_grace(pid, grace_s)
            .await
            .map_err(|e| Python::attach(|py| raise_proc_error(py, &[], &e)))
    })
}

// ---------------------------------------------------------------------------
// Module registration helper
// ---------------------------------------------------------------------------

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(run, m)?)?;
    m.add_function(wrap_pyfunction!(run_ok, m)?)?;
    m.add_function(wrap_pyfunction!(run_quiet, m)?)?;
    m.add_function(wrap_pyfunction!(run_or_raise, m)?)?;
    m.add_function(wrap_pyfunction!(_run_async, m)?)?;
    m.add_function(wrap_pyfunction!(_run_shell_async, m)?)?;
    m.add_function(wrap_pyfunction!(_run_ok_async, m)?)?;
    m.add_function(wrap_pyfunction!(_run_quiet_async, m)?)?;
    m.add_function(wrap_pyfunction!(_run_or_raise_async, m)?)?;
    m.add_function(wrap_pyfunction!(_terminate_with_grace, m)?)?;
    Ok(())
}