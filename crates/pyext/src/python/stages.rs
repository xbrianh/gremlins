use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::stream::StreamExt;
use gremlins::core::proc;
use gremlins::executor::state as rust_state;
use gremlins::stages::agent as rust_agent;
use gremlins::stages::base;
use gremlins::stages::composite::{
    compute_child_params, get_client_from_dict as rust_get_client_from_dict,
    StageAttrs as RustStageAttrs,
};
use gremlins::stages::constants::{BAIL_KEY, FRAMEWORK_KEYS};
use gremlins::stages::exec as rust_exec;
use gremlins::stages::outcome::Done as RustDone;
use gremlins::stages::parallel::BailPolicy;
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict, PyFrozenSet, PyList, PyString, PyTuple, PyType};
use tokio::sync::Semaphore;

use crate::python::artifacts::{ArtifactRegistry, MissingArtifact};
use crate::python::clients::Client;
use crate::python::executor::{PyState, PyStateData};
use crate::python::json_conv::{py_to_value as py_to_json, value_to_py as json_value_to_py};
use crate::schemas::loader;

// Type alias for the future returned by into_future
// pyo3_async_runtimes::tokio::into_future returns a boxed future
// that resolves to PyResult<Py<PyAny>>.
type PyAwaitable = Pin<Box<dyn Future<Output = PyResult<Py<PyAny>>> + Send>>;

// --- Helpers ---

/// Convert a PyDict of string keys to a HashMap<String, serde_json::Value>.
fn extract_json_value_dict(obj: &Bound<'_, PyAny>) -> PyResult<HashMap<String, serde_json::Value>> {
    let dict = obj.cast::<PyDict>()?;
    let mut map = HashMap::new();
    for (key, val) in dict.iter() {
        let k: String = key.extract()?;
        let v = py_to_json(&val)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("option {k:?}: {e}")))?;
        map.insert(k, v);
    }
    Ok(map)
}

/// Convert a serde_json::Value to a Python object.

// --- Done pyclass ---

#[pyclass(name = "Done", module = "_gremlins_core.stages", skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct Done(pub(crate) RustDone);

#[pymethods]
impl Done {
    #[new]
    fn new() -> Self {
        Done(RustDone)
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other.is_instance_of::<Self>()
    }

    fn __hash__(&self) -> isize {
        0
    }
}

// --- Bail exception ---

create_exception!(_gremlins_core.stages, Bail, PyException);

/// The textual payload of a `Bail`: `self.args[0]` or `""` when empty.
fn bail_text(args: &Bound<'_, PyTuple>) -> PyResult<String> {
    let slf = args.get_item(0)?;
    let exc_args = slf.getattr("args")?;
    if exc_args.len()? == 0 {
        return Ok(String::new());
    }
    Ok(exc_args.get_item(0)?.str()?.to_string())
}

/// Give `Bail` a `reason` property surfacing its message.
///
/// `__str__` needs no override: `BaseException.__str__` already returns
/// `args[0]` for a single-argument exception (and `""` for none), which is
/// exactly the behaviour the old Python shim provided.
fn patch_bail(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    let bail = m.getattr("Bail")?;
    let reason_getter =
        PyCFunction::new_closure(py, Some(c"reason"), None, |args, _kwargs| bail_text(args))?;
    let property = py.import("builtins")?.getattr("property")?;
    bail.setattr("reason", property.call1((reason_getter,))?)?;
    Ok(())
}

// --- Exec pyclass ---

#[pyclass(name = "Exec", module = "_gremlins_core.stages", skip_from_py_object)]
pub(crate) struct PyExec {
    inner: rust_exec::Exec,
    raw_dict: Option<Py<PyAny>>,
    gremlin: Option<Py<PyAny>>,
    client: Option<Py<PyAny>>,
    client_explicit: bool,
    skip_if_exists: String,
    #[pyo3(get, set)]
    _shell_fn: Option<Py<PyAny>>,
}

#[pymethods]
impl PyExec {
    #[new]
    #[pyo3(signature = (name, options, interpolation_map = None, bind_map = None))]
    fn new(
        name: String,
        options: &Bound<'_, PyAny>,
        interpolation_map: Option<HashMap<String, String>>,
        bind_map: Option<HashMap<String, String>>,
    ) -> PyResult<Self> {
        let options = extract_json_value_dict(options)?;
        Ok(PyExec {
            inner: rust_exec::Exec {
                name,
                options,
                interpolation_map: interpolation_map.unwrap_or_default(),
                bind_map: bind_map.unwrap_or_default(),
            },
            raw_dict: None,
            gremlin: None,
            client: None,
            client_explicit: false,
            skip_if_exists: String::new(),
            _shell_fn: None,
        })
    }

    #[classmethod]
    #[pyo3(signature = (d, _depth = 0))]
    fn with_dict(_cls: &Bound<'_, PyType>, d: &Bound<'_, PyDict>, _depth: usize) -> PyResult<Self> {
        let name: String = d
            .get_item("name")
            .ok()
            .flatten()
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default();

        if d.contains("in")? || d.contains("out")? {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "stage {name:?}: 'in'/'out' keys are no longer supported; \
                 use 'interpolation'/'bind' with URI values"
            )));
        }

        let raw_interpolation: HashMap<String, String> = match d.get_item("interpolation")? {
            Some(v) => v.extract().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: 'interpolation' must be a mapping"
                ))
            })?,
            None => HashMap::new(),
        };

        let raw_bind: HashMap<String, String> = match d.get_item("bind")? {
            Some(v) => v.extract().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: 'bind' must be a mapping"
                ))
            })?,
            None => HashMap::new(),
        };

        let options: HashMap<String, serde_json::Value> = match d.get_item("options")? {
            Some(v) => extract_json_value_dict(&v).map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: 'options' must be a mapping of string keys to JSON-serializable values"
                ))
            })?,
            None => HashMap::new(),
        };

        for k in options.keys() {
            if FRAMEWORK_KEYS.contains(k.as_str()) {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: option key {k:?} collides with framework substitution variable"
                )));
            }
        }

        let raw_client: Option<String> = d
            .get_item("client")?
            .and_then(|v| v.extract::<String>().ok());
        let (client, client_explicit) = if let Some(raw) = raw_client {
            let parsed: Py<PyAny> = Py::new(_cls.py(), Client::parse(&raw)?)?.into_any();
            (Some(parsed), true)
        } else {
            (None, false)
        };

        Ok(PyExec {
            inner: rust_exec::Exec {
                name,
                options,
                interpolation_map: raw_interpolation,
                bind_map: raw_bind,
            },
            raw_dict: None,
            gremlin: None,
            client,
            client_explicit,
            skip_if_exists: String::new(),
            _shell_fn: None,
        })
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn r#type(&self) -> &'static str {
        "exec"
    }

    #[getter]
    fn bind_map(&self) -> HashMap<String, String> {
        self.inner.bind_map.clone()
    }

    #[getter]
    fn interpolation_map(&self) -> HashMap<String, String> {
        self.inner.interpolation_map.clone()
    }

    #[getter]
    fn body(&self) -> Vec<Py<PyAny>> {
        Vec::new()
    }

    #[getter]
    fn raw_dict(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.raw_dict.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_raw_dict(&mut self, value: &Bound<'_, PyAny>) {
        self.raw_dict = Some(value.clone().unbind());
    }

    #[getter]
    fn gremlin(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.gremlin.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_gremlin(&mut self, value: &Bound<'_, PyAny>) {
        self.gremlin = Some(value.clone().unbind());
    }

    #[getter]
    fn path(&self) -> String {
        String::new()
    }

    #[setter]
    fn set_path(&mut self, _value: &str) {
        // path is set by composite stages; we don't need to store it
    }

    #[getter]
    fn client(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.client.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_client(&mut self, value: Option<&Bound<'_, PyAny>>) {
        self.client = value.map(|v| v.clone().unbind());
    }

    #[getter]
    fn client_explicit(&self) -> bool {
        self.client_explicit
    }

    #[setter]
    fn set_client_explicit(&mut self, value: bool) {
        self.client_explicit = value;
    }

    #[getter]
    fn skip_if_exists(&self) -> &str {
        &self.skip_if_exists
    }

    #[setter]
    fn set_skip_if_exists(&mut self, value: String) {
        self.skip_if_exists = value;
    }

    fn run(slf: PyRef<'_, Self>, gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let py = gremlin.py();
        let stage: Py<PyExec> = slf.into();
        let helper = wrap_pyfunction!(exec_run_async, py)?;
        Ok(helper.call1((stage, gremlin))?.unbind())
    }

    fn _run_impl<'py>(
        slf: PyRef<'_, Self>,
        py: Python<'py>,
        gremlin: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let exec = slf.inner.clone();

        let state_obj = gremlin.getattr("state")?;
        if state_obj.is_none() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "exec stage requires gremlin.state to be initialized",
            ));
        }

        // Extract all data from Python objects while holding GIL.
        let artifacts: Py<ArtifactRegistry> = state_obj.getattr("artifacts")?.extract()?;
        let loop_iter_str: String = state_obj.getattr("loop_iter")?.extract()?;
        let cwd: PathBuf = state_obj.getattr("cwd")?.extract()?;
        let artifact_dir: PathBuf = state_obj.getattr("artifact_dir")?.extract()?;
        let state_dir: PathBuf = gremlin.getattr("state_dir")?.extract()?;
        let fw: HashMap<String, String> = state_obj
            .call_method1("framework_subs", (&slf,))?
            .extract()?;

        // Phase 1: prepare (needs &mut ArtifactRegistry).
        // Lock the registry, call prepare_exec, then drop the lock before
        // entering async so the guard (which is !Send) does not cross an
        // await point.
        let mut prepared = {
            let arts_ref = artifacts.bind(py);
            let arts_inner: PyRef<'_, ArtifactRegistry> = arts_ref.extract()?;
            let mut inner = arts_inner.inner.lock().unwrap();
            match rust_exec::prepare_exec(&exec, &mut inner, &loop_iter_str, &fw) {
                Ok(p) => p,
                Err(rust_exec::ExecError::Resolve {
                    source: gremlins::artifacts::resolve::ResolveError::MissingArtifact(key),
                    ..
                }) => {
                    return Err(MissingArtifact::new_err(format!(
                        "artifact not bound: {key:?}"
                    )));
                }
                Err(e) => {
                    return Err(pyo3::exceptions::PyValueError::new_err(e.to_string()));
                }
            }
        };
        prepared.cwd = cwd;
        prepared.artifact_dir = artifact_dir;
        prepared.state_dir = state_dir;

        if prepared.cmds.is_empty() {
            // Empty commands: everything is synchronous — lock, commit, return.
            let arts_ref = artifacts.bind(py);
            let arts_inner: PyRef<'_, ArtifactRegistry> = arts_ref.extract()?;
            let mut inner = arts_inner.inner.lock().unwrap();
            rust_exec::commit_exec(&prepared, &mut inner)
                .map_err(|e| Bail::new_err(e.to_string()))?;

            return pyo3_async_runtimes::tokio::future_into_py::<_, Py<PyAny>>(py, async move {
                Python::attach(|py| Ok(Py::new(py, Done(RustDone))?.into_any()))
            });
        }

        // Resolve shell hook: per-instance _shell_fn (test seam).
        let shell_fn = slf._shell_fn.as_ref().map(|f| f.clone_ref(py));

        // Non-empty commands: production path calls Rust directly (no nested
        // future_into_py, no Python round-trip). Test path uses a Python hook
        // set via _shell_fn.
        pyo3_async_runtimes::tokio::future_into_py::<_, Py<PyAny>>(py, async move {
            if let Some(shell_fn) = shell_fn {
                // Test path: call the Python hook (async function returning
                // a CompletedProcess-like object).
                let joined = prepared.cmds.join(" && ");
                let mut env_map: HashMap<String, String> = std::env::vars().collect();
                env_map.insert(
                    "GREMLINS_ARTIFACT_DIR".to_string(),
                    prepared.artifact_dir.to_string_lossy().to_string(),
                );

                let proc_fut = Python::attach(|py| -> PyResult<PyAwaitable> {
                    let cwd_str = prepared.cwd.to_string_lossy().to_string();
                    let py_env = PyDict::new(py);
                    for (k, v) in &env_map {
                        py_env.set_item(k.as_str(), v.as_str())?;
                    }
                    let kwargs = PyDict::new(py);
                    kwargs.set_item("cwd", cwd_str)?;
                    kwargs.set_item("env", py_env)?;
                    if let Some(t) = prepared.timeout {
                        kwargs.set_item("timeout", t)?;
                    }
                    let coro = shell_fn.bind(py).call((joined.as_str(),), Some(&kwargs))?;
                    let fut = pyo3_async_runtimes::tokio::into_future(coro)?;
                    Ok(Box::pin(fut))
                })?;

                let proc_result: Py<PyAny> = proc_fut
                    .await
                    .map_err(|e| Bail::new_err(format!("exec stage python error: {e}")))?;

                let (rc, stdout, stderr) =
                    Python::attach(|py| -> PyResult<(i32, String, String)> {
                        let obj = proc_result.bind(py);
                        let rc: i32 = obj.getattr("returncode")?.extract()?;
                        let stdout: String = obj.getattr("stdout")?.extract()?;
                        let stderr: String = obj.getattr("stderr")?.extract()?;
                        Ok((rc, stdout, stderr))
                    })?;

                let raw_result = gremlins::core::proc::ProcResult {
                    returncode: rc,
                    stdout: stdout.into_bytes(),
                    stderr: stderr.into_bytes(),
                };
                rust_exec::process_shell_result(&prepared, raw_result)
                    .map_err(|e| Bail::new_err(e.to_string()))?
            } else {
                // Production path: call Rust directly.
                // No Python round-trip, no nested future_into_py.
                rust_exec::run_shell(&prepared)
                    .await
                    .map_err(|e| Bail::new_err(e.to_string()))?
            };

            Python::attach(|py| {
                let arts_ref = artifacts.bind(py);
                let arts_inner: PyRef<'_, ArtifactRegistry> = arts_ref.extract()?;
                let mut inner = arts_inner.inner.lock().unwrap();
                rust_exec::commit_exec(&prepared, &mut inner)
                    .map_err(|e| Bail::new_err(e.to_string()))?;

                let done_obj: Py<PyAny> = Py::new(py, Done(RustDone))?.into();
                Ok(done_obj.into_any())
            })
        })
    }
}

// --- Agent pyclass ---

#[pyclass(name = "Agent", module = "_gremlins_core.stages", skip_from_py_object)]
struct PyAgent {
    inner: rust_agent::Agent,
    stage_type: String,
    raw_dict: Option<Py<PyAny>>,
    gremlin: Option<Py<PyAny>>,
    client: Option<Py<PyAny>>,
    client_explicit: bool,
    skip_if_exists: String,
}

#[pymethods]
impl PyAgent {
    #[new]
    #[pyo3(signature = (name, prompts, options, interpolation_map = None, bind_map = None))]
    fn new(
        name: String,
        prompts: Vec<String>,
        options: &Bound<'_, PyAny>,
        interpolation_map: Option<HashMap<String, String>>,
        bind_map: Option<HashMap<String, String>>,
    ) -> PyResult<Self> {
        let options = extract_json_value_dict(options)?;
        Ok(PyAgent {
            inner: rust_agent::Agent {
                name,
                prompts,
                options,
                interpolation_map: interpolation_map.unwrap_or_default(),
                bind_map: bind_map.unwrap_or_default(),
            },
            stage_type: "agent".to_string(),
            raw_dict: None,
            gremlin: None,
            client: None,
            client_explicit: false,
            skip_if_exists: String::new(),
        })
    }

