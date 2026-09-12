use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use gremlins::executor::state::{self as rust_state, StateData};
use gremlins::stages::constants::FRAMEWORK_KEYS;
use pyo3::exceptions::{PyAttributeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyFrozenSet, PyList};

fn value_to_py(py: Python<'_>, v: &serde_json::Value) -> PyResult<Py<PyAny>> {
    let json = py.import("json")?;
    let s = serde_json::to_string(v).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(json.call_method1("loads", (s,))?.unbind())
}

// The conversions below are best-effort: `None` instead of raising. Their callers
// mirror Python methods that wrapped everything in `try/except: pass`, and state
// bookkeeping must never crash a running gremlin on a bad argument.
fn try_py_to_value(obj: &Bound<'_, PyAny>) -> Option<serde_json::Value> {
    let json = obj.py().import("json").ok()?;
    let s: String = json.call_method1("dumps", (obj,)).ok()?.extract().ok()?;
    serde_json::from_str(&s).ok()
}

fn try_string_seq(obj: &Bound<'_, PyAny>) -> Option<Vec<String>> {
    obj.try_iter()
        .ok()?
        .map(|i| i.ok()?.extract().ok())
        .collect()
}

/// `None` = unconvertible (caller must no-op), `Some(None)` = no sub_stage given.
fn opt_sub_stage(obj: Option<&Bound<'_, PyAny>>) -> Option<Option<serde_json::Value>> {
    match obj.filter(|s| !s.is_none()) {
        Some(s) => try_py_to_value(s).map(Some),
        None => Some(None),
    }
}

fn py_dict_to_map(d: &Bound<'_, PyDict>) -> PyResult<serde_json::Map<String, serde_json::Value>> {
    let json = d.py().import("json")?;
    let s: String = json.call_method1("dumps", (d,))?.extract()?;
    match serde_json::from_str(&s).map_err(|e| PyValueError::new_err(e.to_string()))? {
        serde_json::Value::Object(m) => Ok(m),
        _ => Ok(serde_json::Map::new()),
    }
}

fn map_to_py_dict(
    py: Python<'_>,
    m: &serde_json::Map<String, serde_json::Value>,
) -> PyResult<Py<PyAny>> {
    let out = PyDict::new(py);
    for (k, v) in m {
        out.set_item(k, value_to_py(py, v)?)?;
    }
    Ok(out.into())
}

// --- StateData ---

#[pyclass(name = "StateData", module = "_gremlins_core.executor")]
pub struct PyStateData {
    inner: Mutex<StateData>,
}

impl PyStateData {
    fn with<R>(&self, f: impl FnOnce(&StateData) -> R) -> R {
        f(&self.inner.lock().unwrap())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StateData> {
        self.inner.lock().unwrap()
    }

    fn patch_rs(&self, delete: &[String], fields: &serde_json::Map<String, serde_json::Value>) {
        self.lock().patch(delete, fields);
    }

    fn read_field_rs(&self, field: &str) -> Option<serde_json::Value> {
        self.lock().read_field(field)
    }
}

#[pymethods]
impl PyStateData {
    #[new]
    #[pyo3(signature = (gremlin_id=None))]
    fn new(gremlin_id: Option<String>) -> Self {
        PyStateData {
            inner: Mutex::new(StateData::new(gremlin_id)),
        }
    }

    #[getter]
    fn gremlin_id(&self) -> Option<String> {
        self.with(|d| d.gremlin_id.clone())
    }

    #[setter]
    fn set_gremlin_id(&self, value: Option<String>) {
        let mut d = self.lock();
        d.gremlin_id = value;
        d.invalidate();
    }

    #[getter]
    fn state_file(&self) -> Option<PathBuf> {
        self.with(|d| d.state_file.clone())
    }

    #[setter]
    fn set_state_file(&self, value: Option<PathBuf>) {
        let mut d = self.lock();
        d.state_file = value;
        d.invalidate();
    }

    fn __getattr__(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        if name.starts_with('_') {
            return Err(PyAttributeError::new_err(format!(
                "'StateData' has no field {name:?}"
            )));
        }
        match self.with(|d| d.get_field(name)) {
            Some(v) => value_to_py(py, &v),
            None => Err(PyAttributeError::new_err(format!(
                "'StateData' has no field {name:?}"
            ))),
        }
    }

