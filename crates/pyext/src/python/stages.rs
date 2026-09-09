use gremlins::stages::constants::{BAIL_KEY, FRAMEWORK_KEYS};
use gremlins::stages::outcome::Done as RustDone;
use pyo3::create_exception;
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

    fn __hash__(&self) -> isize {
        0
    }
}

// --- Bail exception ---

create_exception!(_gremlins_core.stages, Bail, PyException);

// Patch .reason onto the class at module init time.
fn patch_bail(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    let globals = pyo3::types::PyDict::new(py);
    globals.set_item("_m", m)?;
    py.run(
        c"\
def _reason(self):
    return self.args[0] if self.args else ''
def _str(self):
    return self.args[0] if self.args else ''
_m.Bail.reason = property(_reason)
_m.Bail.__str__ = _str
",
        Some(&globals),
        None,
    )
}

// --- Module registration ---

pub fn register_stages_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "stages")?;
    let py = m.py();

    m.add_class::<Done>()?;
    // Bail must be added explicitly (like artifacts.rs does for its exceptions)
    // so it's available on the module before patch_bail tries to access _m.Bail.
    m.add("Bail", m.py().get_type::<Bail>())?;

    // Register in parent and sys.modules before patch_bail so _m.Bail is findable.
    parent.add_submodule(&m)?;
    let modules = py.import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.stages", &m)?;

    patch_bail(py, &m)?;

    m.add("Outcome", m.getattr("Done")?)?;

    m.add("_BAIL_KEY", BAIL_KEY)?;
    let keys: Vec<&str> = FRAMEWORK_KEYS.iter().copied().collect();
    m.add("FRAMEWORK_KEYS", PyFrozenSet::new(py, &keys)?)?;

    Ok(())
}