    #[classmethod]
    #[pyo3(signature = (d, _depth = 0))]
    fn with_dict(_cls: &Bound<'_, PyType>, d: &Bound<'_, PyDict>, _depth: usize) -> PyResult<Self> {
        let name: String = d
            .get_item("name")
            .ok()
            .flatten()
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default();

        if d.contains("in")? || d.contains("out")? {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "stage {name:?}: 'in'/'out' keys are no longer supported; \
                 use 'interpolation'/'bind' with URI values"
            )));
        }

        let raw_interpolation: HashMap<String, String> = match d.get_item("interpolation")? {
            Some(v) => v.extract().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: 'interpolation' must be a mapping"
                ))
            })?,
            None => HashMap::new(),
        };

        let raw_bind: HashMap<String, String> = match d.get_item("bind")? {
            Some(v) => v.extract().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: 'bind' must be a mapping"
                ))
            })?,
            None => HashMap::new(),
        };

        let options: HashMap<String, serde_json::Value> = match d.get_item("options")? {
            Some(v) => extract_json_value_dict(&v).map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: 'options' must be a mapping of string keys to JSON-serializable values"
                ))
            })?,
            None => HashMap::new(),
        };

        for k in options.keys() {
            if FRAMEWORK_KEYS.contains(k.as_str()) && k != "model" {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: option key {k:?} collides with framework substitution variable"
                )));
            }
        }

        let prompts: Vec<String> = match d.get_item("prompt")? {
            Some(v) => v.extract::<Vec<String>>().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: 'prompt' must be a list of strings"
                ))
            })?,
            None => Vec::new(),
        };

        let raw_client: Option<String> = d
            .get_item("client")?
            .and_then(|v| v.extract::<String>().ok());
        let (client, client_explicit) = if let Some(raw) = raw_client {
            let parsed: Py<PyAny> = Py::new(_cls.py(), Client::parse(&raw)?)?.into_any();
            (Some(parsed), true)
        } else {
            (None, false)
        };

        Ok(PyAgent {
            inner: rust_agent::Agent {
                name,
                prompts,
                options,
                interpolation_map: raw_interpolation,
                bind_map: raw_bind,
            },
            stage_type: "agent".to_string(),
            raw_dict: None,
            gremlin: None,
            client,
            client_explicit,
            skip_if_exists: String::new(),
        })
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn r#type(&self) -> &str {
        &self.stage_type
    }

    #[setter]
    fn set_type(&mut self, value: String) {
        self.stage_type = value;
    }

    #[getter]
    fn prompts(&self) -> Vec<String> {
        self.inner.prompts.clone()
    }

    #[getter]
    fn bind_map(&self) -> HashMap<String, String> {
        self.inner.bind_map.clone()
    }

    #[getter]
    fn interpolation_map(&self) -> HashMap<String, String> {
        self.inner.interpolation_map.clone()
    }

    #[getter]
    fn body(&self) -> Vec<Py<PyAny>> {
        Vec::new()
    }

    #[getter]
    fn raw_dict(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.raw_dict.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_raw_dict(&mut self, value: &Bound<'_, PyAny>) {
        self.raw_dict = Some(value.clone().unbind());
    }

    #[getter]
    fn gremlin(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.gremlin.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_gremlin(&mut self, value: &Bound<'_, PyAny>) {
        self.gremlin = Some(value.clone().unbind());
    }

    #[getter]
    fn path(&self) -> String {
        String::new()
    }

    #[setter]
    fn set_path(&mut self, _value: &str) {}

    #[getter]
    fn client(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.client.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_client(&mut self, value: Option<&Bound<'_, PyAny>>) {
        self.client = value.map(|v| v.clone().unbind());
    }

    #[getter]
    fn client_explicit(&self) -> bool {
        self.client_explicit
    }

    #[setter]
    fn set_client_explicit(&mut self, value: bool) {
        self.client_explicit = value;
    }

    #[getter]
    fn skip_if_exists(&self) -> &str {
        &self.skip_if_exists
    }

    #[setter]
    fn set_skip_if_exists(&mut self, value: String) {
        self.skip_if_exists = value;
    }

    fn run(slf: PyRef<'_, Self>, gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let py = gremlin.py();
        let stage: Py<PyAgent> = slf.into();
        let helper = wrap_pyfunction!(agent_run_async, py)?;
        Ok(helper.call1((stage, gremlin))?.unbind())
    }

    fn _run_impl<'py>(
        slf: PyRef<'_, Self>,
        py: Python<'py>,
        gremlin: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let agent = slf.inner.clone();

        let state_obj = gremlin.getattr("state")?;
        if state_obj.is_none() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "agent stage requires gremlin.state to be initialized",
            ));
        }

        // Extract all data from Python objects while holding GIL.
        let artifacts: Py<ArtifactRegistry> = state_obj.getattr("artifacts")?.extract()?;
        let loop_iter_str: String = state_obj.getattr("loop_iter")?.extract()?;
        let cwd_str: String = state_obj.getattr("cwd")?.extract()?;
        let worktree_path: Option<PathBuf> = state_obj.getattr("worktree")?.extract()?;
        let artifact_dir: PathBuf = state_obj.getattr("artifact_dir")?.extract()?;
        let client_py: Py<PyAny> = state_obj.getattr("client")?.extract()?;
        let data_py: Py<PyAny> = state_obj.getattr("data")?.extract()?;
        let fw: HashMap<String, String> = state_obj
            .call_method1("framework_subs", (&slf,))?
            .extract()?;

        let worktree_str = worktree_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Phase 1: prepare (needs &mut ArtifactRegistry).
        let mut prepared = {
            let arts_ref = artifacts.bind(py);
            let arts_inner: PyRef<'_, ArtifactRegistry> = arts_ref.extract()?;
            let mut inner = arts_inner.inner.lock().unwrap();
            match rust_agent::prepare_agent(&agent, &mut inner, &loop_iter_str, &fw) {
                Ok(p) => p,
                Err(rust_agent::AgentError::Resolve {
                    source: gremlins::artifacts::resolve::ResolveError::MissingArtifact(key),
                    ..
                }) => {
                    return Err(MissingArtifact::new_err(format!(
                        "artifact not bound: {key:?}"
                    )));
                }
                Err(e) => {
                    return Err(Bail::new_err(e.to_string()));
                }
            }
        };
        prepared.cwd = cwd_str.clone();
        prepared.worktree = worktree_str.clone();
        prepared.artifact_dir = artifact_dir.to_string_lossy().to_string();

        // Harness system prompt and pipeline user content are passed
        // separately; the client routes the system prompt through the
        // provider's native system role.
        let system_prompt = prepared.system_prompt();
        let user_prompt = prepared.user_prompt();

        let raw_path = artifact_dir.join(format!("stream-{}.jsonl", prepared.name));

        let model = prepared.model.clone();

        let expected_artifact_paths: Vec<PathBuf> = prepared
            .expected_artifact_paths
            .iter()
            .map(PathBuf::from)
            .collect();

        pyo3_async_runtimes::tokio::future_into_py::<_, Py<PyAny>>(py, async move {
            // Call the Python client.run() method.
            let client_fut = Python::attach(|py| -> PyResult<PyAwaitable> {
                let client_obj = client_py.bind(py);

                // Build kwargs for client.run()
                let kwargs = PyDict::new(py);
                kwargs.set_item("label", prepared.name.as_str())?;

                // If model is specified, use it; otherwise client default
                let resolved_model = model.clone().or_else(|| {
                    client_obj
                        .getattr("model")
                        .ok()
                        .and_then(|m| m.extract::<String>().ok())
                });
                if let Some(m) = &resolved_model {
                    kwargs.set_item("model", m.as_str())?;
                }

                kwargs.set_item("raw_path", raw_path.as_path())?;
                if let Some(ref wt) = worktree_str {
                    kwargs.set_item("cwd", wt.as_str())?;
                } else {
                    kwargs.set_item("cwd", py.None())?;
                }
                kwargs.set_item("artifact_dir", prepared.artifact_dir.as_str())?;

                // Pass through expected_artifact_paths
                kwargs.set_item(
                    "expected_artifact_paths",
                    expected_artifact_paths.as_slice(),
                )?;
                kwargs.set_item("system_prompt", system_prompt.as_str())?;

                // Pass through remaining options (except "model") as kwargs to client.run(),
                // reserving "system_prompt" so a user option can never clobber the harness prompt.
                for (k, v) in &agent.options {
                    if k == "model" || k == "system_prompt" {
                        continue;
                    }
                    let py_val = json_value_to_py(py, v)?;
                    kwargs.set_item(k.as_str(), py_val)?;
                }

                let coro = client_obj.call_method("run", (&user_prompt,), Some(&kwargs))?;
                let fut = pyo3_async_runtimes::tokio::into_future(coro)?;
                Ok(Box::pin(fut))
            })?;

            let completed_result: Py<PyAny> = client_fut
                .await
                .map_err(|e| Bail::new_err(format!("agent stage client error: {e}")))?;

            Python::attach(|py| -> PyResult<Py<PyAny>> {
                // Record token usage
                let token_usage = completed_result.bind(py).getattr("token_usage")?;
                if !token_usage.is_none() {
                    let prompt_tokens: u64 = token_usage.getattr("prompt_tokens")?.extract()?;
                    let completion_tokens: u64 =
                        token_usage.getattr("completion_tokens")?.extract()?;
                    let cached_input_tokens: u64 =
                        token_usage.getattr("cached_input_tokens")?.extract()?;
                    let cache_creation_input_tokens: u64 = token_usage
                        .getattr("cache_creation_input_tokens")?
                        .extract()?;
                    let reasoning_tokens: u64 =
                        token_usage.getattr("reasoning_tokens")?.extract()?;
                    let turns: usize = token_usage.getattr("turns")?.extract()?;

                    let delta = HashMap::from([
                        ("prompt_tokens".to_string(), prompt_tokens as i64),
                        ("completion_tokens".to_string(), completion_tokens as i64),
                        (
                            "cached_input_tokens".to_string(),
                            cached_input_tokens as i64,
                        ),
                        (
                            "cache_creation_input_tokens".to_string(),
                            cache_creation_input_tokens as i64,
                        ),
                        ("reasoning_tokens".to_string(), reasoning_tokens as i64),
                        ("turns".to_string(), turns as i64),
                    ]);

                    let data_obj = data_py.bind(py);
                    data_obj.call_method1("accumulate_token_usage", (delta,))?;
                }

                // Check bail
                let exit_code: i32 = completed_result.bind(py).getattr("exit_code")?.extract()?;
                let text_result: Option<String> = completed_result
                    .bind(py)
                    .getattr("text_result")?
                    .extract()?;

                let cr = gremlins::clients::protocol::CompletedRun {
                    exit_code,
                    text_result,
                    events: None,
                    cost_usd: None,
                    token_usage: None,
                };

                if let Err(rust_agent::AgentError::Bail { reason, .. }) =
                    rust_agent::check_bail(&cr)
                {
                    return Err(Bail::new_err(reason));
                }

                let arts_inner: PyRef<'_, ArtifactRegistry> = artifacts.bind(py).extract()?;
                let mut inner = arts_inner.inner.lock().unwrap();
                rust_agent::commit_agent(&prepared, &mut inner)
                    .map_err(|e| Bail::new_err(e.to_string()))?;

                let done_obj: Py<PyAny> = Py::new(py, Done(RustDone))?.into();
                Ok(done_obj.into_any())
            })
        })
    }
}

// --- StageAttrs pyclass ---

/// Base attributes for composite stages (Loop, Sequence, Parallel) and
/// duck-typed test stages. Subclassable from Python.
#[pyclass(
    name = "StageAttrs",
    module = "_gremlins_core.stages",
    subclass,
    skip_from_py_object
)]
struct PyStageAttrs {
    inner: RustStageAttrs,
    body: Py<PyList>,
    options: Py<PyDict>,
    bind_map: Py<PyDict>,
    client: Option<Py<PyAny>>,
    raw_dict: Option<Py<PyAny>>,
    gremlin: Option<Py<PyAny>>,
}

#[pymethods]
impl PyStageAttrs {
    // Tolerant of extra args: Python subclasses pass their own ctor args
    // through to tp_new (which receives *all* constructor arguments).
    #[new]
    #[pyo3(signature = (name, *args, **kwargs))]
    fn new(
        py: Python<'_>,
        name: String,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> Self {
        let _ = (args, kwargs);
        PyStageAttrs {
            inner: RustStageAttrs::new(name),
            body: PyList::empty(py).unbind(),
            options: PyDict::new(py).unbind(),
            bind_map: PyDict::new(py).unbind(),
            client: None,
            raw_dict: None,
            gremlin: None,
        }
    }

    // Python subclasses call super().__init__(name); tolerate their extra args.
    #[pyo3(signature = (name, *args, **kwargs))]
    fn __init__(
        &mut self,
        name: String,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) {
        let _ = (args, kwargs);
        self.inner.name = name;
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[setter]
    fn set_name(&mut self, value: String) {
        self.inner.name = value;
    }

    #[getter]
    fn r#type(&self) -> &str {
        &self.inner.stage_type
    }

    #[setter]
    fn set_type(&mut self, value: String) {
        self.inner.stage_type = value;
    }

    #[getter]
    fn path(&self) -> &str {
        &self.inner.path
    }

    #[setter]
    fn set_path(&mut self, py: Python<'_>, value: String) {
        self.inner.path = value.clone();
        for child in self.body.bind(py).iter() {
            let Ok(child_name) = child.getattr("name").and_then(|n| n.extract::<String>()) else {
                continue;
            };
            let _ = child.setattr("path", format!("{value}/{child_name}"));
        }
    }

    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> Bound<'py, PyList> {
        self.body.bind(py).clone()
    }

    #[setter]
    fn set_body(&mut self, value: &Bound<'_, PyList>) {
        self.body = value.clone().unbind();
    }

    #[getter]
    fn client(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.client.as_ref().map(|c| c.clone_ref(py))
    }

    #[setter]
    fn set_client(&mut self, value: Option<&Bound<'_, PyAny>>) {
        self.client = value.map(|v| v.clone().unbind());
    }

    #[getter]
    fn client_explicit(&self) -> bool {
        self.inner.client_explicit
    }

    #[setter]
    fn set_client_explicit(&mut self, value: bool) {
        self.inner.client_explicit = value;
    }

    #[getter]
    fn skip_if_exists(&self) -> &str {
        &self.inner.skip_if_exists
    }

    #[setter]
    fn set_skip_if_exists(&mut self, value: String) {
        self.inner.skip_if_exists = value;
    }

    #[getter]
    fn raw_dict(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.raw_dict.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_raw_dict(&mut self, value: &Bound<'_, PyAny>) {
        self.raw_dict = Some(value.clone().unbind());
    }

    #[getter]
    fn gremlin(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.gremlin.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_gremlin(&mut self, value: &Bound<'_, PyAny>) {
        self.gremlin = Some(value.clone().unbind());
    }

    #[getter]
    fn options<'py>(&self, py: Python<'py>) -> Bound<'py, PyDict> {
        self.options.bind(py).clone()
    }

    #[setter]
    fn set_options(&mut self, value: &Bound<'_, PyDict>) {
        self.options = value.clone().unbind();
    }

    #[getter]
    fn bind_map<'py>(&self, py: Python<'py>) -> Bound<'py, PyDict> {
        self.bind_map.bind(py).clone()
    }

    #[setter]
    fn set_bind_map(&mut self, value: &Bound<'_, PyDict>) {
        self.bind_map = value.clone().unbind();
    }
}

// --- Sequence pyclass ---

#[pyclass(name = "Sequence", module = "_gremlins_core.stages", extends = PyStageAttrs, subclass, skip_from_py_object)]
struct PySequence;

impl PySequence {
    /// Build from Rust StageAttrs + pre-parsed body, propagating the parent
    /// path onto each child.
    fn from_rust(
        py: Python<'_>,
        attrs: &RustStageAttrs,
        body: &Bound<'_, PyList>,
    ) -> PyResult<PyStageAttrs> {
        for child in body.iter() {
            let Ok(name) = child.getattr("name").and_then(|n| n.extract::<String>()) else {
                continue;
            };
            child.setattr("path", format!("{}/{name}", attrs.name))?;
        }
        Ok(PyStageAttrs {
            inner: attrs.clone(),
            body: body.clone().unbind(),
            options: PyDict::new(py).unbind(),
            bind_map: PyDict::new(py).unbind(),
            client: None,
            raw_dict: None,
            gremlin: None,
        })
    }
}

#[pymethods]
impl PySequence {
    #[new]
    #[pyo3(signature = (name, *, body = None))]
    fn new(
        py: Python<'_>,
        name: String,
        body: Option<&Bound<'_, PyList>>,
    ) -> PyResult<PyClassInitializer<Self>> {
        let body = body.cloned().unwrap_or_else(|| PyList::empty(py));
        let mut attrs = RustStageAttrs::new(name);
        attrs.stage_type = "sequence".to_string();
        let base = Self::from_rust(py, &attrs, &body)?;
        Ok(PyClassInitializer::from(base).add_subclass(PySequence))
    }