    fn persist(&self, state_dir: PathBuf, data: &Bound<'_, PyDict>) -> PyResult<()> {
        let gid = self.with(|d| d.gremlin_id.clone());
        match &gid {
            Some(id) => data.set_item("id", id)?,
            None => {
                return Err(PyValueError::new_err(
                    "cannot persist StateData with no gremlin_id",
                ))
            }
        }
        let map = py_dict_to_map(data)?;
        self.inner
            .lock()
            .unwrap()
            .persist(&state_dir, &map)
            .map_err(|e| PyValueError::new_err(e.to_string()))
    }

    #[pyo3(signature = (_delete=None, **fields))]
    fn patch(
        &self,
        _delete: Option<&Bound<'_, PyAny>>,
        fields: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<()> {
        let delete = match _delete.filter(|d| !d.is_none()) {
            Some(d) => match try_string_seq(d) {
                Some(v) => v,
                None => return Ok(()),
            },
            None => Vec::new(),
        };
        let map = match fields {
            Some(d) => match py_dict_to_map(d) {
                Ok(m) => m,
                Err(_) => return Ok(()),
            },
            None => serde_json::Map::new(),
        };
        self.with(|d| d.patch(&delete, &map));
        Ok(())
    }

    fn read_str(&self, field: &str) -> String {
        self.with(|d| d.read_str(field))
    }

    #[pyo3(signature = (stage, sub_stage=None, *, parent_stage=""))]
    fn set_stage(
        &self,
        stage: &str,
        sub_stage: Option<&Bound<'_, PyAny>>,
        parent_stage: &str,
    ) -> PyResult<()> {
        let Some(sub) = opt_sub_stage(sub_stage) else {
            return Ok(());
        };
        self.with(|d| d.set_stage(stage, sub.as_ref(), parent_stage));
        Ok(())
    }

    #[pyo3(signature = (bail_class, bail_detail=""))]
    fn write_bail_file(&self, bail_class: &str, bail_detail: &str) {
        self.with(|d| d.write_bail_file(bail_class, bail_detail));
    }

    fn accumulate_token_usage(&self, usage: HashMap<String, i64>) {
        self.with(|d| d.accumulate_token_usage(&usage));
    }

    fn read_bail_info(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        match self.with(|d| d.read_bail_info()) {
            Some(m) => Ok(Some(map_to_py_dict(py, &m)?)),
            None => Ok(None),
        }
    }

    #[pyo3(signature = (group_name, base_head=None, paths=None))]
    fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<String>,
        paths: Option<HashMap<String, String>>,
    ) {
        self.with(|d| d.patch_parallel_worktrees(group_name, base_head.as_deref(), paths.as_ref()));
    }

    fn done_for(&self, path: &str) -> HashSet<String> {
        self.with(|d| d.done_for(path))
    }

    fn mark_done(&self, path: &str, child_name: &str) {
        self.with(|d| d.mark_done(path, child_name));
    }

    fn clear_done(&self, path: &str) {
        self.with(|d| d.clear_done(path));
    }

    fn add_subprocess_cost(&self, amount: f64) {
        self.with(|d| d.add_subprocess_cost(amount));
    }

    fn patch_parallel_attempt(&self, child_key: &str, attempt: &str) {
        self.with(|d| d.patch_parallel_attempt(child_key, attempt));
    }

