use std::collections::HashMap;
use std::path::PathBuf;

use gremlins::stages::constants::{BAIL_KEY, FRAMEWORK_KEYS};
use gremlins::stages::exec::{self as rust_exec, substitute_vars};
use gremlins::stages::outcome::Done as RustDone;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyFrozenSet, PyType};

use crate::python::artifacts::ArtifactRegistry;

// --- Helpers ---

/// Convert a PyDict of string keys to a HashMap<String, serde_json::Value>
/// by serializing each value via Python's json module.
fn extract_json_value_dict(obj: &Bound<'_, PyAny>) -> PyResult<HashMap<String, serde_json::Value>> {
    // Import json module
    let json_mod = obj.py().import("json")?;
    let dict = obj.cast::<PyDict>()?;
    let mut map = HashMap::new();
    for (key, val) in dict.iter() {
        let k: String = key.extract()?;
        // Serialize Python value to JSON string, then parse back to serde_json::Value
        let json_str: String = json_mod.call_method1("dumps", (val,))?.extract()?;
        let v: serde_json::Value =
            serde_json::from_str(&json_str).unwrap_or(serde_json::Value::Null);
        map.insert(k, v);
    }
    Ok(map)
}

/// Try to convert a Python value to a serde_json::Value dict using json module
fn maybe_extract_json_value_dict(
    obj: &Bound<'_, PyAny>,
) -> Option<HashMap<String, serde_json::Value>> {
    if obj.cast::<PyDict>().is_err() {
        return None;
    }
    extract_json_value_dict(obj).ok()
}

/// Convert a serde_json::Value to a Python object using json module
fn json_value_to_py(py: Python<'_>, v: &serde_json::Value) -> PyResult<Py<PyAny>> {
    let json_mod = py.import("json")?;
    let json_str = serde_json::to_string(v).map_err(|e| {
        pyo3::exceptions::PyValueError::new_err(format!("JSON serialization error: {e}"))
    })?;
    let py_obj = json_mod.call_method1("loads", (json_str,))?;
    Ok(py_obj.unbind())
}

/// Extract a ProcResult from a Python subprocess.CompletedProcess.
fn extract_proc_result(
    _py: Python<'_>,
    obj: &Bound<'_, PyAny>,
) -> PyResult<gremlins::core::proc::ProcResult> {
    let rc: i32 = obj.getattr("returncode")?.extract()?;
    let stdout: String = obj.getattr("stdout")?.extract()?;
    let stderr: String = obj.getattr("stderr")?.extract()?;
    Ok(gremlins::core::proc::ProcResult {
        returncode: rc,
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
    })
}

// --- Done pyclass ---

#[pyclass(name = "Done", module = "_gremlins_core.stages", skip_from_py_object)]
#[derive(Clone)]
struct Done(RustDone);

#[pymethods]
impl Done {
    #[new]
    fn new() -> Self {
        Done(RustDone)
    }

    fn __repr__(&self) -> &'static str {
        "Done()"
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

fn patch_bail(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    let globals = pyo3::types::PyDict::new(py);
    globals.set_item("_m", m)?;
    py.run(
        c"\
def _reason(self):
    return self.args[0] if self.args else ''
def _str(self):
    return self.args[0] if self.args else ''
_m.Bail.reason = property(_reason)
_m.Bail.__str__ = _str
",
        Some(&globals),
        None,
    )
}

// --- Exec pyclass ---

#[pyclass(name = "Exec", module = "_gremlins_core.stages", skip_from_py_object)]
struct PyExec {
    inner: rust_exec::Exec,
    raw_dict: Option<Py<PyAny>>,
    gremlin: Option<Py<PyAny>>,
    client: Option<Py<PyAny>>,
    client_explicit: bool,
    skip_if_exists: String,
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

        let options: HashMap<String, serde_json::Value> = d
            .get_item("options")
            .ok()
            .flatten()
            .and_then(|v| maybe_extract_json_value_dict(&v))
            .unwrap_or_default();

