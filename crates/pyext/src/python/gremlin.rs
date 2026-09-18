use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use gremlins::executor::gremlin::{validate_gremlin_id, Gremlin};
use gremlins::executor::state::StateData;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};

use crate::python::json_conv::py_to_value;
use crate::schemas::pipeline::Pipeline;

/// Python-exposed wrapper around [`Gremlin`].
///
/// The inner gremlin is stored behind a [`Mutex`] so that [`PyGremlin::run`]
/// can take ownership (the Rust [`Gremlin::run`] consumes `self`).  Once
/// `run()` has been called the inner [`Option`] is `None` and property
/// getters return defaults.
#[pyclass(name = "Gremlin", module = "_gremlins_core")]
pub struct PyGremlin {
    inner: Mutex<Option<Gremlin>>,
}

#[pymethods]
impl PyGremlin {
    // --- constructors ---------------------------------------------------

    /// Start a fresh run of `pipeline_path` under the id `id`.
    #[classmethod]
    #[pyo3(signature = (
        id,
        pipeline_path,
        *,
        client_override = None,
        worktree_parent = None,
        resume_from = None,
        stage_inputs = None,
        fetch_worktree = false,
        worktree_dir = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn launch(
        _cls: &Bound<'_, PyType>,
        id: &str,
        pipeline_path: PathBuf,
        client_override: Option<&str>,
        worktree_parent: Option<PathBuf>,
        resume_from: Option<&str>,
        stage_inputs: Option<HashMap<String, String>>,
        fetch_worktree: bool,
        worktree_dir: Option<PathBuf>,
    ) -> PyResult<Self> {
        let stage_inputs = stage_inputs.unwrap_or_default();
        let worktree_parent_ref = worktree_parent.as_deref();
        let worktree_dir_ref = worktree_dir.as_deref();

        let gremlin = Gremlin::launch(
            id,
            &pipeline_path,
            client_override,
            worktree_parent_ref,
            resume_from,
            &stage_inputs,
            fetch_worktree,
            worktree_dir_ref,
        )
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        Ok(PyGremlin {
            inner: Mutex::new(Some(gremlin)),
        })
    }

    /// Reconstruct a handle from a persisted state directory.
    #[classmethod]
    #[pyo3(signature = (id))]
    fn open(_cls: &Bound<'_, PyType>, id: &str) -> PyResult<Self> {
        let gremlin = Gremlin::open(id).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(PyGremlin {
            inner: Mutex::new(Some(gremlin)),
        })
    }

    // --- run ------------------------------------------------------------