    fn write_terminal_state(&self, exit_code: i32) {
        self.with(|d| d.write_terminal_state(exit_code));
    }
}

// --- State ---

#[pyclass(name = "State", module = "_gremlins_core.executor")]
pub struct PyState {
    #[pyo3(get, set)]
    data: Py<PyStateData>,
    #[pyo3(get, set)]
    client: Py<PyAny>,
    #[pyo3(get, set)]
    artifact_dir: PathBuf,
    #[pyo3(get, set)]
    artifacts: Py<PyAny>,
    #[pyo3(get, set)]
    cwd: String,
    #[pyo3(get, set)]
    args: Py<PyAny>,
    #[pyo3(get, set)]
    pipeline_data: Option<Py<PyAny>>,
    // Live Python lists, not freshly built copies: Python mutates them in place
    // (`state.current_scope.append(...)`) and assignment must hit the real state.
    #[pyo3(get, set)]
    current_scope: Py<PyList>,
    #[pyo3(get, set)]
    child_key: Option<String>,
    #[pyo3(get, set)]
    parent_stage: String,
    #[pyo3(get, set)]
    worktree: Option<PathBuf>,
    #[pyo3(get, set)]
    worktree_parent: Option<PathBuf>,
    #[pyo3(get, set)]
    base_ref: String,
    #[pyo3(get, set)]
    loop_stack: Py<PyList>,
}

#[pymethods]
impl PyState {
    #[new]
    #[pyo3(signature = (
        data, client, artifact_dir, artifacts,
        cwd="".to_string(), args=None, pipeline_data=None, current_scope=None,
        child_key=None, parent_stage="".to_string(), worktree=None, worktree_parent=None,
        base_ref="".to_string(), loop_stack=None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        data: Py<PyStateData>,
        client: Py<PyAny>,
        artifact_dir: PathBuf,
        artifacts: Py<PyAny>,
        cwd: String,
        args: Option<Py<PyAny>>,
        pipeline_data: Option<Py<PyAny>>,
        current_scope: Option<Vec<Py<PyAny>>>,
        child_key: Option<String>,
        parent_stage: String,
        worktree: Option<PathBuf>,
        worktree_parent: Option<PathBuf>,
        base_ref: String,
        loop_stack: Option<Vec<(String, i32)>>,
    ) -> PyResult<Self> {
        let args = match args {
            Some(a) => a,
            None => py
                .import("argparse")?
                .getattr("Namespace")?
                .call0()?
                .unbind(),
        };
        Ok(PyState {
            data,
            client,
            artifact_dir,
            artifacts,
            cwd,
            args,
            pipeline_data,
            current_scope: PyList::new(py, current_scope.unwrap_or_default())?.unbind(),
            child_key,
            parent_stage,
            worktree,
            worktree_parent,
            base_ref,
            loop_stack: match loop_stack {
                Some(v) => PyList::new(py, v)?.unbind(),
                None => PyList::empty(py).unbind(),
            },
        })
    }

    #[getter]
    fn loop_iter(&self, py: Python<'_>) -> String {
        let mut parts: Vec<String> = Vec::new();
        for item in self.loop_stack.bind(py).iter() {
            // Lenient: this getter runs through getattr during stage setup, a
            // panic here aborts the whole process.
            if let Ok((name, n)) = item.extract::<(String, i32)>() {
                parts.push(format!("{name}~{n}"));
            }
        }
        if parts.is_empty() {
            "1".to_string()
        } else {
            parts.join("~")
        }
    }

    fn push_loop(&mut self, py: Python<'_>, stage_path: &str) {
        let _ = self
            .loop_stack
            .bind(py)
            .append((stage_path.replace('/', "-"), 1));
    }

    fn pop_loop(&mut self, py: Python<'_>) {
        let list = self.loop_stack.bind(py);
        let len = list.len();
        if len > 0 {
            let _ = list.del_item(len - 1);
        }
    }

    fn set_loop_iteration(&mut self, py: Python<'_>, n: i32) {
        let list = self.loop_stack.bind(py);
        let len = list.len();
        if len == 0 {
            return;
        }
        if let Ok((name, _)) = list
            .get_item(len - 1)
            .and_then(|i| i.extract::<(String, i32)>())
        {
            let _ = list.set_item(len - 1, (name, n));
        }
    }

    fn framework_subs(
        &self,
        py: Python<'_>,
        stage: &Bound<'_, PyAny>,
    ) -> PyResult<HashMap<String, String>> {
        let name: String = stage.getattr("name")?.extract()?;
        let model: String = self
            .client
            .bind(py)
            .getattr("model")?
            .extract::<Option<String>>()
            .unwrap_or_default()
            .unwrap_or_default();
        let mut out = HashMap::new();
        for key in FRAMEWORK_KEYS.iter() {
            let value = match *key {
                "name" => name.clone(),
                "model" => model.clone(),
                "cwd" => self.cwd.clone(),
                "base_ref" => self.base_ref.clone(),
                _ => continue,
            };
            out.insert((*key).to_string(), value);
        }
        Ok(out)
    }