        for k in options.keys() {
            if FRAMEWORK_KEYS.contains(k.as_str()) {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: option key {k:?} collides with framework substitution variable"
                )));
            }
        }

        Ok(PyExec {
            inner: rust_exec::Exec {
                name,
                options,
                interpolation_map: raw_interpolation,
                bind_map: raw_bind,
            },
            raw_dict: None,
            gremlin: None,
            client: None,
            client_explicit: false,
            skip_if_exists: String::new(),
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
    fn options<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (k, v) in &self.inner.options {
            let py_val = json_value_to_py(py, v)?;
            dict.set_item(k.as_str(), py_val)?;
        }
        Ok(dict)
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

    #[pyo3(signature = (text, state, extra = None))]
    fn substitute_vars(
        slf: PyRef<'_, Self>,
        text: &str,
        state: &Bound<'_, PyAny>,
        extra: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<String> {
        let str_opts = string_options(&slf.inner.options);
        let extra_map: HashMap<String, String> = extra
            .map(|d| d.extract().unwrap_or_default())
            .unwrap_or_default();
        let fw: HashMap<String, String> =
            state.call_method1("framework_subs", (slf,))?.extract()?;
        Ok(substitute_vars(text, &str_opts, &extra_map, &fw))
    }

    fn run<'py>(
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
        let artifacts_py: Py<PyAny> = state_obj.getattr("artifacts")?.unbind();
        let loop_iter_str: String = state_obj.getattr("loop_iter")?.extract()?;
        let cwd: PathBuf = state_obj.getattr("cwd")?.extract()?;
        let artifact_dir: PathBuf = state_obj.getattr("artifact_dir")?.extract()?;
        let state_dir: PathBuf = gremlin.getattr("state_dir")?.extract()?;
        let fw: HashMap<String, String> = state_obj
            .call_method1("framework_subs", (&slf,))?
            .extract()?;

        let name = exec.name.clone();

        // Phase 1: prepare (needs &mut ArtifactRegistry)
        let arts = artifacts_py.bind(py);
        let arts_inner = arts.extract::<PyRef<'_, ArtifactRegistry>>()?;
        let mut inner = arts_inner.inner.lock().unwrap();
        let mut prepared = match rust_exec::prepare_exec(&exec, &mut inner, &loop_iter_str, &fw) {
            Ok(p) => p,
            Err(rust_exec::ExecError::Resolve {
                source: gremlins::artifacts::resolve::ResolveError::MissingArtifact(key),
                ..
            }) => {
                let exc_type = py
                    .import("_gremlins_core.artifacts")?
                    .getattr("MissingArtifact")?;
                let args = (key.clone(),);
                let exc = exc_type.call1(args)?;
                return Err(PyErr::from_value(exc));
            }
            Err(e) => {
                return Err(pyo3::exceptions::PyValueError::new_err(e.to_string()));
            }
        };
        prepared.cwd = cwd;
        prepared.artifact_dir = artifact_dir;
        prepared.state_dir = state_dir;

        let is_empty_cmds = prepared.cmds.is_empty();

        let shell_result = if is_empty_cmds {
            rust_exec::ShellResult {
                output: String::new(),
                rc: 0,
                bail_triggered: false,
            }
        } else {
            // Call through Python's _gremlins_core.utils.proc.run_shell_async so
            // tests can monkeypatch it.
            let joined = prepared.cmds.join(" && ");
            let proc_mod = py.import("_gremlins_core.utils.proc")?;
            let py_cwd: Option<PathBuf> = Some(prepared.cwd.clone());
            let mut env = HashMap::new();
            env.insert(
                "GREMLINS_ARTIFACT_DIR".to_string(),
                prepared.artifact_dir.to_string_lossy().to_string(),
            );
            let py_env: Option<HashMap<String, String>> = Some(env);
            let py_timeout: Option<f64> = prepared.timeout;
            let py_coro = proc_mod.call_method(
                "run_shell_async",
                (joined, py_cwd, py_env, py_timeout),
                None,
            )?;
            // Use asyncio.run to execute the coroutine synchronously.
            // This creates a fresh event loop, avoiding "no running event loop"
            // errors when called outside a running Python event loop.
            let asyncio_mod = py.import("asyncio")?;
            let py_result = asyncio_mod.call_method1("run", (py_coro,))?;
            let proc_result = extract_proc_result(py, &py_result)?;

            rust_exec::process_shell_result(&prepared, proc_result)
                .map_err(|e| Bail::new_err(format!("exec {name}: {e}")))?
        };

        rust_exec::verify_exec(&prepared, &inner, &shell_result)
            .map_err(|e| Bail::new_err(format!("exec {name}: {e}")))?;

        let done_obj: Py<PyAny> = Py::new(py, Done(RustDone))?.into();
        let asyncio_mod = py.import("asyncio")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("result", done_obj)?;
        asyncio_mod.call_method("sleep", (0.0,), Some(&kwargs))
    }
}

fn string_options(options: &HashMap<String, serde_json::Value>) -> HashMap<String, String> {
    options
        .iter()
        .filter_map(|(k, v)| {
            if let serde_json::Value::String(s) = v {
                Some((k.clone(), s.clone()))
            } else {
                None
            }
        })
        .collect()
}

// --- Free functions ---

#[pyfunction]
#[pyo3(signature = (uri_str, loop_iter = ""))]
fn _is_bail_uri(uri_str: &str, loop_iter: &str) -> bool {
    rust_exec::is_bail_uri(uri_str, loop_iter)
}

// --- Module registration ---

pub fn register_stages_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "stages")?;
    let py = m.py();

    m.add_class::<PyExec>()?;
    m.add_class::<Done>()?;
    m.add("Bail", m.py().get_type::<Bail>())?;

    parent.add_submodule(&m)?;
    let modules = py.import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.stages", &m)?;

    patch_bail(py, &m)?;

    m.add("Outcome", m.getattr("Done")?)?;
    m.add("_BAIL_KEY", BAIL_KEY)?;
    let keys: Vec<&str> = FRAMEWORK_KEYS.iter().copied().collect();
    m.add("FRAMEWORK_KEYS", PyFrozenSet::new(py, &keys)?)?;
    m.add_function(wrap_pyfunction!(_is_bail_uri, &m)?)?;

    Ok(())
}