    #[classmethod]
    #[pyo3(signature = (d, depth = 0))]
    fn with_dict(
        cls: &Bound<'_, PyType>,
        d: &Bound<'_, PyDict>,
        depth: usize,
    ) -> PyResult<Py<PyAny>> {
        let map = extract_json_value_dict(d)?;
        let seq = gremlins::stages::sequence::Sequence::with_dict(&map)
            .map_err(pyo3::exceptions::PyValueError::new_err)?;

        let py = d.py();
        let raw_body = PyList::empty(py);
        for child in &seq.body {
            raw_body.append(json_value_to_py(py, child)?)?;
        }
        let parsed = loader::parse_stages(py, &raw_body, depth)?;

        // Construct through `cls` so subclasses get their own type.
        let kwargs = PyDict::new(py);
        kwargs.set_item("body", PyList::new(py, parsed)?)?;
        let obj = cls.call((seq.attrs.name.as_str(),), Some(&kwargs))?;

        let client = match &seq.client {
            Some(spec) => Some(Py::new(py, Client::parse(&spec.0)?)?.into_any()),
            None => None,
        };
        obj.setattr("client", client)?;
        obj.setattr("client_explicit", seq.attrs.client_explicit)?;
        Ok(obj.unbind())
    }

    fn run(slf: PyRef<'_, Self>, gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let py = gremlin.py();
        let stage: Py<PySequence> = slf.into();
        let helper = wrap_pyfunction!(sequence_run_async, py)?;
        Ok(helper.call1((stage, gremlin))?.unbind())
    }
}

// --- Loop pyclass ---

fn check_max_iterations(name: &str, value: u32) -> PyResult<()> {
    if value < 1 {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "Loop {name:?}: max_iterations must be >= 1, got {value}"
        )));
    }
    Ok(())
}

#[pyclass(name = "Loop", module = "_gremlins_core.stages", extends = PyStageAttrs, subclass, skip_from_py_object)]
struct PyLoop {
    max_iterations: u32,
    stop_when_exists: Option<String>,
    interval: Option<f64>,
    /// Test seam: pre-built body runner callables, bypassing `body`.
    body_runners: Option<Py<PyAny>>,
}

#[pymethods]
impl PyLoop {
    #[new]
    #[pyo3(signature = (name, *, body = None, body_runners = None, max_iterations = 3, stop_when_exists = None, interval = None))]
    fn new(
        py: Python<'_>,
        name: String,
        body: Option<&Bound<'_, PyList>>,
        body_runners: Option<&Bound<'_, PyAny>>,
        max_iterations: u32,
        stop_when_exists: Option<String>,
        interval: Option<f64>,
    ) -> PyResult<PyClassInitializer<Self>> {
        check_max_iterations(&name, max_iterations)?;
        let body = body.cloned().unwrap_or_else(|| PyList::empty(py));
        let mut attrs = RustStageAttrs::new(name.clone());
        attrs.stage_type = "loop".to_string();
        for child in body.iter() {
            let Ok(child_name) = child.getattr("name").and_then(|n| n.extract::<String>()) else {
                continue;
            };
            child.setattr("path", format!("{name}/{child_name}"))?;
        }
        let base = PyStageAttrs {
            inner: attrs,
            body: body.unbind(),
            options: PyDict::new(py).unbind(),
            bind_map: PyDict::new(py).unbind(),
            client: None,
            raw_dict: None,
            gremlin: None,
        };
        Ok(PyClassInitializer::from(base).add_subclass(PyLoop {
            max_iterations,
            stop_when_exists,
            interval,
            body_runners: body_runners.map(|b| b.clone().unbind()),
        }))
    }

    #[classmethod]
    #[pyo3(signature = (d, depth = 0))]
    fn with_dict(
        cls: &Bound<'_, PyType>,
        d: &Bound<'_, PyDict>,
        depth: usize,
    ) -> PyResult<Py<PyAny>> {
        let map = extract_json_value_dict(d)?;
        let lp = gremlins::stages::r#loop::Loop::with_dict(&map)
            .map_err(pyo3::exceptions::PyValueError::new_err)?;

        let py = d.py();
        let raw_body = PyList::empty(py);
        for child in &lp.body {
            raw_body.append(json_value_to_py(py, child)?)?;
        }
        let parsed = loader::parse_stages(py, &raw_body, depth)?;

        let kwargs = PyDict::new(py);
        kwargs.set_item("body", PyList::new(py, parsed)?)?;
        kwargs.set_item("max_iterations", lp.max_iterations)?;
        kwargs.set_item("stop_when_exists", lp.stop_when_exists)?;
        kwargs.set_item("interval", lp.interval)?;
        let obj = cls.call((lp.attrs.name.as_str(),), Some(&kwargs))?;

        let client = match &lp.client {
            Some(spec) => Some(Py::new(py, Client::parse(&spec.0)?)?.into_any()),
            None => None,
        };
        obj.setattr("client", client)?;
        obj.setattr("client_explicit", lp.attrs.client_explicit)?;
        Ok(obj.unbind())
    }

    fn run(slf: PyRef<'_, Self>, gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let py = gremlin.py();
        let stage: Py<PyLoop> = slf.into();
        let helper = wrap_pyfunction!(loop_run_async, py)?;
        Ok(helper.call1((stage, gremlin))?.unbind())
    }

    #[getter]
    fn max_iterations(&self) -> u32 {
        self.max_iterations
    }

    /// Mirror the constructor's invariant; assigning 0 would otherwise skip
    /// the loop and hit run()'s fall-through RuntimeError.
    #[setter]
    fn set_max_iterations(mut slf: PyRefMut<'_, Self>, value: u32) -> PyResult<()> {
        let name = slf.as_super().inner.name.clone();
        check_max_iterations(&name, value)?;
        slf.max_iterations = value;
        Ok(())
    }

    #[getter]
    fn stop_when_exists(&self) -> Option<&str> {
        self.stop_when_exists.as_deref()
    }

    #[setter]
    fn set_stop_when_exists(&mut self, value: Option<String>) {
        self.stop_when_exists = value;
    }

    #[getter]
    fn interval(&self) -> Option<f64> {
        self.interval
    }

    #[setter]
    fn set_interval(&mut self, value: Option<f64>) {
        self.interval = value;
    }

    #[getter]
    fn body_runners(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.body_runners.as_ref().map(|p| p.clone_ref(py))
    }

    #[setter]
    fn set_body_runners(&mut self, value: Option<&Bound<'_, PyAny>>) {
        self.body_runners = value.map(|v| v.clone().unbind());
    }
}

// --- ParallelStage pyclass ---

/// Fan-out/fan-in execution of a `parallel:` block.
///
/// The heavy lifting lives in [`parallel_run_async`]; this class owns the
/// parsed configuration and the Python-visible surface (`with_dict`, `run`,
/// and the `max_concurrent` / `cancel_on_bail` / `bail_policy` properties).
#[pyclass(name = "ParallelStage", module = "_gremlins_core.stages", extends = PyStageAttrs, subclass, skip_from_py_object)]
struct PyParallelStage {
    max_concurrent: Option<u32>,
    cancel_on_bail: bool,
    bail_policy: String,
}

impl PyParallelStage {
    /// Build the `StageAttrs` base, propagating the group path onto each child.
    fn base_attrs(py: Python<'_>, name: &str, body: &Bound<'_, PyList>) -> PyResult<PyStageAttrs> {
        let mut attrs = RustStageAttrs::new(name.to_string());
        attrs.stage_type = "parallel".to_string();
        for child in body.iter() {
            let Ok(child_name) = child.getattr("name").and_then(|n| n.extract::<String>()) else {
                continue;
            };
            child.setattr("path", format!("{name}/{child_name}"))?;
        }
        Ok(PyStageAttrs {
            inner: attrs,
            body: body.clone().unbind(),
            options: PyDict::new(py).unbind(),
            bind_map: PyDict::new(py).unbind(),
            client: None,
            raw_dict: None,
            gremlin: None,
        })
    }
}

#[pymethods]
impl PyParallelStage {
    #[new]
    #[pyo3(signature = (name, body = None, *, max_concurrent = None, cancel_on_bail = false, bail_policy = "any".to_string()))]
    fn new(
        py: Python<'_>,
        name: String,
        body: Option<&Bound<'_, PyList>>,
        max_concurrent: Option<u32>,
        cancel_on_bail: bool,
        bail_policy: String,
    ) -> PyResult<PyClassInitializer<Self>> {
        if max_concurrent == Some(0) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "parallel group {name:?}: 'max_concurrent' must be a positive integer"
            )));
        }
        if bail_policy != "any" && bail_policy != "all" {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "parallel group {name:?}: 'bail_policy' must be 'any' or 'all'"
            )));
        }
        let body = body.cloned().unwrap_or_else(|| PyList::empty(py));
        let base = Self::base_attrs(py, &name, &body)?;
        Ok(
            PyClassInitializer::from(base).add_subclass(PyParallelStage {
                max_concurrent,
                cancel_on_bail,
                bail_policy,
            }),
        )
    }

    #[classmethod]
    #[pyo3(signature = (d, depth = 0))]
    fn with_dict(
        cls: &Bound<'_, PyType>,
        d: &Bound<'_, PyDict>,
        depth: usize,
    ) -> PyResult<Py<PyAny>> {
        let map = extract_json_value_dict(d)?;
        let group = gremlins::stages::parallel::ParallelGroup::with_dict(&map, depth)
            .map_err(pyo3::exceptions::PyValueError::new_err)?;

        let py = d.py();
        let raw_body = PyList::empty(py);
        for child in &group.body {
            raw_body.append(json_value_to_py(py, child)?)?;
        }
        let parsed = loader::parse_stages(py, &raw_body, depth + 1)?;

        // Child names are only known once parse_stages has named them.
        let names: Vec<String> = parsed
            .iter()
            .map(|c| c.bind(py).getattr("name")?.extract::<String>())
            .collect::<PyResult<_>>()?;
        gremlins::stages::parallel::validate_child_names(&group.attrs.name, &names)
            .map_err(pyo3::exceptions::PyValueError::new_err)?;

        // Construct through `cls` so subclasses get their own type.
        let kwargs = PyDict::new(py);
        kwargs.set_item("body", PyList::new(py, parsed)?)?;
        kwargs.set_item("max_concurrent", group.max_concurrent)?;
        kwargs.set_item("cancel_on_bail", group.cancel_on_bail)?;
        kwargs.set_item("bail_policy", group.bail_policy.as_str())?;
        let obj = cls.call((group.attrs.name.as_str(),), Some(&kwargs))?;

        let client = match &group.client {
            Some(spec) => Some(Py::new(py, Client::parse(&spec.0)?)?.into_any()),
            None => None,
        };
        obj.setattr("client", client)?;
        obj.setattr("client_explicit", group.attrs.client_explicit)?;
        Ok(obj.unbind())
    }

    fn run(slf: PyRef<'_, Self>, gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let py = gremlin.py();
        let stage: Py<PyParallelStage> = slf.into();
        let helper = wrap_pyfunction!(parallel_run_async, py)?;
        Ok(helper.call1((stage, gremlin))?.unbind())
    }

    #[getter]
    fn max_concurrent(&self) -> Option<u32> {
        self.max_concurrent
    }

    /// Reject `0` here for the same reason the constructor does: `0` would
    /// build a `Semaphore` no child can ever acquire, hanging the group.
    #[setter]
    fn set_max_concurrent(mut slf: PyRefMut<'_, Self>, value: Option<u32>) -> PyResult<()> {
        if let Some(0) = value {
            let name = slf.as_super().inner.name.clone();
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "parallel group {name:?}: 'max_concurrent' must be a positive integer"
            )));
        }
        slf.max_concurrent = value;
        Ok(())
    }

    #[getter]
    fn cancel_on_bail(&self) -> bool {
        self.cancel_on_bail
    }

    #[setter]
    fn set_cancel_on_bail(&mut self, value: bool) {
        self.cancel_on_bail = value;
    }

    #[getter]
    fn bail_policy(&self) -> String {
        self.bail_policy.clone()
    }

    /// Mirror the constructor's invariant. `config()` maps anything other than
    /// `"all"` onto `BailPolicy::Any`, so a typo here would silently widen the
    /// policy instead of failing.
    #[setter]
    fn set_bail_policy(mut slf: PyRefMut<'_, Self>, value: String) -> PyResult<()> {
        if value != "any" && value != "all" {
            let name = slf.as_super().inner.name.clone();
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "parallel group {name:?}: 'bail_policy' must be 'any' or 'all'"
            )));
        }
        slf.bail_policy = value;
        Ok(())
    }

    /// Build the three runtime stages for this group: fan-out, parallel, fan-in.
    ///
    /// This is the test seam that mirrors the pre-port `build_runtime_stages`.
    /// Production code drives the group through [`PyParallelStage::run`].
    #[pyo3(signature = (child_runners, *, parent_state, project_root_path = None, worktree_parent = None, set_stage_fn = None, child_stages = None))]
    #[allow(clippy::too_many_arguments)]
    fn build_runtime_stages(
        slf: PyRef<'_, Self>,
        py: Python<'_>,
        child_runners: Vec<(String, Py<PyAny>, Py<PyAny>)>,
        parent_state: Py<PyAny>,
        project_root_path: Option<PathBuf>,
        worktree_parent: Option<PathBuf>,
        set_stage_fn: Option<Py<PyAny>>,
        child_stages: Option<Vec<Py<PyAny>>>,
    ) -> PyResult<Vec<(String, Py<PyAny>)>> {
        let stage: Py<PyParallelStage> = slf.into();
        let group_name: String = stage.bind(py).getattr("name")?.extract()?;
        let stage_path: String = {
            let path: Option<String> = stage.bind(py).getattr("path")?.extract()?;
            stage_key(path, group_name.clone())
        };
        let project_root = project_root_path.unwrap_or_else(gremlins::config::project_root);
        let parent_id: String = parent_state
            .bind(py)
            .getattr("data")?
            .getattr("gremlin_id")?
            .extract::<Option<String>>()?
            .unwrap_or_default();

        let stages_by_key: HashMap<String, Py<PyAny>> = child_stages
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                let name: String = s.bind(py).getattr("name")?.extract()?;
                Ok::<_, PyErr>((name, s))
            })
            .collect::<PyResult<_>>()?;

        let children: Vec<ChildSpec> = child_runners
            .into_iter()
            .map(|(key, state, runner)| ChildSpec {
                stage: stages_by_key.get(&key).map(|s| s.clone_ref(py)),
                key,
                state,
                runner,
            })
            .collect();
        let all_child_keys: Vec<String> = children.iter().map(|c| c.key.clone()).collect();

        let group = Arc::new(ParallelGroupState::new(
            group_name.clone(),
            parent_state.bind(py).getattr("data")?.unbind(),
        ));
        let parent_gremlin: Option<Py<PyAny>> = stage
            .bind(py)
            .getattr("gremlin")
            .ok()
            .filter(|g| !g.is_none())
            .map(|g| g.unbind());
        let runtime = Py::new(
            py,
            ParallelRuntime {
                stage,
                group,
                children,
                all_child_keys,
                parent_state,
                parent_gremlin,
                project_root,
                worktree_parent,
                set_stage_fn,
                stage_path,
                group_name: group_name.clone(),
                parent_id,
            },
        )?;
        let fanout = runtime.bind(py).getattr("fanout")?.unbind();
        let parallel = runtime.bind(py).getattr("parallel")?.unbind();
        let fanin = runtime.bind(py).getattr("fanin")?.unbind();
        Ok(vec![
            (format!("{group_name}-fanout"), fanout),
            (group_name.clone(), parallel),
            (format!("{group_name}-fanin"), fanin),
        ])
    }
}

/// The three runtime stages of one parallel group, as callable coroutines.
#[pyclass(name = "_ParallelRuntime", module = "_gremlins_core.stages")]
struct ParallelRuntime {
    stage: Py<PyParallelStage>,
    group: Arc<ParallelGroupState>,
    children: Vec<ChildSpec>,
    /// Every child declared by the group, including those skipped as already
    /// done. Fan-in needs the complete set for artifact gathering and cleanup.
    all_child_keys: Vec<String>,
    parent_state: Py<PyAny>,
    parent_gremlin: Option<Py<PyAny>>,
    project_root: PathBuf,
    worktree_parent: Option<PathBuf>,
    set_stage_fn: Option<Py<PyAny>>,
    stage_path: String,
    group_name: String,
    parent_id: String,
}