    #[staticmethod]
    #[pyo3(signature = (state_dir, artifact_dir, gremlin_id=None))]
    fn setup_dirs(
        state_dir: PathBuf,
        artifact_dir: PathBuf,
        gremlin_id: Option<&str>,
    ) -> PyResult<()> {
        std::fs::create_dir_all(&state_dir)?;
        std::fs::create_dir_all(&artifact_dir)?;
        let sf = state_dir.join("state.json");
        if let Some(gid) = gremlin_id.filter(|s| !s.is_empty()) {
            if !sf.exists() {
                let mut data = serde_json::Map::new();
                data.insert("id".into(), serde_json::Value::String(gid.to_string()));
                rust_state::write_state(&state_dir, &data)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
            }
        }
        Ok(())
    }

    fn done_for(&self, py: Python<'_>, path: &str) -> HashSet<String> {
        self.data.bind(py).borrow().lock().done_for(path)
    }

    fn mark_done(&self, py: Python<'_>, path: &str, child_name: &str) {
        self.data
            .bind(py)
            .borrow()
            .lock()
            .mark_done(path, child_name);
    }

    fn clear_done(&self, py: Python<'_>, path: &str) {
        self.data.bind(py).borrow().lock().clear_done(path);
    }

    #[pyo3(signature = (reason, *, kind="other"))]
    fn record_bail(&self, py: Python<'_>, reason: &str, kind: &str) {
        self.data
            .bind(py)
            .borrow()
            .lock()
            .write_bail_file(kind, reason);
    }

    #[pyo3(signature = (name, sub_stage=None, *, parent_stage=""))]
    fn record_stage_progress(
        &self,
        py: Python<'_>,
        name: &str,
        sub_stage: Option<&Bound<'_, PyAny>>,
        parent_stage: &str,
    ) -> PyResult<()> {
        let Some(sub) = opt_sub_stage(sub_stage) else {
            return Ok(());
        };
        self.data
            .bind(py)
            .borrow()
            .lock()
            .set_stage(name, sub.as_ref(), parent_stage);
        Ok(())
    }

    #[pyo3(signature = (entry, scope=None, record_stage=true))]
    fn _make_runner_impl(
        &self,
        py: Python<'_>,
        entry: &Bound<'_, PyAny>,
        scope: Option<&Bound<'_, PyAny>>,
        record_stage: bool,
    ) -> PyResult<Py<PyState>> {
        let name: String = entry.getattr("name")?.extract()?;

        if record_stage {
            self.data
                .bind(py)
                .borrow()
                .lock()
                .set_stage(&name, None, &self.parent_stage);
        }

        let client_repr = self.client.bind(py).str()?.to_string();
        let stored: String = self
            .data
            .bind(py)
            .borrow()
            .read_field_rs("client")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        if client_repr != stored {
            let mut fields = serde_json::Map::new();
            fields.insert("client".into(), serde_json::Value::String(client_repr));
            self.data.bind(py).borrow().patch_rs(&[], &fields);
        }

        let gremlin_id = self.data.bind(py).borrow().lock().gremlin_id.clone();
        let attempt = match &gremlin_id {
            Some(_) => format!("{name}-{}", rust_state::token_hex(4)),
            None => String::new(),
        };
        if !attempt.is_empty() {
            match &self.child_key {
                Some(ck) => self
                    .data
                    .bind(py)
                    .borrow()
                    .lock()
                    .patch_parallel_attempt(ck, &attempt),
                None => {
                    let mut fields = serde_json::Map::new();
                    fields.insert("attempt".into(), serde_json::Value::String(attempt));
                    self.data.bind(py).borrow().patch_rs(&[], &fields);
                }
            }
        }

        let fresh = Py::new(
            py,
            PyStateData {
                inner: Mutex::new(StateData::new(gremlin_id)),
            },
        )?;

        let scope_list: Vec<Py<PyAny>> = match scope {
            Some(s) if !s.is_none() => s
                .try_iter()?
                .map(|i| i.map(|x| x.unbind()))
                .collect::<PyResult<_>>()?,
            _ => Vec::new(),
        };
        // Child copies, not shared handles: the child must not mutate the parent.
        let current_scope = PyList::new(py, scope_list)?.unbind();
        let loop_stack = PyList::empty(py);
        for item in self.loop_stack.bind(py).iter() {
            loop_stack.append(item)?;
        }

        Py::new(
            py,
            PyState {
                data: fresh,
                client: self.client.clone_ref(py),
                artifact_dir: self.artifact_dir.clone(),
                artifacts: self.artifacts.clone_ref(py),
                cwd: self.cwd.clone(),
                args: self.args.clone_ref(py),
                pipeline_data: self.pipeline_data.as_ref().map(|p| p.clone_ref(py)),
                current_scope,
                child_key: self.child_key.clone(),
                parent_stage: self.parent_stage.clone(),
                worktree: self.worktree.clone(),
                worktree_parent: self.worktree_parent.clone(),
                base_ref: self.base_ref.clone(),
                loop_stack: loop_stack.unbind(),
            },
        )
    }

