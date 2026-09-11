use std::path::PathBuf;

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyList, PyType};

use crate::convert::pyval_to_serde;
use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::into_pyerr;
use crate::schemas::loader;

#[pyclass(name = "Pipeline")]
pub struct Pipeline {
    #[pyo3(get, set)]
    pub name: String,
    #[pyo3(get, set)]
    pub path: PathBuf,
    #[pyo3(get, set)]
    pub stages: Vec<Py<PyAny>>,
    #[pyo3(get, set)]
    pub default_client: Option<Py<PyAny>>,
    #[pyo3(get, set)]
    pub base_ref: String,
    #[pyo3(get, set)]
    pub bootstrap: Option<Py<PyAny>>,
    #[pyo3(get, set)]
    pub land: Option<Py<PyAny>>,
}

#[pymethods]
impl Pipeline {
    #[new]
    #[pyo3(signature = (name, path, stages, default_client=None, base_ref="current".to_string(), bootstrap=None, land=None))]
    fn new(
        name: String,
        path: PathBuf,
        stages: Vec<Py<PyAny>>,
        default_client: Option<Py<PyAny>>,
        base_ref: String,
        bootstrap: Option<Py<PyAny>>,
        land: Option<Py<PyAny>>,
    ) -> Self {
        Pipeline {
            name,
            path,
            stages,
            default_client,
            base_ref,
            bootstrap,
            land,
        }
    }

    #[classmethod]
    #[pyo3(signature = (path, *, default_client_override=None))]
    fn from_yaml(
        _cls: &Bound<'_, PyType>,
        path: PathBuf,
        default_client_override: Option<String>,
    ) -> PyResult<Self> {
        let py = _cls.py();

        let path = path.canonicalize().unwrap_or(path);
        if !path.exists() {
            return Err(pyo3::exceptions::PyFileNotFoundError::new_err(format!(
                "pipeline file not found: {}",
                path.display()
            )));
        }

        py.import("gremlins._clients_init")?;

        // Determine project root: walk up parent chain to find .gremlins directory
        let project_root = {
            let mut p = path.parent();
            let mut root = None;
            while let Some(parent) = p {
                if parent.file_name().is_some_and(|n| n == ".gremlins") {
                    root = parent.parent().map(PathBuf::from);
                    break;
                }
                p = parent.parent();
            }
            root.unwrap_or_else(|| {
                path.parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
            })
        };

        // Use Rust parse_pipeline_file directly — no Python callback
        let raw = gremlins::schemas::expand::parse_pipeline_file(&path, &project_root)
            .map_err(into_pyerr)?;

        let raw_dict: Bound<'_, PyDict> = serde_yaml_value_to_py_dict(py, &raw)?;
        let pipeline_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let default_client: Option<Py<PyAny>> = raw_dict
            .get_item("default_client")?
            .filter(|v: &Bound<'_, PyAny>| !v.is_none())
            .map(|v: Bound<'_, PyAny>| {
                let s: String = v.extract()?;
                if s.is_empty() {
                    return Err(pyo3::exceptions::PyValueError::new_err(
                        "default_client must be a non-empty string",
                    ));
                }
                let client_cls = py.import("_gremlins_core.clients")?.getattr("RustClient")?;
                let client: Py<PyAny> = client_cls.call_method1("parse", (s,))?.extract()?;
                Ok(client)
            })
            .transpose()?;

        let base_ref: String = raw_dict
            .get_item("base_ref")?
            .map(|v: Bound<'_, PyAny>| {
                let s: String = v.extract()?;
                if s.trim().is_empty() {
                    Err(pyo3::exceptions::PyValueError::new_err(
                        "base_ref must be a non-empty string",
                    ))
                } else {
                    Ok(s.trim().to_string())
                }
            })
            .transpose()?
            .unwrap_or_else(|| "current".to_string());

