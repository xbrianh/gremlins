mod core;
mod python;

use pyo3::prelude::*;

/// The version of the native extension.
#[pyfunction]
fn __version__() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The `_gremlins_core` native extension module.
#[pymodule(name = "_gremlins_core")]
fn _gremlins_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(python::proc::run_ok, m)?)?;
    m.add_function(wrap_pyfunction!(__version__, m)?)?;
    Ok(())
}