    #[pyo3(signature = (entry, gremlin, scope=None, *, record_stage=true))]
    fn make_runner(
        slf: pyo3::PyRef<'_, Self>,
        py: Python<'_>,
        entry: &Bound<'_, PyAny>,
        gremlin: &Bound<'_, PyAny>,
        scope: Option<&Bound<'_, PyAny>>,
        record_stage: bool,
    ) -> PyResult<Py<PyAny>> {
        let this: Py<PyState> = slf.into();
        let kwargs = PyDict::new(py);
        kwargs.set_item("record_stage", record_stage)?;
        let scope = match scope {
            Some(s) if !s.is_none() => s.clone(),
            _ => py.None().into_bound(py),
        };
        Ok(py
            .import("_gremlins_core.executor")?
            .getattr("_make_runner_factory")?
            .call((this, entry, gremlin, scope), Some(&kwargs))?
            .unbind())
    }
}

// --- free functions ---

#[pyfunction]
#[pyo3(signature = (gremlin_id=None))]
fn resolve_state_file(gremlin_id: Option<&str>) -> Option<PathBuf> {
    rust_state::resolve_state_file(gremlin_id.filter(|s| !s.is_empty()))
}

#[pyfunction]
fn write_state(state_dir: PathBuf, data: &Bound<'_, PyDict>) -> PyResult<()> {
    let map = py_dict_to_map(data)?;
    rust_state::write_state(&state_dir, &map).map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pyfunction]
fn locked_update(sf: PathBuf, apply: &Bound<'_, PyAny>) -> PyResult<()> {
    let _lock = rust_state::acquire_lock(&sf).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let data = rust_state::read_json_map(&sf).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let py = apply.py();
    let dict = map_to_py_dict(py, &data)?
        .into_bound(py)
        .cast_into::<PyDict>()?;
    apply.call1((&dict,))?;
    let out = py_dict_to_map(&dict)?;
    rust_state::atomic_write_json(&sf, &out).map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pyfunction]
#[pyo3(signature = (sf=None))]
fn read_state_json(py: Python<'_>, sf: Option<PathBuf>) -> PyResult<Py<PyAny>> {
    map_to_py_dict(py, &rust_state::read_state_json(sf.as_deref()))
}

