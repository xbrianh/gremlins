mod convert;
mod python;
pub mod schemas;

use pyo3::prelude::*;

/// The `_gremlins_core` native extension module.
#[pymodule(name = "_gremlins_core")]
fn _gremlins_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let _ = pyo3_log::init();

    // utils submodule
    let utils = PyModule::new(m.py(), "utils")?;
    let proc = PyModule::new(m.py(), "proc")?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_ok, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_ok_async, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_quiet, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_or_raise, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run, &proc)?)?;
    proc.add_function(wrap_pyfunction!(python::utils::proc::run_async, &proc)?)?;
    proc.add_function(wrap_pyfunction!(
        python::utils::proc::run_shell_async,
        &proc
    )?)?;
    proc.add_function(wrap_pyfunction!(
        python::utils::proc::terminate_with_grace,
        &proc
    )?)?;
    utils.add_submodule(&proc)?;
    m.add_submodule(&utils)?;
    // Register in sys.modules immediately so that Python imports triggered
    // by later submodule registration (e.g. schemas) can find them.
    let modules = m.py().import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.utils", &utils)?;
    modules.set_item("_gremlins_core.utils.proc", &proc)?;

    // clients submodule
    let clients = PyModule::new(m.py(), "clients")?;
    python::clients::init_clients_module(&clients)?;
    m.add_submodule(&clients)?;
    modules.set_item("_gremlins_core.clients", &clients)?;

    // artifacts submodule — must be registered before schemas because
    // register_schemas_module imports _gremlins_core.artifacts.Uri.
    python::artifacts::register_artifacts_module(m)?;

    // assets submodule
    python::assets::register_assets_module(m)?;

    // config submodule — must be registered before schemas because the
    // Python imports triggered by register_schemas_module also pull in
    // _gremlins_core.config.
    python::config::register_config_module(m)?;

    // executor submodule — must be registered before schemas because
    // STAGE_TYPES construction imports gremlins.stages.parallel, which
    // imports _gremlins_core.executor.
    python::executor::register_executor_module(m)?;

    // stages submodule — must be registered before schemas because
    // register_schemas_module builds STAGE_TYPES from _gremlins_core.stages.
    python::stages::register_stages_module(m)?;

    // schemas submodule
    python::schemas::register_schemas_module(m)?;

    // discovery submodule
    python::discovery::register_discovery_module(m)?;

    Ok(())
}