impl ParallelRuntime {
    fn set_stage(&self, py: Python<'_>, name: &str) {
        if let Some(f) = &self.set_stage_fn {
            let _ = f.bind(py).call1((name,));
        }
    }

    fn config(&self, py: Python<'_>) -> PyResult<(Option<u32>, bool, BailPolicy)> {
        let stage = self.stage.bind(py);
        let max_concurrent: Option<u32> = stage.getattr("max_concurrent")?.extract()?;
        let cancel_on_bail: bool = stage.getattr("cancel_on_bail")?.extract()?;
        let raw: String = stage.getattr("bail_policy")?.extract()?;
        let policy = if raw == "all" {
            BailPolicy::All
        } else {
            BailPolicy::Any
        };
        Ok((max_concurrent, cancel_on_bail, policy))
    }
}

#[pymethods]
impl ParallelRuntime {
    async fn fanout(&self) -> PyResult<Py<PyAny>> {
        let children: Vec<ChildSpec> = Python::attach(|py| {
            self.set_stage(py, &format!("{}-fanout", self.group_name));
            self.children.iter().map(|c| c.clone_ref(py)).collect()
        });
        let group = self.group.clone();
        let project_root = self.project_root.clone();
        let worktree_parent = self.worktree_parent.clone();
        let parent_id = self.parent_id.clone();
        fan_out(
            &group,
            &children,
            &project_root,
            worktree_parent.as_deref(),
            &parent_id,
            &self.parent_state,
            self.parent_gremlin.as_ref(),
        )
        .await?;
        Python::attach(|py| Ok(Py::new(py, Done(RustDone))?.into_any()))
    }

    async fn parallel(&self) -> PyResult<Py<PyAny>> {
        let (max_concurrent, cancel_on_bail, children, state) = Python::attach(|py| {
            self.set_stage(py, &self.group_name);
            let (max_concurrent, cancel_on_bail, _) = self.config(py)?;
            // Skip children already recorded as done on a prior run.
            let done: HashSet<String> = self
                .parent_state
                .bind(py)
                .call_method1("done_for", (&self.stage_path,))?
                .extract()?;
            let children: Vec<ChildSpec> = self
                .children
                .iter()
                .filter(|c| !done.contains(&c.key))
                .map(|c| c.clone_ref(py))
                .collect();
            Ok::<_, PyErr>((
                max_concurrent,
                cancel_on_bail,
                children,
                self.parent_state.clone_ref(py),
            ))
        })?;
        let group = self.group.clone();
        let stage_path = self.stage_path.clone();
        let group_name = self.group_name.clone();
        let parent_id = self.parent_id.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        dispatch_children(
            &children,
            &state,
            &group,
            &stage_path,
            &group_name,
            &parent_id,
            max_concurrent,
            cancel_on_bail,
            cancel,
        )
        .await?;
        Python::attach(|py| Ok(Py::new(py, Done(RustDone))?.into_any()))
    }

    async fn fanin(&self) -> PyResult<Py<PyAny>> {
        let (bail_policy, children, all_child_keys, state) = Python::attach(|py| {
            self.set_stage(py, &format!("{}-fanin", self.group_name));
            let (_, _, bail_policy) = self.config(py)?;
            let children: Vec<ChildSpec> = self.children.iter().map(|c| c.clone_ref(py)).collect();
            Ok::<_, PyErr>((
                bail_policy,
                children,
                self.all_child_keys.clone(),
                self.parent_state.clone_ref(py),
            ))
        })?;
        let group = self.group.clone();
        let stage_path = self.stage_path.clone();
        let group_name = self.group_name.clone();
        let parent_id = self.parent_id.clone();
        let project_root = self.project_root.clone();
        fan_in(
            &group,
            &all_child_keys,
            &children,
            &state,
            &stage_path,
            &group_name,
            &parent_id,
            bail_policy,
            &project_root,
        )
        .await?;
        Python::attach(|py| Ok(Py::new(py, Done(RustDone))?.into_any()))
    }
}

// --- Free functions ---

/// Await `stage._run_impl(gremlin)` as a loop-independent coroutine.
#[pyfunction]
#[pyo3(name = "_exec_run_async")]
async fn exec_run_async(stage: Py<PyExec>, gremlin: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let coro: Py<PyAny> = Python::attach(|py| {
        stage
            .bind(py)
            .call_method1("_run_impl", (gremlin.bind(py),))
            .map(|c| c.unbind())
    })?;
    let fut = Python::attach(|py| pyo3_async_runtimes::tokio::into_future(coro.bind(py).clone()))?;
    fut.await
}

/// Await `stage._run_impl(gremlin)` as a loop-independent coroutine.
#[pyfunction]
#[pyo3(name = "_agent_run_async")]
async fn agent_run_async(stage: Py<PyAgent>, gremlin: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let coro: Py<PyAny> = Python::attach(|py| {
        stage
            .bind(py)
            .call_method1("_run_impl", (gremlin.bind(py),))
            .map(|c| c.unbind())
    })?;
    let fut = Python::attach(|py| pyo3_async_runtimes::tokio::into_future(coro.bind(py).clone()))?;
    fut.await
}

/// Call a zero-argument awaitable-returning callable and await its result.
async fn await_callable(callable: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let coro: Py<PyAny> = Python::attach(|py| callable.bind(py).call0().map(|c| c.unbind()))?;
    let fut = Python::attach(|py| pyo3_async_runtimes::tokio::into_future(coro.bind(py).clone()))?;
    fut.await
}

/// Set or clear `state.data.active_children`.
fn patch_active_children(
    py: Python<'_>,
    state: &Bound<'_, PyAny>,
    name: Option<&str>,
) -> PyResult<()> {
    let data = state.getattr("data")?;
    match name {
        Some(n) => {
            let patch = PyDict::new(py);
            patch.set_item("active_children", vec![n.to_string()])?;
            data.call_method("patch", (), Some(&patch))?;
        }
        None => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("_delete", ("active_children",))?;
            data.call_method("patch", (), Some(&kwargs))?;
        }
    }
    Ok(())
}

/// Build a runner for `child` via `child_state(state, child).make_runner(...)`.
fn build_child_runner(
    py: Python<'_>,
    state: &Bound<'_, PyAny>,
    child: &Bound<'_, PyAny>,
    gremlin: &Bound<'_, PyAny>,
    scope: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let child_state_fn = wrap_pyfunction!(child_state_py, py)?;
    let cs = child_state_fn.call1((state, child))?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("scope", scope)?;
    kwargs.set_item("record_stage", false)?;
    Ok(cs
        .call_method("make_runner", (child, gremlin), Some(&kwargs))?
        .unbind())
}

/// The key a composite stage is tracked under.
///
/// Root stages may carry no `path` at all, so fall back to `name` — mirroring
/// the Python assertion-free `stage.path or stage.name` idiom.
fn stage_key(path: Option<String>, name: String) -> String {
    match path {
        Some(path) if !path.is_empty() => path,
        _ => name,
    }
}

/// What a Loop iterates over.
///
/// `body_runners` is a test seam: when supplied it takes precedence over
/// `body`, whose runners are instead built fresh on every iteration.
enum LoopBody {
    Provided(Vec<Py<PyAny>>),
    Children(Vec<Py<PyAny>>),
}

/// Build a runner for `child`, await it, and publish it as the active child for
/// the duration of the call.
///
/// The runner is built *before* `active_children` is published, so a failure to
/// construct it cannot strand a stale marker; the marker is cleared on both the
/// success and the failure path of the await.
async fn run_child(
    state: &Py<PyAny>,
    gremlin: &Py<PyAny>,
    stage: &Py<PyAny>,
    child: &Py<PyAny>,
    child_name: &str,
) -> PyResult<()> {
    let runner = Python::attach(|py| {
        build_child_runner(
            py,
            state.bind(py),
            child.bind(py),
            gremlin.bind(py),
            &stage.bind(py).getattr("body")?,
        )
    })?;
    Python::attach(|py| patch_active_children(py, state.bind(py), Some(child_name)))?;
    let awaited = await_callable(runner).await;
    let cleared = Python::attach(|py| patch_active_children(py, state.bind(py), None));
    awaited?;
    cleared
}

/// Read the bail reason stored at `key`, if any.
fn bail_reason(artifacts: &Bound<'_, PyAny>, key: &str) -> PyResult<Option<String>> {
    if !artifacts
        .call_method1("is_registered", (key,))?
        .extract::<bool>()?
    {
        return Ok(None);
    }
    let raw: String = artifacts.call_method1("data_uri", (key,))?.extract()?;
    if !raw.starts_with('/') {
        return Ok(Some(raw.trim().to_string()));
    }
    let path = std::path::Path::new(&raw);
    if !path.exists() {
        return Ok(None);
    }
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(_) => Ok(Some(raw)),
    }
}

/// `_gremlins_core.stages._bail_reason`: the module-level view of
/// [`bail_reason`], kept for parity with the module's historical surface.
#[pyfunction]
#[pyo3(name = "_bail_reason")]
fn bail_reason_py(artifacts: &Bound<'_, PyAny>, key: &str) -> PyResult<Option<String>> {
    bail_reason(artifacts, key)
}

/// `_gremlins_core.stages._is_bail_set`: whether a bail is recorded for
/// `loop_iter` (scoped) or for the run as a whole.
#[pyfunction]
#[pyo3(name = "_is_bail_set")]
fn is_bail_set_py(artifacts: &Bound<'_, PyAny>, loop_iter: &str) -> PyResult<bool> {
    Ok(
        bail_reason(artifacts, &format!("artifact://{loop_iter}/bail"))?.is_some()
            || bail_reason(artifacts, BAIL_KEY)?.is_some(),
    )
}

/// Run a Sequence's body in order, skipping children already marked done.
#[pyfunction]
#[pyo3(name = "_sequence_run_async")]
async fn sequence_run_async(stage: Py<PySequence>, gremlin: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let (state, key, body) = Python::attach(|py| {
        let stage_ref = stage.bind(py);
        let gremlin_ref = gremlin.bind(py);
        let state = gremlin_ref.getattr("state")?;
        if state.is_none() {
            return Err(PyRuntimeError::new_err(
                "sequence stage requires gremlin.state to be initialized",
            ));
        }
        let path: Option<String> = stage_ref.getattr("path")?.extract()?;
        let name: String = stage_ref.getattr("name")?.extract()?;
        let key = stage_key(path, name);
        let body: Vec<Py<PyAny>> = stage_ref.getattr("body")?.extract()?;
        Ok::<_, PyErr>((state.unbind(), key, body))
    })?;

    let done: HashSet<String> =
        Python::attach(|py| state.bind(py).call_method1("done_for", (&key,))?.extract())?;

    for child in &body {
        let child_name: String = Python::attach(|py| child.bind(py).getattr("name")?.extract())?;
        if done.contains(&child_name) {
            continue;
        }
        let stage_obj = Python::attach(|py| stage.clone_ref(py).into_any());
        run_child(&state, &gremlin, &stage_obj, child, &child_name).await?;
        Python::attach(|py| {
            state
                .bind(py)
                .call_method1("mark_done", (&key, &child_name))
                .map(|_| ())
        })?;
    }

    Python::attach(|py| Ok(Py::new(py, Done(RustDone))?.into_any()))
}

/// Run a Loop's body until a stop condition, bail, or exhaustion.
#[pyfunction]
#[pyo3(name = "_loop_run_async")]
async fn loop_run_async(stage: Py<PyLoop>, gremlin: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let (state, stage_path, name, max_iterations, stop_when_exists, interval, body) =
        Python::attach(|py| {
            let stage_ref = stage.bind(py);
            let gremlin_ref = gremlin.bind(py);
            let state = gremlin_ref.getattr("state")?;
            if state.is_none() {
                return Err(PyRuntimeError::new_err(
                    "loop stage requires gremlin.state to be initialized",
                ));
            }
            let path: Option<String> = stage_ref.getattr("path")?.extract()?;
            let name: String = stage_ref.getattr("name")?.extract()?;
            let stage_path = stage_key(path, name.clone());
            let max_iterations: u32 = stage_ref.getattr("max_iterations")?.extract()?;
            let stop_when_exists: Option<String> =
                stage_ref.getattr("stop_when_exists")?.extract()?;
            let interval: Option<f64> = stage_ref.getattr("interval")?.extract()?;
            // `body_runners` is a test seam: a Loop constructed with it may leave
            // `body` unset entirely, so only read `body` when it is unusable.
            let body = match stage_ref
                .getattr("body_runners")?
                .extract::<Option<Vec<Py<PyAny>>>>()?
            {
                Some(runners) => LoopBody::Provided(runners),
                None => LoopBody::Children(stage_ref.getattr("body")?.extract()?),
            };
            Ok::<_, PyErr>((
                state.unbind(),
                stage_path,
                name,
                max_iterations,
                stop_when_exists,
                interval,
                body,
            ))
        })?;

    Python::attach(|py| {
        state
            .bind(py)
            .call_method1("push_loop", (&stage_path,))
            .map(|_| ())
    })?;
    let state_for_loop = Python::attach(|py| state.clone_ref(py));
    let result = loop_iterations(
        state_for_loop,
        gremlin,
        stage,
        name,
        max_iterations,
        stop_when_exists,
        interval,
        body,
    )
    .await;
    let _ = Python::attach(|py| state.bind(py).call_method0("pop_loop").map(|_| ()));
    result
}

#[allow(clippy::too_many_arguments)]
async fn loop_iterations(
    state: Py<PyAny>,
    gremlin: Py<PyAny>,
    stage: Py<PyLoop>,
    name: String,
    max_iterations: u32,
    stop_when_exists: Option<String>,
    interval: Option<f64>,
    body: LoopBody,
) -> PyResult<Py<PyAny>> {
    for iteration in 1..=max_iterations {
        Python::attach(|py| {
            state
                .bind(py)
                .call_method1("set_loop_iteration", (iteration as i32,))
                .map(|_| ())
        })?;
        let loop_iter: String =
            Python::attach(|py| state.bind(py).getattr("loop_iter")?.extract())?;

        // Clear any stale per-iteration bail left by a prior attempt/resume.
        let scoped_bail = format!("artifact://{loop_iter}/bail");
        Python::attach(|py| {
            let artifacts = state.bind(py).getattr("artifacts")?;
            if artifacts
                .call_method1("is_registered", (&scoped_bail,))?
                .extract::<bool>()?
            {
                let bail_path: String = artifacts
                    .call_method1("data_uri", (&scoped_bail,))?
                    .extract()?;
                let _ = std::fs::remove_file(&bail_path);
            }
            Ok::<_, PyErr>(())
        })?;

        log::info!(
            target: "gremlins.executor.state",
            "loop {name}: iteration {iteration}/{max_iterations} starting"
        );

        match &body {
            LoopBody::Provided(runners) => {
                for runner in runners {
                    let runner = Python::attach(|py| runner.clone_ref(py));
                    await_callable(runner).await?;
                }
            }
            LoopBody::Children(children) => {
                let stage_obj = Python::attach(|py| stage.clone_ref(py).into_any());
                for child in children {
                    let child_name: String =
                        Python::attach(|py| child.bind(py).getattr("name")?.extract())?;
                    run_child(&state, &gremlin, &stage_obj, child, &child_name).await?;
                }
            }
        }

        let (bail, reason) = Python::attach(|py| {
            let artifacts = state.bind(py).getattr("artifacts")?;
            let scoped = bail_reason(&artifacts, &format!("artifact://{loop_iter}/bail"))?;
            let global = bail_reason(&artifacts, BAIL_KEY)?;
            Ok::<_, PyErr>((scoped.is_some() || global.is_some(), scoped.or(global)))
        })?;
        if bail {
            let reason = reason.unwrap_or_default();
            Python::attach(|py| {
                state
                    .bind(py)
                    .call_method1("record_bail", (&reason,))
                    .map(|_| ())
            })?;
            return Err(Bail::new_err(reason));
        }

        if let Some(stop) = &stop_when_exists {
            let resolved = stop.replace("{loop_iter}", &loop_iter);
            let live: bool = Python::attach(|py| {
                let artifacts = state.bind(py).getattr("artifacts")?;
                let direct: bool = artifacts.call_method1("is_live", (&resolved,))?.extract()?;
                let prefixed: bool = artifacts
                    .call_method1("is_live", (&format!("artifact://{resolved}"),))?
                    .extract()?;
                Ok::<_, PyErr>(direct || prefixed)
            })?;
            if live {
                return Python::attach(|py| Ok(Py::new(py, Done(RustDone))?.into_any()));
            }
        }

        if iteration == max_iterations {
            let msg = format!("loop exhausted {max_iterations} iterations");
            Python::attach(|py| {
                state
                    .bind(py)
                    .call_method1("record_bail", (&msg,))
                    .map(|_| ())
            })?;
            return Err(Bail::new_err(msg));
        }

        if let Some(secs) = interval {
            // Sleep on the asyncio loop, not Tokio: this coroutine is driven by
            // asyncio and there is no Tokio reactor in scope.
            let coro: Py<PyAny> = Python::attach(|py| {
                py.import("asyncio")
                    .and_then(|m| m.call_method1("sleep", (secs,)))
                    .map(|c| c.unbind())
            })?;
            let fut = Python::attach(|py| {
                pyo3_async_runtimes::tokio::into_future(coro.bind(py).clone())
            })?;
            fut.await?;
        }
    }

    Err(PyRuntimeError::new_err(format!(
        "Loop.run() fell through: max_iterations={max_iterations}"
    )))
}

