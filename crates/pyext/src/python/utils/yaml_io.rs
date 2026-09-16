//! Python bindings for [`gremlins::core::yaml_io`], exposed as
//! `_gremlins_core.utils.yaml_io`.
//!
//! The four functions mirror the Python module they replace. Two exception
//! classes keep the failure split that module defined: file and parse problems
//! raise `YamlLoadError`, prompt problems raise `PromptLoadError`. The core
//! error enum carries the distinction, so the mapping here is a straight match
//! and the message is the core `Display` output unchanged.
//!
//! `load_yaml_file` reads from disk, so it releases the GIL through `py.detach`
//! for the duration of the read and parse, matching the conventions in
//! [`crate::python::utils::proc`] and [`crate::python::utils::git`]. The other
//! three work on values already in memory.

use std::collections::HashMap;
use std::path::PathBuf;

use gremlins::core::yaml_io::{self, YamlIoError};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::convert::{pyval_to_serde, serde_to_pyval};

create_exception!(_gremlins_core.utils.yaml_io, YamlLoadError, PyException);
create_exception!(_gremlins_core.utils.yaml_io, PromptLoadError, PyException);

/// Translate a core failure into the Python exception callers expect.
fn map_yaml_io_error(error: YamlIoError) -> PyErr {
    let message = error.to_string();
    match error {
        YamlIoError::PromptNotFound { .. }
        | YamlIoError::PromptEmpty { .. }
        | YamlIoError::PromptRender { .. } => PromptLoadError::new_err(message),
        YamlIoError::FileNotFound { .. }
        | YamlIoError::Read { .. }
        | YamlIoError::Parse { .. }
        | YamlIoError::NotAMapping { .. }
        | YamlIoError::Serialize { .. } => YamlLoadError::new_err(message),
    }
}

#[pyfunction]
pub fn load_yaml_file(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
    let value = py
        .detach(|| yaml_io::load_yaml_file(&path))
        .map_err(map_yaml_io_error)?;
    serde_to_pyval(py, &value)
}

#[pyfunction]
pub fn dump_yaml_text(data: &Bound<'_, PyAny>) -> PyResult<String> {
    let value = pyval_to_serde(data)?;
    yaml_io::dump_yaml_text(&value).map_err(map_yaml_io_error)
}

#[pyfunction]
pub fn load_bundled_prompt(name: &str) -> PyResult<String> {
    yaml_io::load_bundled_prompt(name).map_err(map_yaml_io_error)
}

#[pyfunction]
#[pyo3(signature = (name, **kwargs))]
pub fn render_bundled_prompt(name: &str, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<String> {
    let mut subs = HashMap::new();
    if let Some(dict) = kwargs {
        for (key, value) in dict.iter() {
            // The Python original formatted arbitrary `**kwargs` with
            // `str.format`, so coerce each value the same way rather than
            // rejecting a non-string argument with a `TypeError`.
            subs.insert(key.extract::<String>()?, value.str()?.to_string());
        }
    }
    yaml_io::render_bundled_prompt(name, &subs).map_err(map_yaml_io_error)
}