        let raw_stages_val = raw_dict.get_item("stages")?;
        let empty_list = PyList::empty(py);
        let raw_stages: &Bound<'_, PyList> = match raw_stages_val.as_ref() {
            None => &empty_list,
            Some(ob) if ob.is_none() => &empty_list,
            Some(ob) => ob.cast::<PyList>().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "'stages' must be a list; got {} type",
                    ob.get_type()
                        .name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "?".to_string())
                ))
            })?,
        };

        let stages = loader::parse_stages(py, raw_stages, 0)?;

        // Reject the old "inputs" key
        if raw_dict.get_item("inputs")?.is_some() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "'inputs' is not a valid pipeline key; declare CLI arguments under bootstrap.source"
            ));
        }

        let bootstrap: Option<Py<PyAny>> = {
            let raw_bs = raw_dict.get_item("bootstrap")?;
            let rust_bs = match raw_bs {
                None => gremlins::schemas::bootstrap::Bootstrap::default(),
                Some(v) if v.is_none() => gremlins::schemas::bootstrap::Bootstrap::default(),
                Some(v) => {
                    let bootstrap_dict: &Bound<'_, PyDict> = v.cast().map_err(|_| {
                        pyo3::exceptions::PyValueError::new_err("'bootstrap' must be a mapping")
                    })?;
                    let mut mapping = serde_yaml::Mapping::new();
                    for (k, v) in bootstrap_dict.iter() {
                        let k_str: String = k.extract()?;
                        mapping.insert(serde_yaml::Value::String(k_str), pyval_to_serde(&v)?);
                    }
                    gremlins::schemas::bootstrap::Bootstrap::from_yaml(Some(
                        &serde_yaml::Value::Mapping(mapping),
                    ))
                    .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?
                }
            };
            let bs = Bootstrap { inner: rust_bs };
            let bs_obj = Py::new(py, bs)?;
            Some(bs_obj.into_any())
        };

        let land_stage: Option<Py<PyAny>> = raw_dict
            .get_item("land")?
            .map(|v: Bound<'_, PyAny>| {
                let land_dict: &Bound<'_, PyDict> = v.cast().map_err(|_| {
                    pyo3::exceptions::PyValueError::new_err("'land' must be a mapping")
                })?;
                let exec_cls = py.import("_gremlins_core.stages")?.getattr("Exec")?;
                let land_stage_dict = PyDict::new(py);
                land_stage_dict.set_item("name", "land")?;
                for (k, v) in land_dict.iter() {
                    land_stage_dict.set_item(k, v)?;
                }
                let stage: Py<PyAny> = exec_cls
                    .call_method1("with_dict", (land_stage_dict,))?
                    .extract()?;
                Ok::<Py<PyAny>, pyo3::PyErr>(stage)
            })
            .transpose()?;

        let stages_list = PyList::new(py, stages.iter().map(|s| s.bind(py)))?;
        let extra_out: Option<Bound<'_, PyDict>> = bootstrap.as_ref().and_then(|bs| {
            bs.getattr(py, "cli_out")
                .ok()
                .and_then(|v| v.extract::<Bound<'_, PyDict>>(py).ok())
        });
        loader::check_duplicate_producers(&stages_list, extra_out.as_ref())?;

        // Validate that artifact consumers have a producer
        let launch_cmds: Vec<String> = bootstrap
            .as_ref()
            .and_then(|bs| {
                bs.getattr(py, "launch_cmds")
                    .ok()
                    .and_then(|v| v.extract::<Vec<String>>(py).ok())
            })
            .unwrap_or_default();
        let launch_cmds_list = PyList::new(py, &launch_cmds)?;
        loader::check_unresolved_consumers(&stages_list, &launch_cmds_list, extra_out.as_ref())?;

        // Resolve default_client: YAML field > CLI override > global config
        let default_client = match (default_client, default_client_override) {
            (Some(dc), _) => Some(dc),
            (None, Some(override_str)) => {
                let client_cls = py.import("_gremlins_core.clients")?.getattr("RustClient")?;
                Some(
                    client_cls
                        .call_method1("parse", (override_str,))?
                        .extract()?,
                )
            }
            (None, None) => {
                let cfg_default = gremlins::config::global_config()
                    .ok()
                    .and_then(|cfg| cfg.default_client().map(String::from));
                match cfg_default {
                    Some(client_str) => {
                        let client_cls =
                            py.import("_gremlins_core.clients")?.getattr("RustClient")?;
                        let client: Py<PyAny> =
                            client_cls.call_method1("parse", (client_str,))?.extract()?;
                        Some(client)
                    }
                    None => None,
                }
            }
        };

        if default_client.is_none() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "pipeline is missing 'default_client' — set a 'default_client' in the pipeline YAML, pass --client on the command line, or set 'default-client' in config.json",
            ));
        }

        if let Some(ref dc) = default_client {
            fill_stage_clients_inner(py, &stages, dc)?;
        }

        Ok(Pipeline {
            name: pipeline_name,
            path,
            stages,
            default_client,
            base_ref,
            bootstrap,
            land: land_stage,
        })
    }

    fn uses_loop_handoff(&self, py: Python<'_>) -> bool {
        if self.stages.is_empty() {
            return false;
        }
        let first = &self.stages[0];
        let stage_type: Option<String> = first
            .getattr(py, "type")
            .ok()
            .and_then(|v| v.extract(py).ok());
        if stage_type.as_deref() != Some("loop") {
            return false;
        }
        let body: Option<Vec<Py<PyAny>>> = first
            .getattr(py, "body")
            .ok()
            .and_then(|v| v.extract(py).ok());
        match body {
            Some(children) => children.iter().any(|c| {
                c.getattr(py, "name")
                    .ok()
                    .and_then(|n| n.extract::<String>(py).ok())
                    .is_some_and(|n| n == "handoff")
            }),
            None => false,
        }
    }
}