fn stage_name_from_dict(d: &Bound<'_, PyDict>) -> String {
    for key in ["name", "type"] {
        if let Ok(Some(v)) = d.get_item(key) {
            if let Ok(s) = v.extract::<String>() {
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    "?".to_string()
}

#[pyfunction]
#[pyo3(name = "get_client_from_dict")]
fn get_client_from_dict_py(py: Python<'_>, d: &Bound<'_, PyDict>) -> PyResult<Option<Py<PyAny>>> {
    // Only the `client` key is inspected: other stage keys may hold values
    // that are not JSON-convertible.
    let mut map = HashMap::new();
    if let Some(raw) = d.get_item("client")? {
        if !raw.is_none() {
            // Any non-string collapses to a value the core rejects; the error
            // below names the real Python type.
            let encoded = match raw.is_instance_of::<PyString>() {
                true => serde_json::Value::String(raw.extract()?),
                false => serde_json::Value::Bool(false),
            };
            map.insert("client".to_string(), encoded);
        }
    }
    let name = stage_name_from_dict(d);
    match rust_get_client_from_dict(&map, &name) {
        Ok(Some(spec)) => {
            let parsed: Py<PyAny> = Py::new(py, Client::parse(&spec.0)?)?.into_any();
            Ok(Some(parsed))
        }
        Ok(None) => Ok(None),
        // Render the offending Python type, as the pre-port helper did.
        Err(_) => {
            let kind = match d.get_item("client")? {
                Some(raw) => raw.get_type().repr()?.to_string(),
                None => "NoneType".to_string(),
            };
            Err(pyo3::exceptions::PyValueError::new_err(format!(
                "stage '{name}': 'client' must be a string, got {kind}"
            )))
        }
    }
}

#[pyfunction]
#[pyo3(name = "child_state", signature = (parent, child, *, fan_out = false, child_id = None))]
fn child_state_py(
    py: Python<'_>,
    parent: &Bound<'_, PyAny>,
    child: &Bound<'_, PyAny>,
    fan_out: bool,
    child_id: Option<String>,
) -> PyResult<Py<PyAny>> {
    // A child's explicitly-set client wins; otherwise inherit the parent's.
    let child_client = child.getattr("client")?;
    let child_explicit: bool = child.getattr("client_explicit")?.extract()?;
    let client: Py<PyAny> = if !child_client.is_none() && child_explicit {
        child_client.unbind()
    } else {
        parent.getattr("client")?.unbind()
    };

    let data: Py<PyStateData> = parent.getattr("data")?.extract()?;
    let artifact_dir: PathBuf = parent.getattr("artifact_dir")?.extract()?;
    let artifacts: Py<PyAny> = parent.getattr("artifacts")?.extract()?;
    let cwd: String = parent.getattr("cwd")?.extract()?;
    let args: Py<PyAny> = parent.getattr("args")?.extract()?;
    let pipeline_data: Option<Py<PyAny>> = parent.getattr("pipeline_data")?.extract()?;
    let current_scope_py: Py<PyList> = parent.getattr("current_scope")?.extract()?;
    let child_key: Option<String> = parent.getattr("child_key")?.extract()?;
    let parent_stage: String = parent.getattr("parent_stage")?.extract()?;
    let worktree: Option<PathBuf> = parent.getattr("worktree")?.extract()?;
    let worktree_parent: Option<PathBuf> = parent.getattr("worktree_parent")?.extract()?;
    let base_ref: String = parent.getattr("base_ref")?.extract()?;
    let loop_stack_py: Py<PyList> = parent.getattr("loop_stack")?.extract()?;

    let (artifact_dir, child_key) = if !fan_out {
        (artifact_dir, child_key)
    } else {
        let child_name: String = child.getattr("name")?.extract()?;
        let child_scratch: Option<PathBuf> = child_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|cid| gremlins::config::scratch_root(Some(cid)));
        let params = compute_child_params(&artifact_dir, &child_name, child_scratch.as_deref());
        std::fs::create_dir_all(&params.artifact_dir)?;
        (params.artifact_dir, Some(params.child_key))
    };

    let current_scope: Vec<Py<PyAny>> = current_scope_py
        .bind(py)
        .iter()
        .map(|item| Ok(item.unbind()))
        .collect::<PyResult<_>>()?;
    let loop_stack: Vec<(String, i32)> = loop_stack_py
        .bind(py)
        .iter()
        .map(|item| item.extract())
        .collect::<PyResult<_>>()?;

    let new_state = Py::new(
        py,
        PyState::new(
            py,
            data,
            client,
            artifact_dir,
            artifacts,
            cwd,
            Some(args),
            pipeline_data,
            Some(current_scope),
            child_key,
            parent_stage,
            worktree,
            worktree_parent,
            base_ref,
            Some(loop_stack),
        )?,
    )?;

    if !fan_out {
        let bound = new_state.bind(py);
        let client_str: String = bound.getattr("client")?.str()?.extract()?;
        let data_client: String = bound.getattr("data")?.getattr("client")?.extract()?;
        if client_str != data_client {
            let patch = PyDict::new(py);
            patch.set_item("client", client_str.as_str())?;
            bound
                .getattr("data")?
                .call_method("patch", (), Some(&patch))?;
        }
    }

    Ok(new_state.into_any())
}

#[pyfunction]
#[pyo3(name = "substitute_vars", signature = (text, string_options, extra, framework_subs))]
fn substitute_vars_py(
    text: &str,
    string_options: HashMap<String, String>,
    extra: HashMap<String, String>,
    framework_subs: HashMap<String, String>,
) -> String {
    base::substitute_vars(text, &string_options, &extra, &framework_subs)
}

// --- Parallel execution ---

/// A child of a parallel group, resolved to the pieces the driver needs.
struct ChildSpec {
    key: String,
    state: Py<PyAny>,
    runner: Py<PyAny>,
    /// Present when the child came from a parsed `parallel:` block, which
    /// routes it through the subprocess path.
    stage: Option<Py<PyAny>>,
}

impl ChildSpec {
    fn clone_ref(&self, py: Python<'_>) -> Self {
        ChildSpec {
            key: self.key.clone(),
            state: self.state.clone_ref(py),
            runner: self.runner.clone_ref(py),
            stage: self.stage.as_ref().map(|s| s.clone_ref(py)),
        }
    }
}

/// Everything `parallel_run_async` reads off the gremlin before awaiting.
struct ParallelRun {
    state: Py<PyAny>,
    group_name: String,
    stage_path: String,
    parent_id: String,
    children: Vec<ChildSpec>,
    /// Every child declared in the group, whether or not it was skipped as
    /// already done. Fan-in gathers artifacts from and cleans up after all of
    /// them, so a resumed run must not lose a previously-completed child.
    all_child_keys: Vec<String>,
    /// The gremlin attached to the stage; present only when the group was
    /// wired into a running pipeline. Without it, children get detached
    /// worktrees instead of forks.
    parent_gremlin: Option<Py<PyAny>>,
}

/// Read the group's configuration and build one [`ChildSpec`] per child.
///
/// Children already recorded in `done_children` are skipped, mirroring the
/// Python `run()` which filtered them before building runners.
fn prepare_parallel_run(
    py: Python<'_>,
    stage: &Bound<'_, PyAny>,
    gremlin: &Bound<'_, PyAny>,
) -> PyResult<ParallelRun> {
    let state = gremlin.getattr("state")?;
    if state.is_none() {
        return Err(PyRuntimeError::new_err(
            "parallel stage requires gremlin.state to be initialized",
        ));
    }

    let name: String = stage.getattr("name")?.extract()?;
    let path: Option<String> = stage.getattr("path")?.extract()?;
    let stage_path = stage_key(path, name.clone());
    let body: Vec<Py<PyAny>> = stage.getattr("body")?.extract()?;

    let parent_id: String = state
        .getattr("data")?
        .getattr("gremlin_id")?
        .extract::<Option<String>>()?
        .unwrap_or_default();
    let done: HashSet<String> = state.call_method1("done_for", (&stage_path,))?.extract()?;

    // `copy.copy(state)` then `parent_stage = parent_stage or name`.
    let group_state = py.import("copy")?.call_method1("copy", (&state,))?;
    let parent_stage: String = state.getattr("parent_stage")?.extract()?;
    group_state.setattr(
        "parent_stage",
        if parent_stage.is_empty() {
            name.clone()
        } else {
            parent_stage
        },
    )?;

    let child_state_fn = wrap_pyfunction!(child_state_py, py)?;
    let mut all_child_keys: Vec<String> = Vec::with_capacity(body.len());
    let mut children = Vec::new();
    for child in &body {
        let child_name: String = child.bind(py).getattr("name")?.extract()?;
        all_child_keys.push(child_name.clone());
        if done.contains(&child_name) {
            continue;
        }
        let child_id = if parent_id.is_empty() {
            None
        } else {
            Some(format!("{parent_id}--{name}--{child_name}"))
        };
        let kwargs = PyDict::new(py);
        kwargs.set_item("fan_out", true)?;
        kwargs.set_item("child_id", child_id)?;
        let cs = child_state_fn.call((&group_state, child.bind(py)), Some(&kwargs))?;
        let runner_kwargs = PyDict::new(py);
        runner_kwargs.set_item("scope", PyList::new(py, &body)?)?;
        let runner = cs
            .call_method(
                "make_runner",
                (child.bind(py), gremlin),
                Some(&runner_kwargs),
            )?
            .unbind();
        // Only stages parsed from YAML carry a raw_dict; a bare stage object
        // (test seam) runs in-process through its runner.
        let stage = if child
            .bind(py)
            .getattr("raw_dict")
            .map(|r| !r.is_none())
            .unwrap_or(false)
        {
            Some(child.clone_ref(py))
        } else {
            None
        };
        children.push(ChildSpec {
            key: child_name,
            state: cs.unbind(),
            runner,
            stage,
        });
    }

    let parent_gremlin: Option<Py<PyAny>> = stage
        .getattr("gremlin")
        .ok()
        .filter(|g| !g.is_none())
        .map(|g| g.unbind());
    Ok(ParallelRun {
        state: state.unbind(),
        group_name: name,
        stage_path,
        parent_id,
        children,
        all_child_keys,
        parent_gremlin,
    })
}

/// Run one parallel group end to end: fan-out, parallel, fan-in.
///
/// This drives the same [`ParallelRuntime`] stages that
/// [`PyParallelStage::build_runtime_stages`] hands to the orchestrator, so the
/// production and test paths share one implementation.
#[pyfunction]
#[pyo3(name = "_parallel_run_async")]
async fn parallel_run_async(stage: Py<PyParallelStage>, gremlin: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let run = Python::attach(|py| prepare_parallel_run(py, stage.bind(py), gremlin.bind(py)))?;
    let ParallelRun {
        state,
        group_name,
        stage_path,
        parent_id,
        children,
        all_child_keys,
        parent_gremlin,
    } = run;

    let project_root = gremlins::config::project_root();
    let worktree_parent: Option<PathBuf> =
        Python::attach(|py| state.bind(py).getattr("worktree_parent")?.extract())?;

    let group = Arc::new(ParallelGroupState::new(
        group_name.clone(),
        Python::attach(|py| state.bind(py).getattr("data").map(|d| d.unbind()))?,
    ));

    // Production reports progress through `record_stage_progress`, scoping the
    // sub-stage to this group.
    let set_stage_fn = Python::attach(|py| -> PyResult<Py<PyAny>> {
        let state = state.clone_ref(py);
        let group_name = group_name.clone();
        let f = PyCFunction::new_closure(
            py,
            None,
            None,
            move |args: &Bound<'_, PyTuple>,
                  _kwargs: Option<&Bound<'_, PyDict>>|
                  -> PyResult<Py<PyAny>> {
                let name: String = args.get_item(0)?.extract()?;
                Python::attach(|py| {
                    let kwargs = PyDict::new(py);
                    kwargs.set_item("sub_stage", name)?;
                    state.bind(py).call_method(
                        "record_stage_progress",
                        (group_name.as_str(),),
                        Some(&kwargs),
                    )?;
                    Ok(py.None())
                })
            },
        )?;
        Ok(f.into_any().unbind())
    })?;

    let runtime = Python::attach(|py| {
        Py::new(
            py,
            ParallelRuntime {
                stage: stage.clone_ref(py),
                group,
                children,
                all_child_keys,
                parent_state: state.clone_ref(py),
                parent_gremlin,
                project_root,
                worktree_parent,
                set_stage_fn: Some(set_stage_fn),
                stage_path,
                group_name,
                parent_id,
            },
        )
    })?;

    let fanout = await_runtime_stage(&runtime, "fanout").await;
    fanout?;
    let parallel = await_runtime_stage(&runtime, "parallel").await;
    // Fan-in still runs when the parallel stage failed, so worktrees and child
    // directories are always torn down.
    let fanin = await_runtime_stage(&runtime, "fanin").await;
    parallel?;
    fanin?;

    Python::attach(|py| Ok(Py::new(py, Done(RustDone))?.into_any()))
}

/// Await one of a [`ParallelRuntime`]'s coroutine methods.
async fn await_runtime_stage(runtime: &Py<ParallelRuntime>, method: &str) -> PyResult<Py<PyAny>> {
    let coro = Python::attach(|py| runtime.bind(py).call_method0(method).map(|c| c.unbind()))?;
    await_py(coro).await
}

/// Per-group worktree mirror and attempt tracking, backed by `StateData`.
struct ParallelGroupState {
    group_name: String,
    parent_data: Py<PyAny>,
    worktree_paths: std::sync::Mutex<HashMap<String, PathBuf>>,
    base_head: std::sync::Mutex<String>,
}

impl ParallelGroupState {
    fn new(group_name: String, state: Py<PyAny>) -> Self {
        ParallelGroupState {
            group_name,
            parent_data: state,
            worktree_paths: std::sync::Mutex::new(HashMap::new()),
            base_head: std::sync::Mutex::new(String::new()),
        }
    }

    /// Load persisted worktree paths, unless they are already in memory.
    fn hydrate(&self, py: Python<'_>) {
        if !self.worktree_paths.lock().unwrap().is_empty() {
            return;
        }
        let Ok(data) = self
            .parent_data
            .bind(py)
            .call_method1("parallel_worktrees", (&self.group_name,))
        else {
            return;
        };
        let Ok(base_head) = data.get_item(0).and_then(|v| v.extract::<String>()) else {
            return;
        };
        let Ok(paths) = data
            .get_item(1)
            .and_then(|v| v.extract::<HashMap<String, String>>())
        else {
            return;
        };
        let mut guard = self.worktree_paths.lock().unwrap();
        for (k, v) in paths {
            guard.insert(k, PathBuf::from(v));
        }
        let mut head = self.base_head.lock().unwrap();
        if !base_head.is_empty() {
            *head = base_head;
        }
    }

    fn paths(&self) -> HashMap<String, PathBuf> {
        self.worktree_paths.lock().unwrap().clone()
    }

    fn set_path(&self, key: &str, path: PathBuf) {
        self.worktree_paths
            .lock()
            .unwrap()
            .insert(key.to_string(), path);
    }

    fn base_head(&self) -> String {
        self.base_head.lock().unwrap().clone()
    }

    fn set_base_head(&self, head: String) {
        *self.base_head.lock().unwrap() = head;
    }

    fn persist(&self, py: Python<'_>) {
        let paths: HashMap<String, String> = self
            .worktree_paths
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string_lossy().to_string()))
            .collect();
        let kwargs = PyDict::new(py);
        kwargs.set_item("base_head", self.base_head()).ok();
        kwargs.set_item("paths", paths).ok();
        let _ = self.parent_data.bind(py).call_method(
            "patch_parallel_worktrees",
            (&self.group_name,),
            Some(&kwargs),
        );
    }

    fn clear(&self, py: Python<'_>) {
        self.worktree_paths.lock().unwrap().clear();
        self.set_base_head(String::new());
        let kwargs = PyDict::new(py);
        kwargs.set_item("base_head", py.None()).ok();
        kwargs.set_item("paths", py.None()).ok();
        let _ = self.parent_data.bind(py).call_method(
            "patch_parallel_worktrees",
            (&self.group_name,),
            Some(&kwargs),
        );
    }

    fn record_attempt(&self, py: Python<'_>, child_key: &str, attempt: &str) {
        let _ = self
            .parent_data
            .bind(py)
            .call_method1("patch_parallel_attempt", (child_key, attempt));
    }

    fn clear_attempts(&self, py: Python<'_>) {
        let _ = self
            .parent_data
            .bind(py)
            .call_method0("clear_parallel_attempts");
    }

    fn write_bail(&self, py: Python<'_>, child_key: &str, reason: &str) {
        let _ = self
            .parent_data
            .bind(py)
            .call_method1("write_parallel_bail", (child_key, reason));
    }

    fn read_bail_scan_inputs(&self, py: Python<'_>) -> (Option<PathBuf>, HashMap<String, String>) {
        let Ok(tuple) = self
            .parent_data
            .bind(py)
            .call_method0("read_bail_scan_inputs")
        else {
            return (None, HashMap::new());
        };
        let dir: Option<String> = tuple.get_item(0).ok().and_then(|v| v.extract().ok());
        let attempts: HashMap<String, String> = tuple
            .get_item(1)
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or_default();
        (dir.map(PathBuf::from), attempts)
    }
}

