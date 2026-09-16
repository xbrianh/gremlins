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
    proc.add_function(wrap_pyfunction!(
        python::utils::proc::terminate_with_grace_blocking,
        &proc
    )?)?;
    proc.add_class::<python::utils::proc::ProcResult>()?;
    proc.add(
        "CalledProcessError",
        proc.py()
            .get_type::<python::utils::proc::CalledProcessError>(),
    )?;
    proc.add(
        "TimeoutExpired",
        proc.py().get_type::<python::utils::proc::TimeoutExpired>(),
    )?;
    utils.add_submodule(&proc)?;

    // git submodule
    let git = PyModule::new(m.py(), "git")?;
    git.add_function(wrap_pyfunction!(python::utils::git::in_git_repo, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::head_sha, &git)?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::status_porcelain,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::has_dirty_worktree,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::has_commits, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::current_branch, &git)?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::resolve_base_ref,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::is_ancestor, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::merge_base, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::rev_list_count, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::log_oneline, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::diff_stat, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::ls_others, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::toplevel, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::squash_merge, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::reset_hard, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::clean_fd, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::commit, &git)?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::ff_merge, &git)?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::force_update_branch,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::try_fetch_all, &git)?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::setup_detached_worktree,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::remove_worktree, &git)?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::in_git_repo_async,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(python::utils::git::head_sha_async, &git)?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::status_porcelain_async,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::setup_detached_worktree_async,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::remove_worktree_async,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::prune_worktrees_async,
        &git
    )?)?;
    git.add_function(wrap_pyfunction!(
        python::utils::git::remove_worktrees_async,
        &git
    )?)?;
    git.add(
        "GitError",
        git.py().get_type::<python::utils::git::GitError>(),
    )?;
    utils.add_submodule(&git)?;

    // env_file submodule
    let env_file = PyModule::new(m.py(), "env_file")?;
    env_file.add_function(wrap_pyfunction!(
        python::utils::env_file::load_env_file_isolated,
        &env_file
    )?)?;
    env_file.add_function(wrap_pyfunction!(
        python::utils::env_file::source_env_string,
        &env_file
    )?)?;
    utils.add_submodule(&env_file)?;

    // yaml_io submodule
    let yaml_io = PyModule::new(m.py(), "yaml_io")?;
    yaml_io.add_function(wrap_pyfunction!(
        python::utils::yaml_io::load_yaml_file,
        &yaml_io
    )?)?;
    yaml_io.add_function(wrap_pyfunction!(
        python::utils::yaml_io::dump_yaml_text,
        &yaml_io
    )?)?;
    yaml_io.add_function(wrap_pyfunction!(
        python::utils::yaml_io::load_bundled_prompt,
        &yaml_io
    )?)?;
    yaml_io.add_function(wrap_pyfunction!(
        python::utils::yaml_io::render_bundled_prompt,
        &yaml_io
    )?)?;
    yaml_io.add(
        "YamlLoadError",
        yaml_io
            .py()
            .get_type::<python::utils::yaml_io::YamlLoadError>(),
    )?;
    yaml_io.add(
        "PromptLoadError",
        yaml_io
            .py()
            .get_type::<python::utils::yaml_io::PromptLoadError>(),
    )?;
    utils.add_submodule(&yaml_io)?;
    m.add_submodule(&utils)?;
    // Register in sys.modules immediately so that Python imports triggered
    // by later submodule registration (e.g. schemas) can find them.
    let modules = m.py().import("sys")?.getattr("modules")?;
    modules.set_item("_gremlins_core.utils", &utils)?;
    modules.set_item("_gremlins_core.utils.proc", &proc)?;
    modules.set_item("_gremlins_core.utils.git", &git)?;
    modules.set_item("_gremlins_core.utils.env_file", &env_file)?;
    modules.set_item("_gremlins_core.utils.yaml_io", &yaml_io)?;

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
    // STAGE_TYPES construction imports _gremlins_core.stages, which
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