    /// Drive the gremlin to completion, consuming it.
    ///
    /// Spawns a dedicated OS thread with its own single-threaded tokio
    /// runtime, runs [`Gremlin::run`] on it, and returns the exit code.
    async fn run(&self) -> PyResult<i32> {
        let mut gremlin = self
            .inner
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("gremlin already consumed"))?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build gremlin runtime");
            let outcome = rt.block_on(async { gremlin.run().await });
            let _ = tx.send(outcome);
        });

        match rx.await {
            Ok(Ok(code)) => Ok(code),
            Ok(Err(e)) => Err(PyRuntimeError::new_err(e.to_string())),
            Err(_) => Err(PyRuntimeError::new_err("gremlin thread panicked")),
        }
    }

    // --- property getters -----------------------------------------------

    #[getter]
    fn id(&self) -> PyResult<String> {
        self.with(|g| Ok(g.id.to_string()))
    }

    #[getter]
    fn state_dir(&self) -> PyResult<PathBuf> {
        self.with(|g| Ok(g.state_dir.clone()))
    }

    #[getter]
    fn worktree(&self) -> PyResult<Option<PathBuf>> {
        self.with(|g| Ok(g.worktree.clone()))
    }

    #[getter]
    fn artifact_dir(&self) -> PyResult<PathBuf> {
        self.with(|g| Ok(g.artifact_dir.clone()))
    }

    #[getter]
    fn project_root(&self) -> PyResult<PathBuf> {
        self.with(|g| Ok(g.project_root.clone()))
    }

    #[getter]
    fn base_ref_sha(&self) -> PyResult<String> {
        self.with(|g| Ok(g.base_ref_sha.clone()))
    }

    #[getter]
    fn base_ref(&self) -> PyResult<String> {
        self.with(|g| Ok(g.base_ref.clone()))
    }

    #[getter]
    fn resume_from(&self) -> PyResult<Option<String>> {
        self.with(|g| Ok(g.resume_from.clone()))
    }

    #[getter]
    fn gremlin_id(&self) -> PyResult<Option<String>> {
        self.with(|g| Ok(Some(g.id.to_string())))
    }

    #[getter]
    fn state_data(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with(|g| {
            let executor_mod = py.import("_gremlins_core.executor")?;
            let state_data_cls = executor_mod.getattr("StateData")?;
            let sd = state_data_cls.call1((Some(g.id.to_string()),))?;
            Ok(sd.into())
        })
    }

    #[getter]
    fn finished(&self) -> PyResult<bool> {
        self.with(|g| Ok(g.state_dir.join("finished").is_file()))
    }

    #[getter]
    fn pipeline_data(&self, py: Python<'_>) -> PyResult<Py<Pipeline>> {
        self.with(|g| {
            let p = Pipeline {
                name: g.pipeline.name.clone(),
                path: g.pipeline.path.clone(),
                stages: Vec::new(),
                default_client: None,
                base_ref: g.pipeline.base_ref.clone(),
                bootstrap: None,
                land: None,
            };
            Py::new(py, p)
        })
    }

    #[getter]
    fn pipeline_args(&self) -> PyResult<Vec<String>> {
        self.with(|g| {
            let raw = g.state.read_str("pipeline_args");
            Ok(if raw.is_empty() {
                Vec::new()
            } else {
                raw.split_whitespace().map(String::from).collect()
            })
        })
    }

    #[getter]
    fn pipeline_path(&self) -> PyResult<String> {
        self.with(|g| Ok(g.state.read_str("pipeline_path")))
    }

    // --- static helpers ------------------------------------------------

    /// Patch the state of a gremlin by id.
    ///
    /// Delegates to [`StateData::patch`].  Accepts keyword arguments that
    /// become the fields to patch.
    #[staticmethod]
    #[pyo3(signature = (gremlin_id, **fields))]
    fn patch_state_for(gremlin_id: &str, fields: Option<&Bound<'_, PyDict>>) -> PyResult<()> {
        validate_gremlin_id(gremlin_id).map_err(PyRuntimeError::new_err)?;
        let state = StateData::new(Some(gremlin_id.to_string()));
        let map = match fields {
            Some(dict) => {
                let mut m = serde_json::Map::new();
                for (k, v) in dict.iter() {
                    let key: String = k.extract()?;
                    m.insert(key, py_to_value(&v)?);
                }
                m
            }
            None => serde_json::Map::new(),
        };
        state.patch(&[], &map);
        Ok(())
    }

    /// Read the bail info for a gremlin by id.
    ///
    /// Delegates to [`StateData::read_bail_info`].
    #[staticmethod]
    fn bail_info_for(gremlin_id: &str) -> PyResult<Option<HashMap<String, String>>> {
        validate_gremlin_id(gremlin_id).map_err(PyRuntimeError::new_err)?;
        let state = StateData::new(Some(gremlin_id.to_string()));
        Ok(state.read_bail_info().map(|m| {
            m.into_iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => (k, s),
                    other => (k, other.to_string()),
                })
                .collect()
        }))
    }
}

impl PyGremlin {
    /// Access the inner gremlin through the lock, returning an error when it
    /// has already been consumed.
    fn with<F, R>(&self, f: F) -> PyResult<R>
    where
        F: FnOnce(&Gremlin) -> PyResult<R>,
    {
        let guard = self.inner.lock().unwrap();
        match guard.as_ref() {
            Some(gremlin) => f(gremlin),
            None => Err(PyRuntimeError::new_err("gremlin already consumed")),
        }
    }
}
