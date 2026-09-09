use std::collections::HashMap;
use std::path::PathBuf;

use gremlins::artifacts::resolve::resolve_interpolation_map;
use gremlins::artifacts::uri::Uri;
use gremlins::core::proc::run_shell_async;
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
#[derive(Clone)]
struct PyExec {
    inner: rust_exec::Exec,
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
    fn skip_if_exists(&self) -> &str {
        ""
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
        let loop_iter: String = state_obj.call_method0("loop_iter")?.extract()?;
        let cwd: PathBuf = state_obj.getattr("cwd")?.extract()?;
        let artifact_dir: PathBuf = state_obj.getattr("artifact_dir")?.extract()?;
        let state_dir: PathBuf = gremlin.getattr("state_dir")?.extract()?;
        let fw: HashMap<String, String> = state_obj
            .call_method1("framework_subs", (&slf,))?
            .extract()?;

        let name = exec.name.clone();
        let str_opts = string_options(&exec.options);

        // Resolve interpolation vars (needs GIL for registry)
        let interpolation_map = {
            let arts = artifacts_py.bind(py);
            let arts_inner = arts.extract::<PyRef<'_, ArtifactRegistry>>()?;
            let inner = arts_inner.inner.lock().unwrap();
            resolve_interpolation_map(&inner, &exec.interpolation_map, &loop_iter)
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("exec {name}: {e}")))?
        };

        // Register bind URIs (needs GIL for registry)
        let mut bind_paths: HashMap<String, String> = HashMap::new();
        {
            let arts = artifacts_py.bind(py);
            let arts_inner = arts.extract::<PyRef<'_, ArtifactRegistry>>()?;
            let mut inner = arts_inner.inner.lock().unwrap();
            for (raw_key, raw_uri_str) in &exec.bind_map {
                let key = substitute_vars(raw_key, &str_opts, &interpolation_map, &fw);
                let key = key.trim_end_matches('?').to_string();
                let mut uri_str = substitute_vars(raw_uri_str, &str_opts, &interpolation_map, &fw);
                if !loop_iter.is_empty() {
                    uri_str = uri_str.replace("{loop_iter}", &loop_iter);
                }
                let uri = Uri::parse(&uri_str).map_err(|e| {
                    pyo3::exceptions::PyValueError::new_err(format!(
                        "exec {name}: invalid URI: {e}"
                    ))
                })?;
                let path = inner.register(&uri, true).map_err(|e| {
                    pyo3::exceptions::PyValueError::new_err(format!("exec {name}: {e}"))
                })?;
                bind_paths.insert(key, path);
            }
        }

        // Build substitution map
        let subst_vars: HashMap<String, String> = interpolation_map
            .iter()
            .chain(bind_paths.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Read cmds and substitute
        let raw_cmds: Vec<String> = exec
            .options
            .get("cmds")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let cmds: Vec<String> = raw_cmds
            .iter()
            .map(|c| substitute_vars(c, &str_opts, &subst_vars, &fw))
            .collect();

        let timeout: Option<f64> = exec.options.get("timeout").and_then(|v| v.as_f64());

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let (_shell_output, _shell_rc, bail_triggered) = if cmds.is_empty() {
                (String::new(), 0, false)
            } else {
                let joined = cmds.join(" && ");
                let mut env = HashMap::new();
                env.insert(
                    "GREMLINS_ARTIFACT_DIR".to_string(),
                    artifact_dir.to_string_lossy().to_string(),
                );

                let result = run_shell_async(&joined, Some(&cwd), Some(&env), timeout)
                    .await
                    .map_err(|e| {
                        pyo3::exceptions::PyValueError::new_err(format!("exec {name}: {e}"))
                    })?;

                let raw_output = {
                    let mut buf = result.stdout.clone();
                    buf.extend_from_slice(&result.stderr);
                    buf
                };
                let raw_output_str = String::from_utf8_lossy(&raw_output).to_string();
                let shell_output = raw_output_str.trim().to_string();
                let shell_rc = result.returncode;

                let log_path = state_dir.join(format!("exec-{name}.log"));
                let log_content = if raw_output_str.is_empty() {
                    "(no output)\n".to_string()
                } else {
                    raw_output_str.clone()
                };
                let _ = std::fs::write(&log_path, &log_content);

                let bail_triggered = if shell_rc != 0 {
                    if exec
                        .bind_map
                        .values()
                        .any(|v| rust_exec::is_bail_uri(v, &loop_iter))
                    {
                        true
                    } else {
                        return Err(Bail::new_err(format!("exec {name}: exited {shell_rc}")));
                    }
                } else {
                    false
                };

                (shell_output, shell_rc, bail_triggered)
            };

            // Post-command verification: re-acquire GIL, return Py<PyAny> (Send)
            let result: PyResult<Py<PyAny>> = pyo3::Python::attach(|py| {
                let arts = artifacts_py.bind(py);
                let arts_inner = arts.extract::<PyRef<'_, ArtifactRegistry>>()?;
                let inner = arts_inner.inner.lock().unwrap();

                for (raw_key, raw_uri_str) in &exec.bind_map {
                    let key = substitute_vars(raw_key, &str_opts, &interpolation_map, &fw);
                    let optional = key.ends_with('?');
                    let _key = key.trim_end_matches('?').to_string();
                    let mut uri_str =
                        substitute_vars(raw_uri_str, &str_opts, &interpolation_map, &fw);
                    if !loop_iter.is_empty() {
                        uri_str = uri_str.replace("{loop_iter}", &loop_iter);
                    }
                    if rust_exec::is_bail_uri(&uri_str, &loop_iter) && !bail_triggered {
                        continue;
                    }
                    if !inner.exists(&uri_str) {
                        if optional {
                            continue;
                        }
                        return Err(Bail::new_err(format!(
                            "exec {name}: artifact {uri_str} was not produced"
                        )));
                    }
                }

                let done_obj: Py<PyAny> = Py::new(py, Done(RustDone))?.into();
                Ok(done_obj)
            });
            result
        })
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
