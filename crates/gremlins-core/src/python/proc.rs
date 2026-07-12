use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
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
            PyErr::from_value(
                exc.call1((*returncode, args, stdout.clone(), stderr.clone()))
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
            PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("ProcessLookupError")
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

// ---------------------------------------------------------------------------
// Sync functions
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None, check=false, text=true, timeout=None))]
fn run<'py>(
    py: Python<'py>,
    cmd: &Bound<'py, PyAny>,
    cwd: Option<&Bound<'py, PyAny>>,
    check: bool,
    text: bool,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;
    let timeout = timeout.map(Duration::from_secs_f64);

    match proc::spawn_sync(&cmd, cwd.as_deref(), true, check, timeout) {
        Ok(result) => {
            let obj = to_py_completed_process(py, &cmd, &result, text)?;
            Ok(obj.into_bound(py))
        }
        Err(err) => Err(raise_proc_error(py, &cmd, &err)),
    }
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn run_ok(cmd: &Bound<'_, PyAny>, cwd: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    match proc::spawn_sync(&cmd, cwd.as_deref(), false, false, None) {
        Ok(result) => Ok(result.returncode == 0),
        Err(_) => Ok(false),
    }
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn run_quiet<'py>(
    py: Python<'py>,
    cmd: &Bound<'py, PyAny>,
    cwd: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    match proc::spawn_sync(&cmd, cwd.as_deref(), false, false, None) {
        Ok(result) => {
            let subprocess = PyModule::import(py, "subprocess")?;
            let completed_process = subprocess.getattr("CompletedProcess")?;
            let args: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            let obj = completed_process.call1((args, result.returncode))?;
            Ok(obj)
        }
        Err(err) => Err(raise_proc_error(py, &cmd, &err)),
    }
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd=None))]
fn run_or_raise(cmd: &Bound<'_, PyAny>, cwd: Option<&Bound<'_, PyAny>>) -> PyResult<String> {
    let cmd = parse_cmd(cmd)?;
    let cwd = parse_cwd(cwd)?;

    match proc::spawn_sync(&cmd, cwd.as_deref(), true, true, None) {
        Ok(result) => {
            let stdout = String::from_utf8_lossy(result.stdout.as_ref().unwrap_or(&vec![]))
                .trim()
                .to_string();
            Ok(stdout)
        }
        Err(err) => Err(raise_proc_error(
            unsafe { Python::assume_attached() },
            &cmd,
            &err,
        )),
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
    let timeout = timeout.map(Duration::from_secs_f64);

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
    let timeout = timeout.map(Duration::from_secs_f64);

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
        let mut command = tokio::process::Command::new(&cmd[0]);
        command.args(&cmd[1..]);
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        command.kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(&cwd);
        }
        unsafe {
            command.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }

        match command.spawn() {
            Ok(mut child) => match child.wait().await {
                Ok(status) => Ok(status.code().unwrap_or(-1) == 0),
                Err(_) => Ok(false),
            },
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
        let mut command = tokio::process::Command::new(&cmd[0]);
        command.args(&cmd[1..]);
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        command.kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(&cwd);
        }
        unsafe {
            command.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }

        match command.spawn() {
            Ok(mut child) => match child.wait().await {
                Ok(status) => Ok(status.code().unwrap_or(-1)),
                Err(_) => Ok(-1),
            },
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
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        proc::terminate_with_grace(pid, grace_s)
            .await
            .map_err(|e| Python::attach(|py| raise_proc_error(py, &[], &e)))
    })
}

#[pyfunction]
#[pyo3(signature = (pid, timeout_s, child_key))]
fn _wait_child_proc<'py>(
    py: Python<'py>,
    pid: i32,
    timeout_s: Option<f64>,
    child_key: &Bound<'py, PyString>,
) -> PyResult<Bound<'py, PyAny>> {
    let child_key = child_key.to_string();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        match proc::wait_child_proc(pid, timeout_s, &child_key).await {
            Ok(()) => Ok(()),
            Err(ProcError::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                Python::attach(|_py| {
                    Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                        e.to_string(),
                    ))
                })
            }
            Err(err) => Python::attach(|py| Err(raise_proc_error(py, &[], &err))),
        }
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
    m.add_function(wrap_pyfunction!(_wait_child_proc, m)?)?;
    Ok(())
}
