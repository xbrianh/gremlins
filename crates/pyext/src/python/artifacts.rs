use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::Mutex;

use gremlins::artifacts::registry as rust_registry;
use gremlins::artifacts::resolve as rust_resolve;
use gremlins::artifacts::uri as rust_uri;
use pyo3::create_exception;
use pyo3::exceptions::PyKeyError;
use pyo3::prelude::*;
use pyo3::types::PyType;

// --- Exception types ---

create_exception!(_gremlins_core.artifacts, MissingArtifact, PyKeyError);
create_exception!(_gremlins_core.artifacts, DuplicateArtifact, PyKeyError);

// --- Uri wrapper (unchanged) ---

#[pyclass(name = "Uri", module = "_gremlins_core.artifacts")]
struct Uri {
    inner: rust_uri::Uri,
}

#[pymethods]
impl Uri {
    #[new]
    fn new(scheme: String, path: String) -> Self {
        Uri {
            inner: rust_uri::Uri::new(scheme, path),
        }
    }

    #[staticmethod]
    fn parse(s: &str) -> PyResult<Self> {
        rust_uri::Uri::parse(s)
            .map(|u| Uri { inner: u })
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }

    #[staticmethod]
    fn parse_or_none(s: &str) -> Option<Self> {
        rust_uri::Uri::parse(s).ok().map(|u| Uri { inner: u })
    }

    #[getter]
    fn scheme(&self) -> &str {
        &self.inner.scheme
    }

    #[getter]
    fn path(&self) -> &str {
        &self.inner.path
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!(
            "Uri(scheme={:?}, path={:?})",
            self.inner.scheme, self.inner.path
        )
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        if let Ok(other) = other.extract::<PyRef<'_, Self>>() {
            self.inner == other.inner
        } else {
            false
        }
    }

    fn __hash__(&self) -> u64 {
        let mut s = DefaultHasher::new();
        self.inner.hash(&mut s);
        s.finish()
    }
}

// --- ArtifactRegistry ---

#[pyclass(name = "ArtifactRegistry", module = "_gremlins_core.artifacts")]
pub(crate) struct ArtifactRegistry {
    pub(crate) inner: Mutex<rust_registry::ArtifactRegistry>,
}

#[pymethods]
impl ArtifactRegistry {
    #[new]
    fn new(artifact_dir: PathBuf) -> Self {
        ArtifactRegistry {
            inner: Mutex::new(rust_registry::ArtifactRegistry::new(artifact_dir)),
        }
    }

    #[pyo3(signature = (uri, overwrite = true))]
    fn register(&self, uri: &Uri, overwrite: bool) -> PyResult<String> {
        let inner = &mut *self.inner.lock().unwrap();
        match inner.register(&uri.inner, overwrite) {
            Ok(p) => Ok(p),
            Err(e) => {
                if let Some(dup) = e.downcast_ref::<rust_registry::DuplicateArtifact>() {
                    return Err(DuplicateArtifact::new_err(format!(
                        "duplicate artifact: {:?} already bound to {:?}, cannot rebind to {:?}",
                        dup.key, dup.existing, dup.incoming,
                    )));
                }
                Err(pyo3::exceptions::PyValueError::new_err(e.to_string()))
            }
        }
    }

    fn data_uri(&self, key: &str) -> PyResult<String> {
        self.inner
            .lock()
            .unwrap()
            .data_uri(key)
            .map(|s| s.to_string())
            .map_err(|e| MissingArtifact::new_err(e.to_string()))
    }

    #[pyo3(signature = (uri_str, json_path = None))]
    fn content(&self, uri_str: &str, json_path: Option<&str>) -> PyResult<String> {
        self.inner
            .lock()
            .unwrap()
            .content(uri_str, json_path)
            .map_err(|e| {
                if e.downcast_ref::<rust_registry::MissingArtifact>().is_some() {
                    return MissingArtifact::new_err(format!("artifact not bound: {:?}", uri_str));
                }
                pyo3::exceptions::PyValueError::new_err(e.to_string())
            })
    }

    #[pyo3(signature = (uri))]
    fn exists(&self, uri: &Bound<'_, PyAny>) -> PyResult<bool> {
        let s = if let Ok(s) = uri.extract::<&str>() {
            s.to_string()
        } else if let Ok(uri_obj) = uri.extract::<PyRef<'_, Uri>>() {
            uri_obj.inner.to_string()
        } else {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "expected str or Uri",
            ));
        };
        Ok(self.inner.lock().unwrap().exists(&s))
    }

    fn is_registered(&self, key: &str) -> bool {
        self.inner.lock().unwrap().is_registered(key)
    }

    fn keys(&self) -> Vec<String> {
        self.inner.lock().unwrap().data.keys().cloned().collect()
    }

    #[pyo3(signature = (other, key_map = None, copy_files = false, keys = None))]
    fn merge_from(
        &self,
        other: &ArtifactRegistry,
        key_map: Option<HashMap<String, String>>,
        copy_files: bool,
        keys: Option<HashSet<String>>,
    ) -> PyResult<()> {
        let other_borrowed = other.inner.lock().unwrap();
        self.inner
            .lock()
            .unwrap()
            .merge_from(&other_borrowed, key_map.as_ref(), copy_files, keys.as_ref())
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }

    #[classmethod]
    fn from_registry_file(
        _cls: &Bound<'_, PyType>,
        path: PathBuf,
        artifact_dir: PathBuf,
    ) -> PyResult<Self> {
        rust_registry::ArtifactRegistry::from_registry_file(&path, artifact_dir)
            .map(|r| ArtifactRegistry {
                inner: Mutex::new(r),
            })
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }

    #[getter]
    fn get_registry_path(&self) -> PathBuf {
        self.inner.lock().unwrap().registry_path.clone()
    }

    /// Low-level set: insert a raw key/value into the registry dict and persist.
    /// Used by bootstrap to bind paths computed externally.
    fn _set(&self, key: String, value: String) -> PyResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.data.insert(key, value);
        inner
            .persist()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }
}

// --- Python function ---

#[pyfunction]
#[pyo3(signature = (artifacts, interpolation_map, loop_iter = ""))]
fn resolve_interpolation_map(
    artifacts: &ArtifactRegistry,
    interpolation_map: HashMap<String, String>,
    loop_iter: &str,
) -> PyResult<HashMap<String, String>> {
    let inner = artifacts.inner.lock().unwrap();
    rust_resolve::resolve_interpolation_map(&inner, &interpolation_map, loop_iter).map_err(|e| {
        match &e {
            rust_resolve::ResolveError::MissingArtifact(key) => {
                MissingArtifact::new_err(format!("artifact not bound: {:?}", key))
            }
            rust_resolve::ResolveError::Other(src) => {
                pyo3::exceptions::PyValueError::new_err(src.to_string())
            }
        }
    })
}

// --- Module registration ---

pub fn register_artifacts_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "artifacts")?;
    m.add_class::<Uri>()?;
    m.add("MissingArtifact", m.py().get_type::<MissingArtifact>())?;
    m.add("DuplicateArtifact", m.py().get_type::<DuplicateArtifact>())?;
    m.add_class::<ArtifactRegistry>()?;
    m.add_function(wrap_pyfunction!(resolve_interpolation_map, &m)?)?;
    parent.add_submodule(&m)?;

    let modules = parent.py().import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.artifacts", &m)?;

    Ok(())
}
