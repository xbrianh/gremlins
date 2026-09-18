use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use gremlins::core::proc;
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
        let coro = stage.bind(py).call_method1("_run_impl", (gremlin,))?;
        Ok(coro.unbind())
    }

    fn _run_impl<'py>(
        slf: PyRef<'_, Self>,
        py: Python<'py>,
        gremlin: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let exec = slf.inner.clone();

        let state_obj = gremlin.getattr("state")?;
        if state_obj.is_none() {
            return Err(PyRuntimeError::new_err(
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
        let mut prepared = {
            let arts_ref = artifacts.bind(py);
            let arts_inner: PyRef<'_, ArtifactRegistry> = arts_ref.extract()?;
            let inner = &arts_inner.inner;
            match rust_exec::prepare_exec(&exec, inner, &loop_iter_str, &fw) {
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
            let inner = &arts_inner.inner;
            rust_exec::commit_exec(&prepared, inner).map_err(|e| Bail::new_err(e.to_string()))?;

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

                let raw_result = proc::ProcResult {
                    returncode: rc,
                    stdout: stdout.into_bytes(),
                    stderr: stderr.into_bytes(),
                };
                rust_exec::process_shell_result(&prepared, raw_result)
                    .map_err(|e| Bail::new_err(e.to_string()))?
            } else {
                // Production path: call Rust directly.
                rust_exec::run_shell(&prepared)
                    .await
                    .map_err(|e| Bail::new_err(e.to_string()))?
            };

            Python::attach(|py| {
                let arts_ref = artifacts.bind(py);
                let arts_inner: PyRef<'_, ArtifactRegistry> = arts_ref.extract()?;
                let inner = &arts_inner.inner;
                rust_exec::commit_exec(&prepared, inner)
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

    fn run(&self, _gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        Err(pyo3::exceptions::PyNotImplementedError::new_err(
            "Agent.run() is not available — use the native Gremlin executor",
        ))
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

    fn run(&self, _gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        Err(pyo3::exceptions::PyNotImplementedError::new_err(
            "Sequence.run() is not available — use the native Gremlin executor",
        ))
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

    fn run(&self, _gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        Err(pyo3::exceptions::PyNotImplementedError::new_err(
            "Loop.run() is not available — use the native Gremlin executor",
        ))
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
/// The heavy lifting is not yet implemented in Rust; this class owns the
/// parsed configuration and the Python-visible surface (`with_dict`, `run`,
/// and the `max_concurrent` / `cancel_on_error` / `error_policy` properties).
#[pyclass(name = "ParallelStage", module = "_gremlins_core.stages", extends = PyStageAttrs, subclass, skip_from_py_object)]
struct PyParallelStage {
    max_concurrent: Option<u32>,
    cancel_on_error: bool,
    error_policy: String,
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
    #[pyo3(signature = (name, body = None, *, max_concurrent = None, cancel_on_error = false, error_policy = "any".to_string()))]
    fn new(
        py: Python<'_>,
        name: String,
        body: Option<&Bound<'_, PyList>>,
        max_concurrent: Option<u32>,
        cancel_on_error: bool,
        error_policy: String,
    ) -> PyResult<PyClassInitializer<Self>> {
        if max_concurrent == Some(0) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "parallel group {name:?}: 'max_concurrent' must be a positive integer"
            )));
        }
        if error_policy != "any" && error_policy != "all" {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "parallel group {name:?}: 'error_policy' must be 'any' or 'all'"
            )));
        }
        let body = body.cloned().unwrap_or_else(|| PyList::empty(py));
        let base = Self::base_attrs(py, &name, &body)?;
        Ok(
            PyClassInitializer::from(base).add_subclass(PyParallelStage {
                max_concurrent,
                cancel_on_error,
                error_policy,
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
        kwargs.set_item("cancel_on_error", group.cancel_on_error)?;
        kwargs.set_item("error_policy", group.error_policy.as_str())?;
        let obj = cls.call((group.attrs.name.as_str(),), Some(&kwargs))?;

        let client = match &group.client {
            Some(spec) => Some(Py::new(py, Client::parse(&spec.0)?)?.into_any()),
            None => None,
        };
        obj.setattr("client", client)?;
        obj.setattr("client_explicit", group.attrs.client_explicit)?;
        Ok(obj.unbind())
    }

    fn run(&self, _gremlin: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        Err(pyo3::exceptions::PyNotImplementedError::new_err(
            "ParallelStage.run() is not available — use the native Gremlin executor",
        ))
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
    fn cancel_on_error(&self) -> bool {
        self.cancel_on_error
    }

    #[setter]
    fn set_cancel_on_error(&mut self, value: bool) {
        self.cancel_on_error = value;
    }

    #[getter]
    fn error_policy(&self) -> String {
        self.error_policy.clone()
    }

    /// Mirror the constructor's invariant. `config()` maps anything other than
    /// `"all"` onto `ErrorPolicy::Any`, so a typo here would silently widen the
    /// policy instead of failing.
    #[setter]
    fn set_error_policy(mut slf: PyRefMut<'_, Self>, value: String) -> PyResult<()> {
        if value != "any" && value != "all" {
            let name = slf.as_super().inner.name.clone();
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "parallel group {name:?}: 'error_policy' must be 'any' or 'all'"
            )));
        }
        slf.error_policy = value;
        Ok(())
    }
}

// --- Free functions ---

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
    let (bootstrap_cmds, bootstrap_env): (Vec<String>, String) = {
        let pipeline_data = child_st.getattr("pipeline_data")?;
        if pipeline_data.is_none() {
            (Vec::new(), String::new())
        } else {
            let bootstrap = pipeline_data.getattr("bootstrap")?;
            if bootstrap.is_none() {
                (Vec::new(), String::new())
            } else {
                (
                    bootstrap.getattr("cmds")?.extract::<Vec<String>>()?,
                    bootstrap.getattr("env")?.extract::<String>()?,
                )
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
            bootstrap_cmds
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    if !bootstrap_env.is_empty() {
        map.insert(
            "bootstrap_env".into(),
            serde_json::Value::String(bootstrap_env),
        );
    }
    Ok(map)
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
    m.add_class::<Done>()?;
    m.add_function(wrap_pyfunction!(get_client_from_dict_py, &m)?)?;
    m.add_function(wrap_pyfunction!(child_state_py, &m)?)?;
    m.add_function(wrap_pyfunction!(gather_child_artifacts_py, &m)?)?;
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
