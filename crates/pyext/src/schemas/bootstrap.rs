use std::collections::HashMap;
use std::path::PathBuf;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyType};

use crate::convert::pyval_to_serde;
use gremlins::schemas::bootstrap as rust_bootstrap;

#[pyclass(name = "InputSource", from_py_object)]
#[derive(Clone)]
pub struct InputSource {
    pub(crate) inner: rust_bootstrap::InputSource,
}

#[pymethods]
impl InputSource {
    #[new]
    #[pyo3(signature = (name, types, optional = false))]
    fn new(name: String, types: Vec<String>, optional: bool) -> PyResult<Self> {
        rust_bootstrap::InputSource::new(name, types, optional)
            .map(|inner| InputSource { inner })
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }

    #[getter]
    fn get_name(&self) -> String {
        self.inner.name.clone()
    }

    #[setter]
    fn set_name(&mut self, name: String) {
        self.inner.name = name;
    }

    #[getter]
    fn get_types(&self) -> Vec<String> {
        self.inner.types.clone()
    }

    #[setter]
    fn set_types(&mut self, types: Vec<String>) {
        self.inner.types = types;
    }

    #[getter]
    fn get_optional(&self) -> bool {
        self.inner.optional
    }

    #[setter]
    fn set_optional(&mut self, optional: bool) {
        self.inner.optional = optional;
    }
}

#[pyclass(name = "InputSources", from_py_object)]
#[derive(Clone)]
pub struct InputSources {
    pub(crate) inner: rust_bootstrap::InputSources,
}

#[pymethods]
impl InputSources {
    #[new]
    fn new(sources: Option<HashMap<String, InputSource>>) -> Self {
        let inner = match sources {
            Some(s) => rust_bootstrap::InputSources::new(
                s.into_iter().map(|(k, v)| (k, v.inner)).collect(),
            ),
            None => rust_bootstrap::InputSources::new(HashMap::new()),
        };
        InputSources { inner }
    }

    #[classmethod]
    fn from_yaml(_cls: &Bound<'_, PyType>, raw: &Bound<'_, PyDict>) -> PyResult<Self> {
        let mut mapping = serde_yaml::Mapping::new();
        for (key, entry) in raw.iter() {
            let key_str: String = key.extract()?;
            let entry_dict: &Bound<'_, PyDict> = entry.cast().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "input source {key_str:?}: expected a mapping"
                ))
            })?;
            let mut entry_map = serde_yaml::Mapping::new();
            for (ek, ev) in entry_dict.iter() {
                let ek_str: String = ek.extract()?;
                if let Ok(s) = ev.extract::<String>() {
                    entry_map.insert(
                        serde_yaml::Value::String(ek_str),
                        serde_yaml::Value::String(s),
                    );
                } else if let Ok(b) = ev.extract::<bool>() {
                    entry_map.insert(
                        serde_yaml::Value::String(ek_str),
                        serde_yaml::Value::Bool(b),
                    );
                } else if let Ok(list) = ev.cast::<PyList>() {
                    let mut seq = Vec::new();
                    for item in list.iter() {
                        if let Ok(s) = item.extract::<String>() {
                            seq.push(serde_yaml::Value::String(s));
                        } else if let Ok(i) = item.extract::<i64>() {
                            seq.push(serde_yaml::Value::Number(serde_yaml::Number::from(i)));
                        } else if let Ok(b) = item.extract::<bool>() {
                            seq.push(serde_yaml::Value::Bool(b));
                        } else {
                            seq.push(serde_yaml::Value::String(item.to_string()));
                        }
                    }
                    entry_map.insert(
                        serde_yaml::Value::String(ek_str),
                        serde_yaml::Value::Sequence(seq),
                    );
                }
            }
            mapping.insert(
                serde_yaml::Value::String(key_str),
                serde_yaml::Value::Mapping(entry_map),
            );
        }
        let inner = rust_bootstrap::InputSources::from_yaml(&mapping)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        Ok(InputSources { inner })
    }

    fn get(&self, key: &str) -> Option<InputSource> {
        self.inner
            .get(key)
            .map(|s| InputSource { inner: s.clone() })
    }

    fn all_sources(&self) -> Vec<String> {
        self.inner.all_sources()
    }

    fn required_sources(&self) -> Vec<String> {
        self.inner.required_sources()
    }

    #[getter]
    fn get_sources(&self) -> HashMap<String, InputSource> {
        self.inner
            .sources
            .iter()
            .map(|(k, v)| (k.clone(), InputSource { inner: v.clone() }))
            .collect()
    }
}