/// Create (or reuse) a worktree for every child, persisting the result.
async fn fan_out(
    group: &Arc<ParallelGroupState>,
    children: &[ChildSpec],
    project_root: &Path,
    worktree_parent: Option<&Path>,
    parent_id: &str,
    parent_state: &Py<PyAny>,
    parent_gremlin: Option<&Py<PyAny>>,
) -> PyResult<()> {
    Python::attach(|py| group.hydrate(py));
    let prior: Vec<String> = group
        .paths()
        .values()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    if !prior.is_empty() {
        remove_worktrees(project_root, &prior).await;
    }
    Python::attach(|py| group.clear(py));

    if !in_git_repo(project_root).await {
        return Ok(());
    }
    prune_worktrees(project_root).await;

    // base_head must come from the parent worktree, not project_root: the
    // latter diverges from the fork once implement commits.
    let parent_worktree: Option<PathBuf> =
        Python::attach(|py| parent_state.bind(py).getattr("worktree")?.extract())?;
    let base_ref = parent_worktree
        .as_deref()
        .unwrap_or(project_root)
        .to_string_lossy()
        .to_string();
    group.set_base_head(head_sha(&base_ref).await);

    let result = fan_out_children(
        group,
        children,
        project_root,
        worktree_parent,
        parent_id,
        parent_gremlin,
    )
    .await;
    if let Err(err) = result {
        let paths: Vec<String> = group
            .paths()
            .values()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        remove_worktrees(project_root, &paths).await;
        Python::attach(|py| group.clear(py));
        return Err(err);
    }
    Python::attach(|py| group.persist(py));
    Ok(())
}

/// Build the single-stage branch pipeline a forked child runs with.
///
/// Mirrors the pre-port `_branch_pipeline`: the child inherits the parent
/// pipeline's path, default client, base ref, and bootstrap, but its `stages`
/// list contains only the child's own stage. Returns `None` when the child has
/// no parsed stage (a bare test-seam stage), matching Python's
/// `branch_stage is None or branch_stage.raw_dict is None` guard.
fn build_branch_pipeline(
    py: Python<'_>,
    stage: &Py<PyAny>,
    child_state: &Py<PyAny>,
) -> PyResult<Option<Py<PyAny>>> {
    let stage = stage.bind(py);
    if stage.getattr("raw_dict")?.is_none() {
        return Ok(None);
    }

    let schemas = py.import("_gremlins_core.schemas")?;
    let pipeline_cls = schemas.getattr("Pipeline")?;
    let bootstrap_cls = schemas.getattr("Bootstrap")?;

    let parent_pipeline = child_state.bind(py).getattr("pipeline_data")?;
    let (path, default_client, base_ref, bootstrap) = if parent_pipeline.is_none() {
        (
            PathBuf::from("."),
            py.None(),
            "current".to_string(),
            bootstrap_cls.call0()?.unbind(),
        )
    } else {
        (
            parent_pipeline.getattr("path")?.extract::<PathBuf>()?,
            parent_pipeline.getattr("default_client")?.unbind(),
            parent_pipeline.getattr("base_ref")?.extract::<String>()?,
            parent_pipeline.getattr("bootstrap")?.unbind(),
        )
    };

    let name: String = stage.getattr("name")?.extract()?;
    let stages = PyList::new(py, [stage])?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("default_client", default_client)?;
    kwargs.set_item("base_ref", base_ref)?;
    kwargs.set_item("bootstrap", bootstrap)?;
    let pipeline = pipeline_cls.call((name, path, stages), Some(&kwargs))?;
    Ok(Some(pipeline.unbind()))
}

async fn fan_out_children(
    group: &Arc<ParallelGroupState>,
    children: &[ChildSpec],
    project_root: &Path,
    worktree_parent: Option<&Path>,
    parent_id: &str,
    parent_gremlin: Option<&Py<PyAny>>,
) -> PyResult<()> {
    for child in children {
        if let (false, Some(gremlin)) = (parent_id.is_empty(), parent_gremlin) {
            let child_id = format!("{parent_id}--{}--{}", group.group_name, child.key);
            let forked: Py<PyAny> = Python::attach(|py| {
                let kwargs = PyDict::new(py);
                kwargs.set_item("parent_id", parent_id)?;
                kwargs.set_item("group_name", &group.group_name)?;
                kwargs.set_item("child_key", &child.key)?;
                // The child must run with its own single-stage branch pipeline,
                // not inherit the parent's pipeline metadata.
                let branch_pipeline = match child.stage.as_ref() {
                    Some(stage) => build_branch_pipeline(py, stage, &child.state)?,
                    None => None,
                };
                kwargs.set_item("pipeline", branch_pipeline)?;
                let coro = gremlin.bind(py).call_method(
                    "fork",
                    (child.state.bind(py), child_id.as_str()),
                    Some(&kwargs),
                )?;
                let fut = pyo3_async_runtimes::tokio::into_future(coro)?;
                Ok::<_, PyErr>(fut)
            })?
            .await?;

            let worktree: Option<PathBuf> = Python::attach(|py| {
                let wt = forked.bind(py).getattr("worktree")?;
                let wt: Option<PathBuf> = wt.extract()?;
                if let Some(ref path) = wt {
                    child.state.bind(py).setattr("worktree", path)?;
                }
                Ok::<_, PyErr>(wt)
            })?;
            if let Some(path) = worktree {
                group.set_path(&child.key, path);
            }
        } else {
            let wt_dir =
                setup_detached_worktree(project_root, &group.base_head(), worktree_parent).await?;
            let wt_path = PathBuf::from(wt_dir);
            group.set_path(&child.key, wt_path.clone());
            Python::attach(|py| child.state.bind(py).setattr("worktree", &wt_path))?;
        }
    }
    Ok(())
}

/// Dispatch every child concurrently, honouring `max_concurrent`.
#[allow(clippy::too_many_arguments)]
async fn dispatch_children(
    children: &[ChildSpec],
    state: &Py<PyAny>,
    group: &Arc<ParallelGroupState>,
    stage_path: &str,
    group_name: &str,
    parent_id: &str,
    max_concurrent: Option<u32>,
    cancel_on_bail: bool,
    cancel: Arc<AtomicBool>,
) -> PyResult<()> {
    if children.is_empty() {
        return Ok(());
    }

    Python::attach(|py| group.hydrate(py));
    let paths = group.paths();
    for child in children {
        if let Some(wt) = paths.get(&child.key) {
            Python::attach(|py| {
                let current = child.state.bind(py).getattr("worktree")?;
                if current.is_none() {
                    child.state.bind(py).setattr("worktree", wt)?;
                }
                Ok::<_, PyErr>(())
            })?;
        }
    }

    // Snapshot of all dispatched keys; not updated as children finish.
    let active: Vec<String> = children.iter().map(|c| c.key.clone()).collect();
    Python::attach(|py| {
        let patch = PyDict::new(py);
        patch.set_item("active_children", &active)?;
        state
            .bind(py)
            .getattr("data")?
            .call_method("patch", (), Some(&patch))?;
        Ok::<_, PyErr>(())
    })?;
    // Cancelling this future drops it mid-await, so the completion path below is
    // not guaranteed to run. The guard clears the marker on that path too.
    let mut active_guard = Python::attach(|py| ActiveChildrenGuard::new(state.clone_ref(py)));

    let semaphore = max_concurrent.map(|n| Arc::new(Semaphore::new(n as usize)));
    // Each child runs as an abortable future so a bail can cancel in-flight
    // siblings. `abortable` is runtime-agnostic (unlike `JoinSet`), which
    // matters because these futures are polled by the asyncio loop.
    let mut pending = futures::stream::FuturesUnordered::new();
    let mut handles: Vec<futures::future::AbortHandle> = Vec::with_capacity(children.len());
    for child in children {
        let child_key = child.key.clone();
        let child_state = Python::attach(|py| child.state.clone_ref(py));
        let runner = Python::attach(|py| child.runner.clone_ref(py));
        let stage_obj = child
            .stage
            .as_ref()
            .map(|s| Python::attach(|py| s.clone_ref(py)));
        let state = Python::attach(|py| state.clone_ref(py));
        let group = group.clone();
        let stage_path = stage_path.to_string();
        let group_name = group_name.to_string();
        let parent_id = parent_id.to_string();
        let cancel = cancel.clone();
        let semaphore = semaphore.clone();
        let (fut, handle) = futures::future::abortable(async move {
            let _permit = match semaphore {
                Some(sem) => Some(sem.acquire_owned().await.expect("semaphore open")),
                None => None,
            };
            if cancel_on_bail && cancel.load(Ordering::SeqCst) {
                return Ok(());
            }
            run_parallel_child(
                &child_key,
                &child_state,
                &runner,
                stage_obj.as_ref(),
                &state,
                &group,
                &stage_path,
                &group_name,
                &parent_id,
                cancel_on_bail,
                &cancel,
            )
            .await
        });
        handles.push(handle);
        pending.push(fut);
    }

    // Drain the set. Once a child bails (the cancel flag is set), abort every
    // still-running sibling so in-flight children are cancelled rather than
    // merely skipped at dispatch time. Aborting an already-finished future is
    // a no-op.
    let mut first_error: Option<PyErr> = None;
    while let Some(joined) = pending.next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                if first_error.is_none() {
                    first_error = Some(err);
                } else {
                    log::error!(
                        target: "gremlins.stages.parallel",
                        "parallel child also failed: {err}"
                    );
                }
            }
            // A sibling aborted by the bail path; nothing to record.
            Err(_aborted) => {}
        }
        if cancel_on_bail && cancel.load(Ordering::SeqCst) {
            for handle in &handles {
                handle.abort();
            }
        }
    }

    active_guard.clear();

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Clears `state.data.active_children` when dropped.
///
/// The group publishes the snapshot of dispatched children before draining the
/// child futures. Cancellation unwinds this future mid-await, so the normal
/// completion path may never run; a guard keeps the marker from outliving the
/// stage in that case, which would otherwise leave fleet/resume state stale.
struct ActiveChildrenGuard {
    state: Option<Py<PyAny>>,
}

impl ActiveChildrenGuard {
    fn new(state: Py<PyAny>) -> Self {
        ActiveChildrenGuard { state: Some(state) }
    }

    /// Clear the marker now. Idempotent: a later drop becomes a no-op.
    fn clear(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        Python::attach(|py| {
            let kwargs = PyDict::new(py);
            let cleared = kwargs
                .set_item("_delete", ("active_children",))
                .and_then(|()| {
                    state
                        .bind(py)
                        .getattr("data")?
                        .call_method("patch", (), Some(&kwargs))
                });
            if let Err(err) = cleared {
                log::warn!(
                    target: "gremlins.stages.parallel",
                    "could not clear active_children: {err}"
                );
            }
        });
    }
}

impl Drop for ActiveChildrenGuard {
    fn drop(&mut self) {
        self.clear();
    }
}

/// How a subprocess child finished: cleanly, or by bailing.
///
/// A bail is not an error (the group decides via its bail policy), but it must
/// not be recorded as `done` — hence the explicit distinction rather than
/// collapsing both into `Ok(())`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildOutcome {
    /// The child completed successfully (`done` or `needs_fix`).
    Done,
    /// The child bailed; its bail file has already been written.
    Bailed,
}

/// Run a single child, translating bail/error into the group's bookkeeping.
#[allow(clippy::too_many_arguments)]
async fn run_parallel_child(
    child_key: &str,
    child_state: &Py<PyAny>,
    runner: &Py<PyAny>,
    stage_obj: Option<&Py<PyAny>>,
    state: &Py<PyAny>,
    group: &Arc<ParallelGroupState>,
    stage_path: &str,
    group_name: &str,
    parent_id: &str,
    cancel_on_bail: bool,
    cancel: &Arc<AtomicBool>,
) -> PyResult<()> {
    let outcome = match stage_obj {
        Some(stage) => {
            run_child_subprocess(
                stage,
                child_state,
                child_key,
                group,
                group_name,
                parent_id,
                cancel_on_bail,
                cancel,
            )
            .await
        }
        None => await_callable(Python::attach(|py| runner.clone_ref(py)))
            .await
            .map(|_| ChildOutcome::Done),
    };

    match outcome {
        Ok(ChildOutcome::Done) => {
            Python::attach(|py| {
                state
                    .bind(py)
                    .call_method1("mark_done", (stage_path, child_key))
                    .map(|_| ())
            })?;
            Ok(())
        }
        // A bailed child is never recorded as done; the bail file drives the
        // group's fan-in decision.
        Ok(ChildOutcome::Bailed) => Ok(()),
        Err(err) => {
            let is_bail = Python::attach(|py| err.is_instance_of::<Bail>(py));
            if is_bail {
                if cancel_on_bail {
                    cancel.store(true, Ordering::SeqCst);
                }
                let reason = Python::attach(|py| bail_message(py, &err))?;
                // `collect_bails` resolves each child's bail file through
                // `parallel_attempts[child_key]`. Only the subprocess path
                // records an attempt itself, so an in-process bail must mint
                // one here — without it the bail is invisible at fan-in.
                Python::attach(|py| {
                    group.record_attempt(py, child_key, &in_process_attempt(child_key));
                    group.write_bail(py, child_key, &reason);
                });
                Ok(())
            } else {
                if cancel_on_bail {
                    cancel.store(true, Ordering::SeqCst);
                }
                Err(err)
            }
        }
    }
}

/// The attempt id recorded for an in-process child that bails.
///
/// Deterministic per child, mirroring `write_parallel_bail`'s top-level
/// `attempt` fallback. A parallel group does not nest, so child keys are unique
/// within a group and this cannot collide across children.
fn in_process_attempt(child_key: &str) -> String {
    format!("{child_key}-inprocess")
}

/// The bail detail carried by a `Bail` exception, or an empty string.
fn bail_message(py: Python<'_>, err: &PyErr) -> PyResult<String> {
    let args = err.value(py).getattr("args")?;
    if args.len()? == 0 {
        return Ok(String::new());
    }
    Ok(args.get_item(0)?.str()?.to_string())
}