#[pyfunction]
#[pyo3(signature = (
    data, client, artifact_dir, *,
    args=None, pipeline_data=None, cwd="", worktree=None, worktree_parent=None,
    artifacts=None, child_key=None, parent_stage="", base_ref="",
))]
#[allow(clippy::too_many_arguments)]
fn build_state(
    py: Python<'_>,
    data: Py<PyStateData>,
    client: Py<PyAny>,
    artifact_dir: PathBuf,
    args: Option<Py<PyAny>>,
    pipeline_data: Option<Py<PyAny>>,
    cwd: &str,
    worktree: Option<PathBuf>,
    worktree_parent: Option<PathBuf>,
    artifacts: Option<Py<PyAny>>,
    child_key: Option<String>,
    parent_stage: &str,
    base_ref: &str,
) -> PyResult<Py<PyState>> {
    let artifacts: Py<PyAny> = match artifacts {
        Some(a) => a,
        None => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("artifact_dir", artifact_dir.clone())?;
            py.import("_gremlins_core.artifacts")?
                .getattr("ArtifactRegistry")?
                .call((), Some(&kwargs))?
                .unbind()
        }
    };
    let args = match args {
        Some(a) => a,
        None => py
            .import("argparse")?
            .getattr("Namespace")?
            .call0()?
            .unbind(),
    };
    let cwd = if !cwd.is_empty() {
        cwd.to_string()
    } else if let Some(wt) = &worktree {
        wt.to_string_lossy().to_string()
    } else {
        gremlins::config::project_root()
            .to_string_lossy()
            .to_string()
    };
    Py::new(
        py,
        PyState {
            data,
            client,
            artifact_dir,
            artifacts,
            cwd,
            args,
            pipeline_data,
            current_scope: PyList::empty(py).unbind(),
            child_key,
            parent_stage: parent_stage.to_string(),
            worktree,
            worktree_parent,
            base_ref: base_ref.to_string(),
            loop_stack: PyList::empty(py).unbind(),
        },
    )
}

// --- registration ---

pub fn register_executor_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "executor")?;
    let py = m.py();

    m.add_class::<PyStateData>()?;
    m.add_class::<PyState>()?;
    m.add_function(wrap_pyfunction!(resolve_state_file, &m)?)?;
    m.add_function(wrap_pyfunction!(write_state, &m)?)?;
    m.add_function(wrap_pyfunction!(read_state_json, &m)?)?;
    m.add_function(wrap_pyfunction!(locked_update, &m)?)?;
    m.add_function(wrap_pyfunction!(build_state, &m)?)?;

    parent.add_submodule(&m)?;
    let modules = py.import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.executor", &m)?;

    let globals = PyDict::new(py);
    globals.set_item("_m", &m)?;

    let field_defaults = PyDict::new(py);
    for name in rust_state::field_names() {
        let default = rust_state::default_for(name)
            .ok_or_else(|| PyValueError::new_err(format!("no default for {name}")))?;
        field_defaults.set_item(name, value_to_py(py, &default)?)?;
    }
    globals.set_item("_field_defaults", &field_defaults)?;

    let framework_keys: Vec<&str> = FRAMEWORK_KEYS.iter().copied().collect();
    globals.set_item("_framework_keys", PyFrozenSet::new(py, &framework_keys)?)?;

    py.run(
        &std::ffi::CString::new(
            r#"
import copy as _copy
import dataclasses as _dc
import logging as _logging

_m.StateData.FIELD_DEFAULTS = _field_defaults
_m.State.FRAMEWORK_KEYS = _framework_keys

_StateFields = _dc.make_dataclass("_StateFields", [
    "data", "client", "artifact_dir", "artifacts", "cwd", "args",
    "pipeline_data", "current_scope", "child_key", "parent_stage",
    "worktree", "worktree_parent", "base_ref", "loop_stack",
])
_m.State.__dataclass_fields__ = dict(_StateFields.__dataclass_fields__)

_logger = _logging.getLogger("gremlins.executor.state")


def _make_runner_factory(state, entry, gremlin, scope=None, *, record_stage=True):
    async def _run_async():
        skip = getattr(entry, "skip_if_exists", "") or ""
        if skip:
            skip = skip.replace("{loop_iter}", state.loop_iter)
            if state.artifacts.is_live(skip):
                from _gremlins_core.stages import Done
                _logger.info("stage skipped (artifact exists): %s", entry.name)
                return Done()
        child_gremlin = _copy.copy(gremlin)
        prepared = state._make_runner_impl(entry, scope, record_stage)
        child_gremlin.state = prepared
        child_gremlin.registry = prepared.artifacts
        _logger.info("stage starting: %s (type=%s)", entry.name, entry.type)
        try:
            return await entry.run(child_gremlin)
        finally:
            _logger.info("stage finished: %s", entry.name)
            for h in _logging.getLogger().handlers:
                try:
                    h.flush()
                except Exception:
                    pass
    return _run_async


_m._make_runner_factory = _make_runner_factory
"#,
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?,
        Some(&globals),
        None,
    )?;

    let _ = py;
    Ok(())
}