fn fill_stage_clients_inner(
    py: Python<'_>,
    stages: &[Py<PyAny>],
    default: &Py<PyAny>,
) -> PyResult<()> {
    for stage in stages {
        let stage_type: Option<String> = stage
            .getattr(py, "type")
            .ok()
            .and_then(|v| v.extract(py).ok());
        if stage_type.as_deref() == Some("parallel") {
            let body: Option<Vec<Py<PyAny>>> = stage
                .getattr(py, "body")
                .ok()
                .and_then(|v| v.extract(py).ok());
            if let Some(body) = body {
                fill_stage_clients_inner(py, &body, default)?;
            }
            // Fall through — parallel stage itself needs client set
        }
        let client: Option<Py<PyAny>> = stage.getattr(py, "client")?.extract(py)?;
        if client.is_none() {
            stage.setattr(py, "client", default)?;
        }
        let body: Option<Vec<Py<PyAny>>> = stage
            .getattr(py, "body")
            .ok()
            .and_then(|v| v.extract(py).ok());
        if let Some(body) = body {
            fill_stage_clients_inner(py, &body, default)?;
        }
    }
    Ok(())
}

/// Public pyfunction wrapper for fill_stage_clients (used by tests).
#[pyfunction]
#[pyo3(signature = (stages, default))]
pub fn fill_stage_clients(stages: &Bound<'_, PyList>, default: &Bound<'_, PyAny>) -> PyResult<()> {
    let py = stages.py();
    let stages_vec: Vec<Py<PyAny>> = stages.iter().map(|s| s.unbind()).collect();
    fill_stage_clients_inner(py, &stages_vec, &default.clone().unbind())
}

/// Convert a serde_yaml::Value mapping to a PyDict.
fn serde_yaml_value_to_py_dict<'a>(
    py: Python<'a>,
    value: &'a serde_yaml::Value,
) -> PyResult<Bound<'a, PyDict>> {
    let dict = PyDict::new(py);
    match value {
        serde_yaml::Value::Mapping(m) => {
            for (k, v) in m {
                let key_str = k
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| format!("{k:?}"));
                let val = serde_yaml_to_py(py, v)?;
                dict.set_item(key_str, val)?;
            }
        }
        _ => {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "expected a YAML mapping",
            ));
        }
    }
    Ok(dict)
}

fn serde_yaml_to_py(py: Python<'_>, value: &serde_yaml::Value) -> PyResult<Py<PyAny>> {
    match value {
        serde_yaml::Value::Null => Ok(py.None()),
        serde_yaml::Value::Bool(b) => Ok(PyBool::new(py, *b).to_owned().into_any().unbind()),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_pyobject(py)?.into_any().unbind())
            } else if let Some(f) = n.as_f64() {
                Ok(f.into_pyobject(py)?.into_any().unbind())
            } else {
                Ok(py.None())
            }
        }
        serde_yaml::Value::String(s) => Ok(s.into_pyobject(py)?.into_any().unbind()),
        serde_yaml::Value::Sequence(seq) => {
            let list = PyList::empty(py);
            for item in seq {
                list.append(serde_yaml_to_py(py, item)?)?;
            }
            Ok(list.into())
        }
        serde_yaml::Value::Mapping(m) => {
            let dict = PyDict::new(py);
            for (k, v) in m {
                let key_str = k
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| format!("{k:?}"));
                dict.set_item(key_str, serde_yaml_to_py(py, v)?)?;
            }
            Ok(dict.into())
        }
        serde_yaml::Value::Tagged(t) => serde_yaml_to_py(py, &t.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uses_loop_handoff_empty() {
        let p = Pipeline {
            name: "test".to_string(),
            path: PathBuf::from("."),
            stages: vec![],
            default_client: None,
            base_ref: "current".to_string(),
            bootstrap: None,
            land: None,
        };
        Python::attach(|py| {
            assert!(!p.uses_loop_handoff(py));
        });
    }
}