/// Spawn one child through `gremlins.spawn.child` and fold in its cost.
#[allow(clippy::too_many_arguments)]
async fn run_child_subprocess(
    stage_obj: &Py<PyAny>,
    child_state: &Py<PyAny>,
    child_key: &str,
    group: &Arc<ParallelGroupState>,
    group_name: &str,
    parent_id: &str,
    cancel_on_bail: bool,
    cancel: &Arc<AtomicBool>,
) -> PyResult<ChildOutcome> {
    let child_id = if parent_id.is_empty() {
        String::new()
    } else {
        format!("{parent_id}--{group_name}--{child_key}")
    };
    let attempt = format!("{child_key}-{}", rust_state::token_hex(4));
    Python::attach(|py| group.record_attempt(py, child_key, &attempt));

    let spec_path = Python::attach(|py| -> PyResult<PathBuf> {
        let artifact_dir: PathBuf = child_state.bind(py).getattr("artifact_dir")?.extract()?;
        let spec = build_child_spec_dict(
            stage_obj.bind(py),
            child_state.bind(py),
            child_key,
            &attempt,
            group_name,
            &child_id,
        )?;
        let spec_path = artifact_dir.join(format!("spec_{attempt}.json"));
        let spec_json =
            serde_json::to_string(&spec).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        std::fs::write(&spec_path, spec_json)?;
        Ok(spec_path)
    })?;

    let timeout_s = Python::attach(|py| parse_child_timeout(stage_obj.bind(py), child_key))?;
    let python_exe: String = Python::attach(|py| -> PyResult<String> {
        py.import("sys")?.getattr("executable")?.extract()
    })?;
    let log_path = Python::attach(|py| -> PyResult<Option<PathBuf>> {
        let artifact_dir: PathBuf = child_state.bind(py).getattr("artifact_dir")?.extract()?;
        let log_path = artifact_dir.parent().map(|p| p.join("log"));
        Ok(match log_path {
            Some(path) if path.parent().map(|dir| dir.exists()).unwrap_or(false) => Some(path),
            _ => None,
        })
    })?;

    // The parallel future is polled on the asyncio thread, which has no Tokio
    // reactor, so the reactor-dependent lifecycle runs on the Tokio runtime and
    // is awaited here as a plain join handle.
    let lifecycle = pyo3_async_runtimes::tokio::get_runtime().spawn(run_child_lifecycle(
        python_exe,
        spec_path.clone(),
        attempt,
        timeout_s,
        log_path,
        child_key.to_string(),
    ));
    let mut task_guard = ChildTaskGuard::new(lifecycle);
    let returncode = task_guard.result().await.map_err(PyRuntimeError::new_err)?;
    task_guard.disarm();

    let result = read_child_result(&spec_path, returncode, child_key)?;
    let cost = result
        .get("cost_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    if cost > 0.0 && cost.is_finite() {
        Python::attach(|py| {
            let _ = child_state
                .bind(py)
                .getattr("data")
                .and_then(|d| d.call_method1("add_subprocess_cost", (cost,)));
        });
    }

    match result.get("status").and_then(|v| v.as_str()) {
        Some("done") | Some("needs_fix") => Ok(ChildOutcome::Done),
        Some("bail") => {
            if cancel_on_bail {
                cancel.store(true, Ordering::SeqCst);
            }
            let detail = result
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Python::attach(|py| group.write_bail(py, child_key, &detail));
            Ok(ChildOutcome::Bailed)
        }
        _ => {
            let detail = result.get("detail").and_then(|v| v.as_str()).unwrap_or("");
            Err(PyRuntimeError::new_err(format!(
                "parallel child {child_key:?} error: {detail}"
            )))
        }
    }
}

/// Drive one child's process lifecycle on the Tokio runtime.
///
/// Returns the child's Python-style returncode, or an error message when the
/// wait failed (a timeout, or a reaping failure).
async fn run_child_lifecycle(
    python_exe: String,
    spec_path: PathBuf,
    attempt: String,
    timeout_s: Option<f64>,
    log_path: Option<PathBuf>,
    child_key: String,
) -> Result<Option<i32>, String> {
    let (child, pumps) =
        proc::spawn_with_pumps(&python_exe, &spec_path, &attempt, log_path.as_deref())
            .await
            .map_err(|err| err.to_string())?;
    // If this task is dropped (asyncio cancellation), the explicit cleanup below
    // never runs; `ChildGuard` closes that gap.
    let mut guard = ChildGuard::new(child, pumps);

    let wait_result = proc::wait_child_proc(guard.child_mut(), timeout_s, &child_key).await;
    // The pumps reach EOF once the child's pipes close; drain them before
    // reading the result so no output is lost.
    guard.drain_pumps().await;
    let returncode = match &wait_result {
        Ok(status) => Some(proc::exit_code(status)),
        Err(_) => None,
    };
    guard.disarm();
    wait_result
        .map(|_| returncode)
        .map_err(|err| err.to_string())
}

/// Aborts the child's Tokio task if the owning future is dropped.
///
/// The lifecycle runs on the Tokio runtime, so dropping the asyncio future that
/// awaits it would otherwise leave the task — and the child — running. Aborting
/// drops the task's future, which drops its [`ChildGuard`] and tears the child
/// down.
struct ChildTaskGuard {
    handle: Option<tokio::task::JoinHandle<Result<Option<i32>, String>>>,
    armed: bool,
}

impl ChildTaskGuard {
    fn new(handle: tokio::task::JoinHandle<Result<Option<i32>, String>>) -> Self {
        ChildTaskGuard {
            handle: Some(handle),
            armed: true,
        }
    }

    /// Await the lifecycle task's result.
    async fn result(&mut self) -> Result<Option<i32>, String> {
        self.handle
            .as_mut()
            .expect("handle is present while armed")
            .await
            .map_err(|err| format!("child task failed: {err}"))?
    }

    /// Stand down: the lifecycle completed and was awaited.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildTaskGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Some(handle) = self.handle.take() {
                handle.abort();
            }
        }
    }
}

/// Tears down a subprocess child when the owning future is dropped.
///
/// `asyncio` cancellation drops the Rust future, so the explicit cleanup path
/// in [`run_child_subprocess`] never runs; this guard closes that gap.
struct ChildGuard {
    child: Option<tokio::process::Child>,
    pumps: Vec<tokio::task::JoinHandle<()>>,
    armed: bool,
}

impl ChildGuard {
    fn new(child: tokio::process::Child, pumps: Vec<tokio::task::JoinHandle<()>>) -> Self {
        ChildGuard {
            child: Some(child),
            pumps,
            armed: true,
        }
    }

    /// The live child handle, for waiting on it.
    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child.as_mut().expect("child is present while armed")
    }

    /// Await the pumps so the child's output is fully relayed.
    async fn drain_pumps(&mut self) {
        proc::drain_pumps(std::mem::take(&mut self.pumps)).await;
    }

    /// Stand down: the explicit cleanup path already ran.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Abort the pumps first: they are tokio tasks reading the child's pipes.
        for pump in &self.pumps {
            pump.abort();
        }

        // Hand the blocking SIGTERM→SIGKILL wait to a dedicated thread. `Drop`
        // runs on the Tokio worker polling the cancelled future; blocking there
        // would stall the runtime for the whole grace period and delay sibling
        // cancellation. The thread owns the cleanup and runs to completion
        // independently of the runtime.
        if let Some(mut child) = self.child.take() {
            if let Some(pid) = child.id() {
                std::thread::spawn(move || proc::terminate_with_grace_blocking(pid, 10.0));
            }
            let _ = child.start_kill();
        }
    }
}

/// The parsed result object for a finished child, or a descriptive error.
fn read_child_result(
    spec_path: &Path,
    returncode: Option<i32>,
    child_key: &str,
) -> PyResult<serde_json::Map<String, serde_json::Value>> {
    let result_path = PathBuf::from(format!("{}.result", spec_path.to_string_lossy()));
    if !result_path.exists() {
        return Err(PyRuntimeError::new_err(missing_result_detail(
            child_key, returncode,
        )));
    }
    let text = std::fs::read_to_string(&result_path)?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(PyRuntimeError::new_err(format!(
            "parallel child {child_key:?}: result file is not a JSON object"
        ))),
    }
}

/// The `timeout_seconds` for a child stage, or `None` when unset/non-positive.
fn parse_child_timeout(stage_obj: &Bound<'_, PyAny>, child_key: &str) -> PyResult<Option<f64>> {
    let raw_dict = stage_obj.getattr("raw_dict")?;
    if raw_dict.is_none() {
        return Ok(None);
    }
    let raw = match raw_dict.cast::<PyDict>()?.get_item("timeout_seconds")? {
        Some(v) if !v.is_none() => v,
        _ => return Ok(None),
    };
    let parsed = raw.extract::<f64>().map_err(|_| {
        pyo3::exceptions::PyValueError::new_err(format!(
            "parallel child {child_key:?}: 'timeout_seconds' must be a number, got {raw:?}"
        ))
    })?;
    Ok(if parsed > 0.0 { Some(parsed) } else { None })
}

/// A human-readable reason for a child that exited without a result file.
fn missing_result_detail(child_key: &str, returncode: Option<i32>) -> String {
    match returncode {
        None => format!(
            "parallel child {child_key:?}: subprocess exited with no result file (returncode unavailable)"
        ),
        Some(0) => format!("parallel child {child_key:?} exited 0 without writing result"),
        Some(rc) if rc < 0 => format!(
            "parallel child {child_key:?} terminated by {} with no result file",
            signal_name(-rc)
        ),
        Some(rc) => format!(
            "parallel child {child_key:?} exited with returncode {rc} and no result file"
        ),
    }
}

fn signal_name(signum: i32) -> String {
    match signum {
        1 => "SIGHUP".to_string(),
        2 => "SIGINT".to_string(),
        3 => "SIGQUIT".to_string(),
        6 => "SIGABRT".to_string(),
        9 => "SIGKILL".to_string(),
        13 => "SIGPIPE".to_string(),
        14 => "SIGALRM".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("signal {other}"),
    }
}

