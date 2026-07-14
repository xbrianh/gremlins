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
    let utils = PyModule::new(m.py(), "utils")?;
    let proc = PyModule::new(m.py(), "proc")?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_ok, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_ok_async, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_quiet, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_or_raise, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_async, &proc)?)?;
    utils.add_submodule(&proc)?;
    m.add_submodule(&utils)?;
    let modules = m.py().import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.utils", &utils)?;
    modules.set_item("_gremlins_core.utils.proc", &proc)?;
    m.add_function(wrap_pyfunction!(__version__, m)?)?;
    Ok(())
}
