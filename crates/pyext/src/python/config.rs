use std::collections::HashMap;
use std::sync::Arc;

use pyo3::prelude::*;

use gremlins::config::{self, Config};

/// Python-exposed Config wrapper.
#[pyclass(name = "PyConfig")]
pub struct PyConfig {
    inner: Arc<Config>,
}

#[pymethods]
impl PyConfig {
    #[getter]
    fn default_client(&self) -> Option<&str> {
        self.inner.default_client()
    }

    fn default_client_by_stage(&self) -> (HashMap<String, String>, HashMap<String, String>) {
        let (exact, prefix) = self.inner.default_client_by_stage();
        (exact.clone(), prefix.clone())
    }
}

// ---------------------------------------------------------------------------
// Module-level functions — thin delegation to Rust global
// ---------------------------------------------------------------------------

#[pyfunction]
fn inject_sentinals() -> PyResult<()> {
    config::inject_sentinals()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(e.to_string()))?;
    Ok(())
}

#[pyfunction]
fn overlay_dirname() -> &'static str {
    gremlins::config::overlay_dirname()
}

#[pyfunction]
fn init() -> PyResult<()> {
    config::init_global()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(e.to_string()))?;
    Ok(())
}

#[pyfunction]
fn get_config() -> PyResult<PyConfig> {
    Ok(PyConfig {
        inner: config::global_config()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(e.to_string()))?,
    })
}

#[pyfunction]
fn clear() -> PyResult<()> {
    config::clear_global();
    Ok(())
}

// ---------------------------------------------------------------------------
// Path functions
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (config=None))]
fn state_root(config: Option<PyRef<'_, PyConfig>>) -> PyResult<String> {
    if let Some(cfg) = config {
        let overrides = cfg.inner.path_overrides();
        Ok(gremlins::config::resolve_state_root(Some(overrides))
            .to_string_lossy()
            .to_string())
    } else {
        Ok(config::state_root().to_string_lossy().to_string())
    }
}

#[pyfunction]
#[pyo3(signature = (config=None))]
fn work_root(config: Option<PyRef<'_, PyConfig>>) -> PyResult<String> {
    if let Some(cfg) = config {
        let overrides = cfg.inner.path_overrides();
        Ok(gremlins::config::resolve_work_root(Some(overrides))
            .to_string_lossy()
            .to_string())
    } else {
        Ok(config::work_root().to_string_lossy().to_string())
    }
}

#[pyfunction]
#[pyo3(signature = (config=None))]
fn user_config_root(config: Option<PyRef<'_, PyConfig>>) -> PyResult<String> {
    if let Some(cfg) = config {
        let overrides = cfg.inner.path_overrides();
        Ok(gremlins::config::resolve_user_config_root(Some(overrides))
            .to_string_lossy()
            .to_string())
    } else {
        Ok(config::user_config_root().to_string_lossy().to_string())
    }
}

#[pyfunction]
#[pyo3(signature = (config=None))]
fn project_root(config: Option<PyRef<'_, PyConfig>>) -> PyResult<String> {
    if let Some(cfg) = config {
        let overrides = cfg.inner.path_overrides();
        Ok(gremlins::config::resolve_project_root(Some(overrides))
            .to_string_lossy()
            .to_string())
    } else {
        Ok(config::project_root().to_string_lossy().to_string())
    }
}

#[pyfunction]
#[pyo3(signature = (gremlin_id=None, config=None))]
fn scratch_root(gremlin_id: Option<&str>, config: Option<PyRef<'_, PyConfig>>) -> PyResult<String> {
    if let Some(cfg) = config {
        let overrides = cfg.inner.path_overrides();
        Ok(
            gremlins::config::resolve_scratch_root(Some(overrides), gremlin_id)
                .to_string_lossy()
                .to_string(),
        )
    } else {
        Ok(config::scratch_root(gremlin_id)
            .to_string_lossy()
            .to_string())
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_config_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let config_mod = PyModule::new(m.py(), "config")?;

    config_mod.add_class::<PyConfig>()?;
    config_mod.add_function(wrap_pyfunction!(init, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(get_config, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(clear, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(inject_sentinals, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(state_root, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(work_root, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(user_config_root, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(project_root, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(scratch_root, &config_mod)?)?;
    config_mod.add_function(wrap_pyfunction!(overlay_dirname, &config_mod)?)?;

    m.add_submodule(&config_mod)?;

    let modules = m.py().import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.config", &config_mod)?;

    Ok(())
}