/// The JSON spec handed to `gremlins.spawn.child`.
fn build_child_spec_dict(
    stage_obj: &Bound<'_, PyAny>,
    child_st: &Bound<'_, PyAny>,
    child_key: &str,
    attempt: &str,
    group_name: &str,
    child_id: &str,
) -> PyResult<serde_json::Map<String, serde_json::Value>> {
    let raw_dict = stage_obj.getattr("raw_dict")?;
    let stage_dict = crate::python::json_conv::py_to_value(&raw_dict)?;
    let client: String = child_st.getattr("client")?.str()?.extract()?;
    let parent_id: String = child_st
        .getattr("data")?
        .getattr("gremlin_id")?
        .extract::<Option<String>>()?
        .unwrap_or_default();
    let worktree: Option<PathBuf> = child_st.getattr("worktree")?.extract()?;
    let worktree_parent: Option<PathBuf> = child_st.getattr("worktree_parent")?.extract()?;
    let pipeline_path: Option<String> = child_st
        .getattr("data")?
        .getattr("pipeline_path")?
        .extract::<Option<String>>()?;
    let parent_stage: String = child_st.getattr("parent_stage")?.extract()?;
    let base_ref: String = child_st.getattr("base_ref")?.extract()?;
    let bootstrap: Vec<String> = {
        let pipeline_data = child_st.getattr("pipeline_data")?;
        if pipeline_data.is_none() {
            Vec::new()
        } else {
            let bootstrap = pipeline_data.getattr("bootstrap")?;
            if bootstrap.is_none() {
                Vec::new()
            } else {
                bootstrap.getattr("cmds")?.extract::<Vec<String>>()?
            }
        }
    };

    let mut map = serde_json::Map::new();
    map.insert("stage_dict".into(), stage_dict);
    map.insert("client".into(), serde_json::Value::String(client));
    map.insert(
        "child_id".into(),
        serde_json::Value::String(child_id.to_string()),
    );
    map.insert("parent_id".into(), serde_json::Value::String(parent_id));
    map.insert(
        "group_name".into(),
        serde_json::Value::String(group_name.to_string()),
    );
    map.insert(
        "worktree".into(),
        worktree
            .map(|p| serde_json::Value::String(p.to_string_lossy().to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    map.insert(
        "worktree_parent".into(),
        worktree_parent
            .map(|p| serde_json::Value::String(p.to_string_lossy().to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    map.insert(
        "pipeline_path".into(),
        pipeline_path
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    );
    map.insert(
        "child_key".into(),
        serde_json::Value::String(child_key.to_string()),
    );
    map.insert(
        "attempt".into(),
        serde_json::Value::String(attempt.to_string()),
    );
    map.insert(
        "parent_stage".into(),
        serde_json::Value::String(parent_stage),
    );
    map.insert("base_ref".into(), serde_json::Value::String(base_ref));
    map.insert(
        "bootstrap".into(),
        serde_json::Value::Array(
            bootstrap
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    Ok(map)
}

/// Gather child artifacts, decide the group bail, and tear down worktrees.
///
/// `children` is the *filtered* set the parallel stage actually executed (a
/// resumed run omits children already recorded `done`); `all_child_keys` is the
/// group's full membership. Artifact gathering and directory cleanup must use
/// the full set, or a resumed run would drop the artifacts of — and leave the
/// scratch directory of — any child completed by an earlier attempt. Bail
/// scanning keeps using the executed set, which is what wrote the attempts.
#[allow(clippy::too_many_arguments)]
async fn fan_in(
    group: &Arc<ParallelGroupState>,
    all_child_keys: &[String],
    children: &[ChildSpec],
    state: &Py<PyAny>,
    stage_path: &str,
    group_name: &str,
    parent_id: &str,
    bail_policy: BailPolicy,
    project_root: &Path,
) -> PyResult<()> {
    Python::attach(|py| group.hydrate(py));
    let child_keys: Vec<String> = children.iter().map(|c| c.key.clone()).collect();
    let state_for_teardown = Python::attach(|py| state.clone_ref(py));

    // Capture the artifact-gathering result rather than propagating with `?`:
    // teardown must run even when gathering fails, or detached worktrees and
    // child state directories are left behind on disk.
    let gather_result = gather_child_artifacts(all_child_keys, state, group_name, parent_id);

    let fan_in_result = do_fan_in(
        group,
        &child_keys,
        state,
        stage_path,
        group_name,
        bail_policy,
        project_root,
    )
    .await;

    // Teardown is guarded: if this future is dropped mid-await (asyncio
    // cancellation, or `--resume-from <group>-fanin`), the guard still removes
    // child dirs and detached worktrees. The pre-port `_fan_in` used a
    // `finally`; this is the structured-concurrency equivalent.
    let mut teardown = TeardownGuard::new(
        group.clone(),
        all_child_keys.to_vec(),
        state_for_teardown,
        group_name.to_string(),
        parent_id.to_string(),
        project_root.to_path_buf(),
    );
    teardown.run();

    // Surface the first failure: a gathering error takes precedence over the
    // fan-in outcome, since it means artifacts were never merged.
    gather_result?;
    fan_in_result
}

/// Owns fan-in teardown and runs it exactly once, even on cancellation.
///
/// `run` performs the cleanup eagerly on the normal path; `Drop` covers the
/// path where the enclosing future is dropped before reaching it. The cleanup
/// itself is blocking Rust (no Python coroutines), so it cannot be interrupted
/// halfway by a cancellation that arrives while it is in flight.
struct TeardownGuard {
    group: Arc<ParallelGroupState>,
    child_keys: Vec<String>,
    state: Py<PyAny>,
    group_name: String,
    parent_id: String,
    project_root: PathBuf,
    done: bool,
}

impl TeardownGuard {
    fn new(
        group: Arc<ParallelGroupState>,
        child_keys: Vec<String>,
        state: Py<PyAny>,
        group_name: String,
        parent_id: String,
        project_root: PathBuf,
    ) -> Self {
        TeardownGuard {
            group,
            child_keys,
            state,
            group_name,
            parent_id,
            project_root,
            done: false,
        }
    }

    /// Run the cleanup now. Idempotent: a later drop becomes a no-op.
    fn run(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        remove_child_dirs(
            &self.child_keys,
            &self.state,
            &self.group_name,
            &self.parent_id,
        );
        teardown_worktrees_blocking(&self.group, &self.project_root);
    }
}

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        self.run();
    }
}

/// Copy child artifact bindings into the parent registry before dirs vanish.
fn gather_child_artifacts(
    child_keys: &[String],
    state: &Py<PyAny>,
    group_name: &str,
    parent_id: &str,
) -> PyResult<()> {
    if parent_id.is_empty() {
        return Ok(());
    }
    Python::attach(|py| {
        let parent_artifacts = state.bind(py).getattr("artifacts")?;
        let parent_keys: HashSet<String> = parent_artifacts
            .call_method0("keys")?
            .extract::<Vec<String>>()?
            .into_iter()
            .collect();

        // key -> [(child_key, child_registry)]
        let mut per_key: HashMap<String, Vec<(String, Py<PyAny>)>> = HashMap::new();
        for child_key in child_keys {
            let child_id = format!("{parent_id}--{group_name}--{child_key}");
            let scratch = gremlins::config::scratch_root(Some(&child_id));
            let child_reg_path = scratch.join("registry.json");
            if !child_reg_path.exists() {
                continue;
            }
            let registry_cls = py
                .import("_gremlins_core.artifacts")?
                .getattr("ArtifactRegistry")?;
            let child_registry = registry_cls.call_method1(
                "from_registry_file",
                (
                    child_reg_path.to_string_lossy().to_string(),
                    scratch.join("artifacts").to_string_lossy().to_string(),
                ),
            )?;
            let keys: Vec<String> = child_registry.call_method0("keys")?.extract()?;
            for key in keys {
                if parent_keys.contains(&key) {
                    continue;
                }
                per_key
                    .entry(key)
                    .or_default()
                    .push((child_key.clone(), child_registry.clone().unbind()));
            }
        }

        for (key, producers) in per_key {
            let multi = producers.len() > 1;
            for (child_key, child_registry) in producers {
                let kwargs = PyDict::new(py);
                if multi {
                    let key_map = PyDict::new(py);
                    key_map.set_item(&key, format!("{key}/{child_key}"))?;
                    kwargs.set_item("key_map", key_map)?;
                }
                kwargs.set_item("copy_files", true)?;
                let keys = pyo3::types::PySet::empty(py)?;
                keys.add(&key)?;
                kwargs.set_item("keys", keys)?;
                parent_artifacts.call_method(
                    "merge_from",
                    (child_registry.bind(py),),
                    Some(&kwargs),
                )?;
            }
        }
        Ok::<_, PyErr>(())
    })
}

/// Prune worktrees, apply the bail policy, and clear per-run bookkeeping.
#[allow(clippy::too_many_arguments)]
async fn do_fan_in(
    group: &Arc<ParallelGroupState>,
    child_keys: &[String],
    state: &Py<PyAny>,
    stage_path: &str,
    group_name: &str,
    bail_policy: BailPolicy,
    project_root: &Path,
) -> PyResult<()> {
    prune_worktrees(project_root).await;

    let (state_dir, attempts) = Python::attach(|py| group.read_bail_scan_inputs(py));
    let bailed = match state_dir {
        Some(dir) => gremlins::stages::parallel_bail::collect_bails(&dir, child_keys, &attempts),
        None => Vec::new(),
    };
    let decision = gremlins::stages::parallel_bail::decide(&bailed, child_keys.len(), bail_policy);

    if decision.should_bail {
        let bail_class = decision.bail_class();
        let detail = decision.bail_detail();
        Python::attach(|py| {
            let _ = state
                .bind(py)
                .getattr("data")
                .and_then(|d| d.call_method1("write_bail_file", (bail_class, detail)));
        });
    }
    Python::attach(|py| group.clear_attempts(py));
    if !decision.should_bail {
        Python::attach(|py| {
            let _ = state.bind(py).call_method1("clear_done", (stage_path,));
        });
    }
    if decision.should_bail {
        return Err(Bail::new_err(format!(
            "parallel group {group_name:?} bailed ({} child(ren), policy={:?})",
            bailed.len(),
            bail_policy.as_str()
        )));
    }
    Ok(())
}

/// Remove child state dirs, preserving their logs first.
fn remove_child_dirs(child_keys: &[String], state: &Py<PyAny>, group_name: &str, parent_id: &str) {
    if parent_id.is_empty() {
        return;
    }
    let state_root = gremlins::config::state_root();
    let prefix = format!("{parent_id}--{group_name}--");
    if let Ok(entries) = std::fs::read_dir(&state_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && entry.path().is_dir() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    let state_dir = state_root.join(parent_id);
    let preserve = state_dir.is_dir();
    if !preserve {
        log::warn!(
            target: "gremlins.stages.parallel",
            "parallel {group_name}: parent state dir {} is missing, child logs not preserved",
            state_dir.display()
        );
    }
    let _ = state;
    for child_key in child_keys {
        let child_id = format!("{parent_id}--{group_name}--{child_key}");
        let child_scratch = gremlins::config::scratch_root(Some(&child_id));
        let child_log = child_scratch.join("log");
        if preserve && child_log.is_file() {
            save_child_log(
                &child_log,
                &state_dir.join("logs").join(format!("{child_key}.log")),
                group_name,
            );
        }
        let _ = std::fs::remove_dir_all(&child_scratch);
    }
}

fn save_child_log(src: &Path, dest: &Path, group_name: &str) {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::copy(src, dest) {
        Ok(_) => log::debug!(
            target: "gremlins.stages.parallel",
            "parallel {group_name}: saved child log {}",
            dest.display()
        ),
        Err(exc) => log::warn!(
            target: "gremlins.stages.parallel",
            "parallel {group_name}: could not preserve child log {} -> {}: {exc}",
            src.display(),
            dest.display()
        ),
    }
}

/// Log child worktree mutations, then remove every worktree.
///
/// Blocking on purpose: fan-in teardown must complete atomically with respect
/// to cancellation, and a blocking body cannot be interrupted halfway through.
fn teardown_worktrees_blocking(group: &Arc<ParallelGroupState>, project_root: &Path) {
    let paths = group.paths();
    for (child_key, wt) in &paths {
        if !wt.is_dir() {
            continue;
        }
        let child_head = head_sha_blocking(wt);
        let dirty = status_porcelain_blocking(wt);
        if !child_head.is_empty() && child_head != group.base_head() {
            log::warn!(
                target: "gremlins.stages.parallel",
                "parallel child {child_key} mutated its worktree (HEAD={child_head}, base={}) \
                 — mutations will be discarded on teardown (fan-in merge not yet implemented)",
                group.base_head()
            );
        }
        if !dirty.trim().is_empty() {
            log::warn!(
                target: "gremlins.stages.parallel",
                "parallel child {child_key} has uncommitted changes — \
                 changes will be discarded on teardown (fan-in merge not yet implemented)"
            );
        }
    }
    let all: Vec<String> = paths
        .values()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    remove_worktrees_blocking(project_root, &all);
    Python::attach(|py| group.clear(py));
}

// --- blocking git helpers (for the cancellation-safe teardown path) ---

/// Run `git <args>` in `cwd`, returning trimmed stdout, or `None` on failure.
fn git_stdout_blocking(args: &[&str], cwd: &Path) -> Option<String> {
    let cmd: Vec<String> = std::iter::once("git".to_string())
        .chain(args.iter().map(|a| a.to_string()))
        .collect();
    gremlins::core::proc::run_or_raise(&cmd, Some(cwd)).ok()
}

fn in_git_repo_blocking(cwd: &Path) -> bool {
    let cmd = ["git", "rev-parse", "--git-dir"].map(String::from).to_vec();
    gremlins::core::proc::run_ok(&cmd, Some(cwd)).unwrap_or(false)
}

fn head_sha_blocking(cwd: &Path) -> String {
    git_stdout_blocking(&["rev-parse", "HEAD"], cwd).unwrap_or_default()
}

fn status_porcelain_blocking(cwd: &Path) -> String {
    git_stdout_blocking(&["status", "--porcelain"], cwd).unwrap_or_default()
}

/// Remove worktrees in bulk and prune stale entries. No-op outside a repo.
fn remove_worktrees_blocking(project_root: &Path, paths: &[String]) {
    if !in_git_repo_blocking(project_root) {
        return;
    }
    for wt in paths {
        let cmd = ["git", "worktree", "remove", "--force", wt.as_str()]
            .map(String::from)
            .to_vec();
        let _ = gremlins::core::proc::run_quiet(&cmd, Some(project_root));
    }
    let prune = ["git", "worktree", "prune"].map(String::from).to_vec();
    let _ = gremlins::core::proc::run_quiet(&prune, Some(project_root));
}

// --- git helpers (thin wrappers over the Rust proc layer) ---

/// Await a Python coroutine, bridging it onto the Tokio runtime.
async fn await_py(coro: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let fut = Python::attach(|py| pyo3_async_runtimes::tokio::into_future(coro.bind(py).clone()))?;
    fut.await
}

/// Call an async function in `gremlins.utils.git` and await it.
async fn call_git_async(
    func: &str,
    args: Vec<Py<PyAny>>,
    kwargs: Vec<(&str, Py<PyAny>)>,
) -> PyResult<Py<PyAny>> {
    let coro = Python::attach(|py| {
        let git = py.import("gremlins.utils.git")?;
        let py_kwargs = PyDict::new(py);
        for (k, v) in kwargs {
            py_kwargs.set_item(k, v)?;
        }
        git.getattr(func)?
            .call(PyTuple::new(py, args)?, Some(&py_kwargs))
            .map(|c| c.unbind())
    })?;
    await_py(coro).await
}

fn py_str(py: Python<'_>, s: &str) -> Py<PyAny> {
    PyString::new(py, s).into_any().unbind()
}

async fn in_git_repo(cwd: &Path) -> bool {
    let cwd = cwd.to_string_lossy().to_string();
    let result = Python::attach(|py| {
        call_git_async("in_git_repo_async", vec![], vec![("cwd", py_str(py, &cwd))])
    });
    match result.await {
        Ok(v) => Python::attach(|py| v.bind(py).extract::<bool>().unwrap_or(false)),
        Err(_) => false,
    }
}

async fn head_sha(cwd: &str) -> String {
    let result = Python::attach(|py| {
        call_git_async("head_sha_async", vec![], vec![("cwd", py_str(py, cwd))])
    });
    match result.await {
        Ok(v) => Python::attach(|py| v.bind(py).extract::<String>().unwrap_or_default()),
        Err(_) => String::new(),
    }
}

async fn prune_worktrees(project_root: &Path) {
    if !in_git_repo(project_root).await {
        return;
    }
    let root = project_root.to_string_lossy().to_string();
    let result = Python::attach(|py| {
        call_git_async("prune_worktrees_async", vec![py_str(py, &root)], vec![])
    });
    let _ = result.await;
}

async fn remove_worktrees(project_root: &Path, paths: &[String]) {
    if !in_git_repo(project_root).await {
        return;
    }
    let root = project_root.to_string_lossy().to_string();
    let result = Python::attach(|py| -> PyResult<_> {
        let paths = PyList::new(py, paths)?;
        Ok(call_git_async(
            "remove_worktrees_async",
            vec![py_str(py, &root), paths.into_any().unbind()],
            vec![],
        ))
    });
    if let Ok(fut) = result {
        let _ = fut.await;
    }
}

async fn setup_detached_worktree(
    project_root: &Path,
    base_ref: &str,
    worktree_parent: Option<&Path>,
) -> PyResult<String> {
    let root = project_root.to_string_lossy().to_string();
    let base = if base_ref.is_empty() {
        "HEAD"
    } else {
        base_ref
    };
    let parent = worktree_parent.map(|p| p.to_string_lossy().to_string());
    let result = Python::attach(|py| {
        let mut kwargs: Vec<(&str, Py<PyAny>)> = Vec::new();
        if let Some(parent) = &parent {
            kwargs.push(("worktree_parent", py_str(py, parent)));
        }
        call_git_async(
            "setup_detached_worktree_async",
            vec![py_str(py, &root), py_str(py, base)],
            kwargs,
        )
    });
    let value = result.await?;
    Python::attach(|py| value.bind(py).extract::<String>())
}

// --- Test seams for the parallel helpers ---

#[pyfunction]
#[pyo3(name = "_gather_child_artifacts")]
fn gather_child_artifacts_py(
    parent_state: &Bound<'_, PyAny>,
    child_keys: Vec<String>,
    group_name: &str,
) -> PyResult<()> {
    let parent_id: String = parent_state
        .getattr("data")?
        .getattr("gremlin_id")?
        .extract::<Option<String>>()?
        .unwrap_or_default();
    gather_child_artifacts(
        &child_keys,
        &parent_state.clone().unbind(),
        group_name,
        &parent_id,
    )
}

#[pyfunction]
#[pyo3(name = "_remove_child_dirs")]
fn remove_child_dirs_py(
    parent_state: &Bound<'_, PyAny>,
    child_keys: Vec<String>,
    group_name: &str,
) {
    let parent_id: String = parent_state
        .getattr("data")
        .and_then(|d| d.getattr("gremlin_id"))
        .and_then(|v| v.extract::<Option<String>>())
        .unwrap_or_default()
        .unwrap_or_default();
    remove_child_dirs(
        &child_keys,
        &parent_state.clone().unbind(),
        group_name,
        &parent_id,
    );
}

#[pyfunction]
#[pyo3(name = "_parse_child_timeout")]
fn parse_child_timeout_py(stage_obj: &Bound<'_, PyAny>, child_key: &str) -> PyResult<Option<f64>> {
    parse_child_timeout(stage_obj, child_key)
}

#[pyfunction]
#[pyo3(name = "_missing_result_detail")]
fn missing_result_detail_py(child_key: &str, returncode: Option<i32>) -> String {
    missing_result_detail(child_key, returncode)
}

#[pyfunction]
#[pyo3(name = "_build_child_spec_dict", signature = (stage_obj, child_st, child_key, attempt, group_name = "", child_id = ""))]
fn build_child_spec_dict_py(
    py: Python<'_>,
    stage_obj: &Bound<'_, PyAny>,
    child_st: &Bound<'_, PyAny>,
    child_key: &str,
    attempt: &str,
    group_name: &str,
    child_id: &str,
) -> PyResult<Py<PyAny>> {
    let spec = build_child_spec_dict(
        stage_obj, child_st, child_key, attempt, group_name, child_id,
    )?;
    json_value_to_py(py, &serde_json::Value::Object(spec))
}

// --- Module registration ---

pub fn register_stages_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "stages")?;
    let py = m.py();

    m.add_class::<PyExec>()?;
    m.add_class::<PyAgent>()?;
    m.add_class::<PyStageAttrs>()?;
    m.add_class::<PySequence>()?;
    m.add_class::<PyLoop>()?;
    m.add_class::<PyParallelStage>()?;
    m.add_class::<ParallelRuntime>()?;
    m.add_class::<Done>()?;
    m.add_function(wrap_pyfunction!(get_client_from_dict_py, &m)?)?;
    m.add_function(wrap_pyfunction!(child_state_py, &m)?)?;
    m.add_function(wrap_pyfunction!(exec_run_async, &m)?)?;
    m.add_function(wrap_pyfunction!(agent_run_async, &m)?)?;
    m.add_function(wrap_pyfunction!(sequence_run_async, &m)?)?;
    m.add_function(wrap_pyfunction!(loop_run_async, &m)?)?;
    m.add_function(wrap_pyfunction!(parallel_run_async, &m)?)?;
    m.add_function(wrap_pyfunction!(gather_child_artifacts_py, &m)?)?;
    m.add_function(wrap_pyfunction!(remove_child_dirs_py, &m)?)?;
    m.add_function(wrap_pyfunction!(parse_child_timeout_py, &m)?)?;
    m.add_function(wrap_pyfunction!(missing_result_detail_py, &m)?)?;
    m.add_function(wrap_pyfunction!(build_child_spec_dict_py, &m)?)?;
    m.add("Bail", m.py().get_type::<Bail>())?;

    parent.add_submodule(&m)?;
    let modules = py.import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.stages", &m)?;

    patch_bail(py, &m)?;

    m.add("Outcome", m.getattr("Done")?)?;
    m.add("_BAIL_KEY", BAIL_KEY)?;
    let keys: Vec<&str> = FRAMEWORK_KEYS.iter().copied().collect();
    m.add("FRAMEWORK_KEYS", PyFrozenSet::new(py, &keys)?)?;
    m.add_function(wrap_pyfunction!(substitute_vars_py, &m)?)?;
    m.add_function(wrap_pyfunction!(bail_reason_py, &m)?)?;
    m.add_function(wrap_pyfunction!(is_bail_set_py, &m)?)?;

    Ok(())
}