#[pyclass(name = "Bootstrap", from_py_object)]
#[derive(Clone)]
pub struct Bootstrap {
    pub(crate) inner: rust_bootstrap::Bootstrap,
}

#[pymethods]
impl Bootstrap {
    #[new]
    #[pyo3(signature = (source=None, launch_cmds=None, cmds=None, cli_out=None, env=None))]
    fn new(
        source: Option<InputSources>,
        launch_cmds: Option<Vec<String>>,
        cmds: Option<Vec<String>>,
        cli_out: Option<HashMap<String, String>>,
        env: Option<String>,
    ) -> Self {
        Bootstrap {
            inner: rust_bootstrap::Bootstrap {
                source: source.map(|s| s.inner),
                launch_cmds: launch_cmds.unwrap_or_default(),
                cmds: cmds.unwrap_or_default(),
                cli_out: cli_out.unwrap_or_default(),
                env: env.unwrap_or_default(),
            },
        }
    }

    #[classmethod]
    fn from_yaml(_cls: &Bound<'_, PyType>, raw: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        let raw_value = match raw {
            None => None,
            Some(d) => {
                let mut mapping = serde_yaml::Mapping::new();
                for (k, v) in d.iter() {
                    let k_str: String = k.extract()?;
                    mapping.insert(serde_yaml::Value::String(k_str), pyval_to_serde(&v)?);
                }
                Some(serde_yaml::Value::Mapping(mapping))
            }
        };
        let inner = rust_bootstrap::Bootstrap::from_yaml(raw_value.as_ref())
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        Ok(Bootstrap { inner })
    }

    #[getter]
    fn get_source(&self) -> Option<InputSources> {
        self.inner.source.clone().map(|s| InputSources { inner: s })
    }

    #[setter]
    fn set_source(&mut self, source: Option<InputSources>) {
        self.inner.source = source.map(|s| s.inner);
    }

    #[getter]
    fn get_launch_cmds(&self) -> Vec<String> {
        self.inner.launch_cmds.clone()
    }

    #[setter]
    fn set_launch_cmds(&mut self, cmds: Vec<String>) {
        self.inner.launch_cmds = cmds;
    }

    #[getter]
    fn get_cmds(&self) -> Vec<String> {
        self.inner.cmds.clone()
    }

    #[setter]
    fn set_cmds(&mut self, cmds: Vec<String>) {
        self.inner.cmds = cmds;
    }

    #[getter]
    fn get_cli_out(&self) -> HashMap<String, String> {
        self.inner.cli_out.clone()
    }

    #[setter]
    fn set_cli_out(&mut self, cli_out: HashMap<String, String>) {
        self.inner.cli_out = cli_out;
    }

    #[getter]
    fn get_env(&self) -> String {
        self.inner.env.clone()
    }

    #[setter]
    fn set_env(&mut self, env: String) {
        self.inner.env = env;
    }
}

#[pyfunction]
#[pyo3(signature = (source, values))]
pub fn validate_source_values(
    source: Option<&InputSources>,
    values: &Bound<'_, PyDict>,
) -> PyResult<()> {
    let src = match source {
        Some(s) => &s.inner,
        None => return Ok(()),
    };
    let mut vals = HashMap::new();
    for (k, v) in values.iter() {
        let k_str: String = k.extract()?;
        if v.is_none() {
            continue;
        }
        let v_str: String = v.extract()?;
        if v_str.is_empty() {
            continue;
        }
        vals.insert(k_str, v_str);
    }
    rust_bootstrap::validate_source_values(src, &vals)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

#[pyfunction]
#[pyo3(signature = (cmd, *, cwd, values))]
pub fn substitute_bootstrap_vars(
    cmd: String,
    cwd: PathBuf,
    values: HashMap<String, String>,
) -> String {
    rust_bootstrap::substitute_bootstrap_vars(&cmd, &cwd, &values)
}
