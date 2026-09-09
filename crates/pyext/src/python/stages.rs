use gremlins::stages::constants::{BAIL_KEY, FRAMEWORK_KEYS};
use gremlins::stages::outcome::Done as RustDone;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyFrozenSet;

// --- Done pyclass ---

#[pyclass(name = "Done", module = "_gremlins_core.stages")]
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

// Patch .reason onto the class at module init time.
fn patch_bail(py: Python<'_>) -> PyResult<()> {
    py.run(
        c"\
import _gremlins_core.stages as _m
def _reason(self):
    return self.args[0] if self.args else ''
def _str(self):
    return self.args[0] if self.args else ''
_m.Bail.reason = property(_reason)
_m.Bail.__str__ = _str
",
        None,
        None,
    )
}

// --- Module registration ---

pub fn register_stages_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "stages")?;
    let py = m.py();

    m.add_class::<Done>()?;

    // create_exception! registers Bail in the module's namespace automatically
    // so we just call it via a marker to trigger the macro.
    // Bail is now in _gremlins_core.stages.Bail.
    patch_bail(py)?;

    m.add("Outcome", m.getattr("Done")?)?;

    m.add("_BAIL_KEY", BAIL_KEY)?;
    let keys: Vec<&str> = FRAMEWORK_KEYS.iter().copied().collect();
    m.add("FRAMEWORK_KEYS", PyFrozenSet::new(py, &keys)?)?;

    parent.add_submodule(&m)?;

    let modules = py.import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.stages", &m)?;

    Ok(())
}
