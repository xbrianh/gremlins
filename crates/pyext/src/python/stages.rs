use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use gremlins::stages::agent as rust_agent;
use gremlins::stages::base;
use gremlins::stages::composite::{
    compute_child_params, get_client_from_dict as rust_get_client_from_dict,
    StageAttrs as RustStageAttrs,
};
use gremlins::stages::constants::{BAIL_KEY, FRAMEWORK_KEYS};
use gremlins::stages::exec as rust_exec;
use gremlins::stages::outcome::Done as RustDone;
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict, PyFrozenSet, PyList, PyString, PyTuple, PyType};

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

// --- Module registration ---

pub fn register_stages_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "stages")?;
    let py = m.py();

    m.add_class::<PyExec>()?;
    m.add_class::<PyAgent>()?;
    m.add_class::<PyStageAttrs>()?;
    m.add_class::<PySequence>()?;
    m.add_class::<PyLoop>()?;
    m.add_class::<Done>()?;
    m.add_function(wrap_pyfunction!(get_client_from_dict_py, &m)?)?;
    m.add_function(wrap_pyfunction!(child_state_py, &m)?)?;
    m.add_function(wrap_pyfunction!(exec_run_async, &m)?)?;
    m.add_function(wrap_pyfunction!(agent_run_async, &m)?)?;
    m.add_function(wrap_pyfunction!(sequence_run_async, &m)?)?;
    m.add_function(wrap_pyfunction!(loop_run_async, &m)?)?;
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
