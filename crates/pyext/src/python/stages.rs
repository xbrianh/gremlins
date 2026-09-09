use gremlins::stages::constants::{BAIL_KEY, FRAMEWORK_KEYS};
use gremlins::stages::outcome::Done as RustDone;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyFrozenSet;

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

    fn __hash__(&self) -> u64 {
        0
    }
}

// --- Bail exception ---

#[pyclass(extends = PyException, name = "Bail", module = "_gremlins_core.stages")]
struct Bail {
    #[pyo3(get)]
    reason: String,
}

#[pymethods]
impl Bail {
    #[new]
    fn new(reason: String) -> Self {
        Bail { reason }
    }

    fn __str__(&self) -> String {
        self.reason.clone()
    }

    fn __repr__(&self) -> String {
        format!("Bail({:?})", self.reason)
    }
}

// --- Module registration ---

pub fn register_stages_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "stages")?;

    m.add_class::<Done>()?;
    m.add_class::<Bail>()?;
    m.add("Outcome", m.getattr("Done")?)?;

    m.add("_BAIL_KEY", BAIL_KEY)?;
    let keys: Vec<&str> = FRAMEWORK_KEYS.iter().copied().collect();
    m.add("FRAMEWORK_KEYS", PyFrozenSet::new(m.py(), &keys)?)?;

    parent.add_submodule(&m)?;

    let modules = parent.py().import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.stages", &m)?;

    Ok(())
}
