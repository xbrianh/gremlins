use std::path::PathBuf;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::schemas;

#[pyfunction]
#[pyo3(signature = (d, depth=0))]
fn parse_stage(d: &Bound<'_, PyDict>, depth: usize) -> PyResult<Py<PyAny>> {
    schemas::loader::parse_stage(d.py(), d, depth)
}

#[pyfunction]
#[pyo3(signature = (raw, depth=0))]
fn parse_stages(raw: &Bound<'_, PyList>, depth: usize) -> PyResult<Vec<Py<PyAny>>> {
    schemas::loader::parse_stages(raw.py(), raw, depth)
}

#[pyfunction]
fn fill_names(raw_stages: &Bound<'_, PyList>) -> PyResult<()> {
    schemas::loader::fill_names(raw_stages)
}

#[pyfunction]
fn check_duplicate_producers(
    stages: &Bound<'_, PyList>,
    extra_out: Option<&Bound<'_, PyDict>>,
) -> PyResult<()> {
    schemas::loader::check_duplicate_producers(stages, extra_out)
}

#[pyfunction]
fn expand_pipeline(
    py: Python<'_>,
    yaml_path: PathBuf,
    project_root: Option<PathBuf>,
    resolve_pipeline_name_fn: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    schemas::preprocess::expand_pipeline(
        py,
        yaml_path,
        project_root,
        resolve_pipeline_name_fn.bind(py),
    )
}

pub fn register_schemas_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let schemas_mod = PyModule::new(m.py(), "schemas")?;

    schemas_mod.add_class::<schemas::bootstrap::InputSource>()?;
    schemas_mod.add_class::<schemas::bootstrap::InputSources>()?;
    schemas_mod.add_class::<schemas::bootstrap::Bootstrap>()?;
    schemas_mod.add_class::<schemas::pipeline::Pipeline>()?;
    schemas_mod.add_function(wrap_pyfunction!(parse_stage, &schemas_mod)?)?;
    schemas_mod.add_function(wrap_pyfunction!(parse_stages, &schemas_mod)?)?;
    schemas_mod.add_function(wrap_pyfunction!(fill_names, &schemas_mod)?)?;
    schemas_mod.add_function(wrap_pyfunction!(check_duplicate_producers, &schemas_mod)?)?;
    schemas_mod.add_function(wrap_pyfunction!(expand_pipeline, &schemas_mod)?)?;
    schemas_mod.add_function(wrap_pyfunction!(
        schemas::bootstrap::validate_source_values,
        &schemas_mod
    )?)?;
    schemas_mod.add_function(wrap_pyfunction!(
        schemas::bootstrap::substitute_bootstrap_vars,
        &schemas_mod
    )?)?;
    schemas_mod.add_function(wrap_pyfunction!(
        schemas::pipeline::fill_stage_clients,
        &schemas_mod
    )?)?;

    // Add a placeholder STAGE_TYPES so that Python imports triggered during
    // the real STAGE_TYPES construction (e.g. _gremlins_core.stages ->
    // _gremlins_core.schemas.STAGE_TYPES) can resolve the name without
    // error. We replace it with the real dict below.
    let placeholder = PyDict::new(m.py());
    schemas_mod.add("STAGE_TYPES", &placeholder)?;

    m.add_submodule(&schemas_mod)?;

    // Register in sys.modules *before* building the real STAGE_TYPES, because
    // the Python imports triggered by STAGE_TYPES construction may themselves
    // import _gremlins_core.schemas.
    let modules = m.py().import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.schemas", &schemas_mod)?;

    // Build the real STAGE_TYPES dict from the Rust constant
    let stage_types = PyDict::new(m.py());
    for &(name, module, class_name) in schemas::loader::STAGE_TYPES {
        let cls = m.py().import(module)?.getattr(class_name)?;
        stage_types.set_item(name, cls)?;
    }
    // Replace the placeholder with the real dict
    schemas_mod.setattr("STAGE_TYPES", &stage_types)?;

    Ok(())
}
