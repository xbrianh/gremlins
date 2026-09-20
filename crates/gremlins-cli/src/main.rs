use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};
use gremlins::artifacts::registry::ArtifactRegistry;
use gremlins::config;
use gremlins::core::discovery;
use gremlins::core::git;
use gremlins::core::proc::run_shell_async;
use gremlins::executor::gremlin::{system_env, validate_gremlin_id, Gremlin};
use gremlins::executor::state::{self, StateData};
use gremlins::schemas::bootstrap;
use gremlins::schemas::expand;
use gremlins::schemas::pipeline::Pipeline;
use gremlins::stages::exec::prepare_exec;
use gremlins::stages::node::RunnableStage;
use serde_json::{Map, Value};

mod spawn;

#[derive(Parser)]
#[command(name = "gremlins", about = "AI-backed gremlin pipeline runner")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmds>,
}

#[derive(Subcommand)]
enum Cmds {
    /// Start a pipeline as a detached background gremlin.
    Launch {
        /// Pipeline definition: a bare name (resolved under .gremlins/) or a path.
        definition: String,
        /// Free-form --key value pairs passed to bootstrap sources.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true, name = "spawn")]
    Run {
        /// Gremlin id to resume or start fresh.
        id: String,
        /// Stage to resume from (internal use).
        #[arg(long, hide = true)]
        resume_from: Option<String>,
    },
    /// List gremlins from the state root as a plain-column table.
    Ls {
        /// Only list gremlins whose `project_root` is the current directory.
        #[arg(long)]
        here: bool,
    },
    /// Print the full runtime state of one gremlin as pretty-printed JSON.
    Info {
        /// Gremlin id to inspect.
        id: String,
    },
    /// Stop a running gremlin.
    Stop {
        /// Gremlin id to stop.
        id: String,
    },
    /// Resume a stopped or bailed gremlin from its last recorded stage.
    Resume {
        /// Gremlin id to resume.
        id: String,
    },
    /// Follow a gremlin's log file with `less +F`.
    Log {
        /// Gremlin id whose log to follow.
        id: String,
    },
    /// Remove a gremlin's filesystem assets.
    Clean {
        /// Gremlin id to clean.
        id: String,
        /// Preserve the state directory (with a `closed` marker).
        #[arg(long)]
        keep: bool,
    },
    /// Remove a gremlin and all its filesystem assets.
    Rm {
        /// Gremlin id to remove.
        id: String,
    },
    /// Run the pipeline's land block in the current working directory.
    Land {
        /// Gremlin id whose land block to run.
        id: String,
    },
    /// `gremlins <id>` — print detailed status for one gremlin.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[tokio::main]
async fn main() {
    // Wire up logging: respect GREMLINS_LOG_LEVEL (default: INFO).
    // _run redirects stderr to the log file, so per-stage DEBUG logs end up
    // captured in the gremlin's log.
    let level = std::env::var("GREMLINS_LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&level))
        .format_timestamp_millis()
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Some(Cmds::Launch { definition, args }) => launch(&definition, &args).await,
        Some(Cmds::Run { id, resume_from }) => run_gremlin(&id, resume_from.as_deref()).await,
        Some(Cmds::Ls { here }) => ls(here),
        Some(Cmds::Info { id }) => info(&id),
        Some(Cmds::Stop { id }) => stop(&id),
        Some(Cmds::Resume { id }) => resume(&id).await,
        Some(Cmds::Log { id }) => log_gremlin(&id),
        Some(Cmds::Clean { id, keep }) => clean(&id, keep),
        Some(Cmds::Rm { id }) => rm(&id),
        Some(Cmds::Land { id }) => land(&id).await,
        Some(Cmds::External(args)) => status_external(&args),
        None => {
            // No subcommand — print help and exit 0.
            let mut cmd = <Cli as clap::CommandFactory>::command();
            cmd.print_help().unwrap();
            return;
        }
    };
    if let Err(e) = result {
        eprintln!("gremlins: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// ls & status
// ---------------------------------------------------------------------------

/// List gremlins whose state directories contain a `state.json`.
///
/// Closed gremlins (those with a `closed` marker next to the state file) are
/// skipped without comment: they were cleaned with `remove_state_dir=false` and
/// no longer represent live runs.
fn ls(here: bool) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    let cwd = std::env::current_dir()
        .map(|path| path.canonicalize().unwrap_or(path))
        .unwrap_or_else(|_| PathBuf::from("."));

    let headers = ["ID", "STATUS", "STAGE", "PIPELINE", "PROJECT", "LAUNCH"];
    let mut rows: Vec<Vec<String>> = Vec::new();

    for (id, state_json_path) in state::list_state_dirs() {
        let Some(state_dir) = state_json_path.parent() else {
            continue;
        };
        if state_dir.join("closed").is_file() {
            continue;
        }

        // A directory with a state.json is only listable when that file is a
        // non-empty JSON object. Malformed or mid-write files are skipped
        // silently, matching the "missing state.json is not fatal" guardrail.
        let Some(state_map) = read_state_object(&state_json_path) else {
            continue;
        };

        let mut data = StateData::new(Some(id.clone()));
        data.state_file = Some(state_json_path.clone());

        let project = state_map
            .get("project_root")
            .and_then(Value::as_str)
            .unwrap_or("");
        if here {
            if project.is_empty() {
                continue;
            }
            let Ok(project_path) = PathBuf::from(project).canonicalize() else {
                continue;
            };
            if project_path != cwd {
                continue;
            }
        }

        let launch = state_map
            .get("metadata")
            .and_then(|v| v.get("cli"))
            .and_then(|v| v.get("launch_cmd"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        rows.push(vec![
            id,
            map_field_display(&state_map, "status"),
            map_field_display(&state_map, "stage"),
            pipeline_display_name(&data),
            project.to_string(),
            launch,
        ]);
    }

    print_table(&headers, &rows);
    Ok(())
}

/// The fallback arm for `gremlins <id>`: an unknown subcommand is exactly the
/// single-id status command, provided it was given exactly one token.
fn status_external(args: &[OsString]) -> Result<(), String> {
    if args.len() != 1 {
        return Err("expected exactly one gremlin id".to_string());
    }
    let id = args[0].to_string_lossy();
    status(&id)
}

/// Print the detailed status block for one gremlin.
fn status(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    // Distinguish a missing id from a malformed one without parsing
    // `Gremlin::from`'s error text: absence of the state directory/file is
    // the only case that gets the `gremlins ls` suggestion.
    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let gremlin = Gremlin::from(id).map_err(|e| format!("gremlin {id}: {e}"))?;

    println!("id:            {}", gremlin.id);
    println!("status:        {}", field_display(&gremlin.state, "status"));
    println!("stage:         {}", field_display(&gremlin.state, "stage"));
    println!("pipeline:      {}", pipeline_display_name(&gremlin.state));
    println!("project_root:  {}", gremlin.project_root.display());
    println!(
        "workdir:       {}",
        gremlin
            .worktree
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    );
    println!("state_dir:     {}", gremlin.state_dir.display());
    println!("artifact_dir:  {}", gremlin.artifact_dir.display());
    println!(
        "started_at:    {}",
        field_display(&gremlin.state, "started_at")
    );
    println!(
        "ended_at:      {}",
        field_display(&gremlin.state, "ended_at")
    );
    println!(
        "exit_code:     {}",
        field_display(&gremlin.state, "exit_code")
    );
    println!("pid:           {}", field_display(&gremlin.state, "pid"));
    println!("client:        {}", field_display(&gremlin.state, "client"));
    println!(
        "attempt:       {}",
        field_display(&gremlin.state, "attempt")
    );
    println!("kind:          {}", field_display(&gremlin.state, "kind"));
    Ok(())
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

/// Print the full runtime state of one gremlin as pretty-printed JSON.
///
/// Uses `Gremlin::from` — the cheap constructor — so the pipeline is never
/// parsed and no client is built.  The log path is reported even if the log
/// file has not been created yet.
fn info(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins show` to list gremlins"
        ));
    }

    let gremlin = Gremlin::from(id).map_err(|e| format!("gremlin {id}: {e}"))?;

    let workdir = gremlin
        .worktree
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();

    let payload = serde_json::json!({
        "id": gremlin.id.as_str(),
        "status": gremlin.state.read_str("status"),
        "stage": gremlin.state.read_str("stage"),
        "pipeline": pipeline_display_name(&gremlin.state),
        "project_root": gremlin.project_root.display().to_string(),
        "workdir": workdir,
        "state_dir": gremlin.state_dir.display().to_string(),
        "artifact_dir": gremlin.artifact_dir.display().to_string(),
        "scratch_dir": config::scratch_root(Some(id)).display().to_string(),
        "log_file": gremlin.state_dir.join("log").display().to_string(),
        "started_at": gremlin.state.read_str("started_at"),
        "ended_at": gremlin.state.read_field("ended_at").unwrap_or(Value::Null),
        "exit_code": gremlin.state.read_field("exit_code").unwrap_or(Value::Null),
        "pid": gremlin.state.read_field("pid").unwrap_or(Value::Null),
        "client": gremlin.state.read_str("client"),
        "attempt": gremlin.state.read_str("attempt"),
        "kind": gremlin.state.read_str("kind"),
        "base_ref": gremlin.base_ref,
        "worktree_base": gremlin.base_ref_sha,
        "bail_info": gremlin
            .state
            .read_bail_info()
            .map(Value::Object)
            .unwrap_or(Value::Null),
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&payload)
            .map_err(|e| format!("failed to serialize gremlin state: {e}"))?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

/// Stop a running gremlin.
///
/// Sends SIGTERM to the process recorded in `pid`, waits a short grace period,
/// then SIGKILL if it is still alive. After the process is confirmed gone,
/// patches `status` to `"stopped"`, sets `ended_at` and `exit_code`, and touches
/// the `finished` marker. Idempotent: a gremlin whose status is already
/// `done` or `stopped` is reported and exits 0 without signalling.
fn stop(id: &str) -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("stop is not implemented on this platform".to_string());
    }

    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let gremlin = Gremlin::from(id).map_err(|e| format!("gremlin {id}: {e}"))?;

    let status = gremlin.state.read_str("status");
    if status == "done" || status == "stopped" {
        println!("gremlin {id} is already {status}");
        return Ok(());
    }

    let pid_raw = gremlin
        .state
        .read_field("pid")
        .and_then(|v| v.as_i64())
        .filter(|&n| n > 0 && n <= libc::pid_t::MAX as i64)
        .unwrap_or(0);

    if pid_raw == 0 {
        // A null PID is normal for a parallel child (fork_child seeds
        // pid: null) — it has no OS process to signal.  Reject the stop
        // so the operator stops the parent instead.
        let parent_id = gremlin.state.read_str("parent_id");
        if !parent_id.is_empty() {
            return Err(format!(
                "gremlin {id} is a parallel child — stop its parent {parent_id} instead"
            ));
        }
        // PID is null or absent in a top-level gremlin — it has already
        // stopped on its own.
        println!("gremlin {id} is already stopped");
        gremlin.state.write_terminal_state(-1);
        return Ok(());
    }

    let pid = pid_raw as libc::pid_t;

    // Send SIGTERM to the entire process group so child processes
    // (agent commands, shell tools) are signalled alongside the runner.
    let mut exit_code = -15i32;
    unsafe {
        let ret = libc::kill(-pid, libc::SIGTERM);
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                // ESRCH: the process is already gone — treat the same as
                // the pid_raw == 0 case above.
                let parent_id = gremlin.state.read_str("parent_id");
                if !parent_id.is_empty() {
                    return Err(format!(
                        "gremlin {id} is a parallel child — stop its parent {parent_id} instead"
                    ));
                }
                println!("gremlin {id} has already exited");
                gremlin.state.write_terminal_state(-1);
                return Ok(());
            }
            return Err(format!("failed to signal gremlin {id}: {err}"));
        }
    }

    // Grace period for SIGTERM.
    std::thread::sleep(std::time::Duration::from_millis(500));

    unsafe {
        // kill(-pgid, 0) checks whether any process in the group is still
        // alive — fails with ESRCH when the group is empty.
        if libc::kill(-pid, 0) == 0 {
            let ret = libc::kill(-pid, libc::SIGKILL);
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(format!("failed to kill gremlin {id}: {err}"));
                }
                // ESRCH after SIGKILL: process died between the liveness
                // check and the signal — that's fine, SIGTERM did the job.
            } else {
                exit_code = -9;
                // Poll until the process exits (bounded).
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if libc::kill(-pid, 0) != 0 {
                        break;
                    }
                }
                if libc::kill(-pid, 0) == 0 {
                    return Err(format!(
                        "gremlin {id}: process group {pid} did not exit after SIGKILL"
                    ));
                }
            }
        }
    }

    gremlin.state.write_terminal_state(exit_code);
    println!("gremlin {id} stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// resume
// ---------------------------------------------------------------------------

/// Resume a stopped or bailed gremlin from its last recorded stage.
///
/// Validates the id, confirms the state directory exists, checks that the
/// gremlin is resumable (not running, not done), patches the state to
/// `"running"`, bumps the attempt suffix, and spawns `_run --resume-from`
/// as a detached child.
async fn resume(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let gremlin = Gremlin::from(id).map_err(|e| format!("gremlin {id}: {e}"))?;

    let status = gremlin.state.read_str("status");

    // A running gremlin must not be resumed — two processes driving the same
    // state directory would corrupt it.
    if status == "running" {
        return Err(format!(
            "gremlin {id} is already running — use `gremlins stop {id}` first"
        ));
    }

    // A done gremlin has no stages left to run.
    if status == "done" {
        return Err(format!("gremlin {id} is already done — nothing to resume"));
    }

    // Only stopped gremlins or those carrying a bail record are resumable.
    let has_bail = gremlin.state.read_bail_info().is_some();
    if status != "stopped" && !has_bail {
        return Err(format!(
            "gremlin {id} cannot be resumed — status is {status:?} with no bail record"
        ));
    }

    // Read the last recorded stage — this is the resume point.
    let stage = gremlin.state.read_str("stage");
    if stage.is_empty() || stage == "starting" {
        return Err(format!("gremlin {id} has no recorded stage to resume from"));
    }

    // Remove the finished marker if present.
    let finished = state_dir.join("finished");
    if finished.is_file() {
        let _ = std::fs::remove_file(&finished);
    }

    // Patch state to "running" *before* spawning the child.  When the child
    // finishes quickly it writes terminal fields (status=done, ended_at,
    // exit_code), and a parent patch after that would overwrite them with
    // stale values.  Publishing first means the child's terminal write is
    // the final word.
    let mut fields = serde_json::Map::new();
    fields.insert(
        "status".to_string(),
        serde_json::Value::String("running".to_string()),
    );
    fields.insert("ended_at".to_string(), serde_json::Value::Null);
    fields.insert("exit_code".to_string(), serde_json::Value::Null);
    let current_attempt = gremlin.state.read_str("attempt");
    let new_attempt = format!("{current_attempt}-resume-{}", state::token_hex(2));
    fields.insert(
        "attempt".to_string(),
        serde_json::Value::String(new_attempt),
    );
    gremlin.state.patch(&[], &fields);

    spawn::spawn_gremlin(id, Some(&stage))?;

    println!("{id}");
    Ok(())
}

/// Follow a gremlin's log file interactively with `less +F`.
///
/// `less` takes over the terminal (foreground) so the user can scroll,
/// search, and toggle follow mode.  stdin/stdout/stderr are inherited.
fn log_gremlin(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    // Treat invalid ids the same as nonexistent — both are "unknown gremlin".
    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if validate_gremlin_id(id).is_err() || !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let log_path = state_dir.join("log");
    if !log_path.is_file() {
        return Err(format!(
            "gremlin {id} has no log file yet — launch it and let it run first"
        ));
    }

    let status = Command::new("less")
        .arg("+F")
        .arg(&log_path)
        .status()
        .map_err(|e| format!("failed to spawn less: {e}"))?;

    if !status.success() {
        return Err(format!("less exited with status {status}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

/// Remove a gremlin and all its filesystem assets.
///
/// This is a simpler variant of `clean` that always removes the state
/// directory — there is no `--keep` option.  A running gremlin is rejected;
/// stop it first.  Nonexistent gremlins produce an error.
fn rm(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => {
            if !state_dir.is_dir() || !state_file.is_file() {
                println!("gremlin {id} is already removed");
                return Ok(());
            }
            return Err(format!("gremlin {id}: {e}"));
        }
    };

    let status = gremlin.state.read_str("status");
    if status == "running" {
        return Err(format!(
            "gremlin {id} is running — use `gremlins stop {id}` first"
        ));
    }

    gremlin.clean(true);
    println!("gremlin {id} removed");
    Ok(())
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

/// Remove a gremlin's filesystem assets.
///
/// By default the worktree, scratch directory, and state directory are all
/// removed.  With `--keep` the state directory is preserved (with a `closed`
/// marker) so fleet viewers can still see the run record.
///
/// A running gremlin is rejected — stop it first.  Nonexistent gremlins
/// produce an error.  Already-cleaned gremlins (state directory gone due to
/// a concurrent clean) are not an error: print a message and exit 0.
fn clean(id: &str, keep: bool) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => {
            // If the state directory disappeared between our check and
            // reconstruction, another process already cleaned it.
            if !state_dir.is_dir() || !state_file.is_file() {
                println!("gremlin {id} is already cleaned");
                return Ok(());
            }
            return Err(format!("gremlin {id}: {e}"));
        }
    };

    let status = gremlin.state.read_str("status");
    if status == "running" {
        return Err(format!(
            "gremlin {id} is running — use `gremlins stop {id}` first"
        ));
    }

    gremlin.clean(!keep);
    println!("gremlin {id} cleaned");
    Ok(())
}

// ---------------------------------------------------------------------------
// land
// ---------------------------------------------------------------------------

/// Run the pipeline's `land` block in the current working directory.
async fn land(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    // Load the pipeline from the hermetic snapshot.
    let pipeline_path = state_dir.join("pipeline.yaml");
    if !pipeline_path.is_file() {
        return Err(format!(
            "gremlin {id}: pipeline snapshot not found at {}",
            pipeline_path.display()
        ));
    }
    let pipeline = Pipeline::from_yaml(&pipeline_path, None)
        .map_err(|e| format!("gremlin {id}: failed to load pipeline: {e}"))?;

    let land_stage = match &pipeline.land {
        Some(stage) => stage,
        None => {
            return Err(format!("gremlin {id}: pipeline has no land block"));
        }
    };

    // Extract the Exec from the RunnableStage::Exec variant.
    let exec = match land_stage {
        RunnableStage::Exec { stage, .. } => stage,
        _ => {
            return Err(format!(
                "gremlin {id}: land stage is not an exec (internal error)"
            ));
        }
    };

    // Read project_root and workdir from state.json for system_env.
    let raw = state::read_state_json(Some(&state_file));

    // A running gremlin must not be landed — its worktree is still being
    // mutated by the agent process.
    if raw.get("status").and_then(Value::as_str) == Some("running") {
        return Err(format!(
            "gremlin {id} is running — use `gremlins stop {id}` first"
        ));
    }

    let project_root = {
        let from_state = raw
            .get("project_root")
            .and_then(Value::as_str)
            .unwrap_or("");
        if from_state.is_empty() {
            config::project_root()
        } else {
            PathBuf::from(from_state)
        }
    };
    let workdir = raw.get("workdir").and_then(Value::as_str).unwrap_or("");
    let worktree = (!workdir.is_empty()).then(|| PathBuf::from(workdir));
    let overlay_dir = config::project_overlay_dir(&project_root);

    // Build a read-only artifact registry from the artifact directory.
    let artifact_dir = config::scratch_root(Some(id)).join("artifacts");
    let registry = ArtifactRegistry::new(artifact_dir.clone());

    // Resolve interpolation references.
    let prepared = prepare_exec(exec, &registry, "", &HashMap::new())
        .map_err(|e| format!("gremlin {id}: {e}"))?;

    if prepared.cmds.is_empty() {
        return Err(format!("gremlin {id}: land block has no commands"));
    }

    let joined = prepared.cmds.join(" && ");
    let cwd =
        std::env::current_dir().map_err(|e| format!("failed to get current directory: {e}"))?;

    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.extend(system_env(
        &artifact_dir,
        &state_dir,
        id,
        &project_root,
        worktree.as_deref(),
        &overlay_dir,
    ));

    let result = run_shell_async(&joined, Some(&cwd), Some(&env), prepared.timeout)
        .await
        .map_err(|e| format!("gremlin {id}: land: {e}"))?;

    // Stream captured output to the terminal.
    {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(&result.stdout);
        let _ = handle.write_all(&result.stderr);
        let _ = handle.flush();
    }

    if result.returncode != 0 {
        std::process::exit(result.returncode);
    }
    Ok(())
}

/// Read a state field for display, treating null/absent as empty.
fn field_display(state: &StateData, field: &str) -> String {
    value_display(state.read_field(field).as_ref())
}

/// Read a field from an already-parsed state map for display.
fn map_field_display(state: &serde_json::Map<String, Value>, field: &str) -> String {
    value_display(state.get(field))
}

fn value_display(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) | None => String::new(),
        Some(value) => value.to_string(),
    }
}

/// Parse a state file into a non-empty JSON object, if it is one.
fn read_state_object(path: &Path) -> Option<serde_json::Map<String, Value>> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    match value {
        Value::Object(map) if !map.is_empty() => Some(map),
        _ => None,
    }
}

/// The pipeline column: the persisted `pipeline_path` file stem when recorded,
/// else the `kind` recorded in state.json.
///
/// The hermetic snapshot is always copied to `state_dir/pipeline.yaml`, so its
/// own stem would collapse every row to "pipeline". The recorded
/// `pipeline_path` is the original definition path and preserves the real name.
fn pipeline_display_name(state: &StateData) -> String {
    let recorded = field_display(state, "pipeline_path");
    if let Some(stem) = Path::new(&recorded)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
    {
        return stem.to_string();
    }
    field_display(state, "kind")
}

/// Print `headers` and `rows` as a plain-column table.
///
/// Each column is padded to the width of its widest cell, and columns are
/// separated by two spaces. No ANSI, no color.
fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.len());
            }
        }
    }

    let header_line = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            let width = widths.get(index).copied().unwrap_or(0);
            format!("{header:<width$}")
        })
        .collect::<Vec<_>>()
        .join("  ");
    println!("{header_line}");

    for row in rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let width = widths.get(index).copied().unwrap_or(0);
                format!("{cell:<width$}")
            })
            .collect::<Vec<_>>()
            .join("  ");
        println!("{line}");
    }
}

// ---------------------------------------------------------------------------
// launch
// ---------------------------------------------------------------------------

async fn launch(definition: &str, raw_args: &[String]) -> Result<(), String> {
    // Parse --key value pairs from the trailing free-form arguments.
    let stage_inputs = parse_stage_inputs(raw_args)?;

    // Bootstrap the global config so path resolvers work.
    config::init_global().map_err(|e| e.to_string())?;

    // Resolve the definition to a pipeline path.
    let project_root = config::project_root();
    let pipeline_path = discovery::resolve_pipeline_path(definition, project_root.clone())
        .map_err(|e| format!("pipeline not found: {e}"))?;

    // Load the pipeline just enough to validate --key args against
    // bootstrap.source.
    let pipeline =
        Pipeline::from_yaml(&pipeline_path, None).map_err(|e| format!("invalid pipeline: {e}"))?;

    match &pipeline.bootstrap.source {
        Some(source) => {
            // Reject any --key that is not a declared source.
            let declared: Vec<String> = source.all_sources();
            for key in stage_inputs.keys() {
                if !declared.iter().any(|d| d == key) {
                    return Err(format!(
                        "unknown input {key:?} — pipeline declares sources: {}",
                        declared.join(", ")
                    ));
                }
            }
            // Validate required sources are present and filepath sources exist.
            bootstrap::validate_source_values(source, &stage_inputs).map_err(|e| format!("{e}"))?;
        }
        None => {
            if !stage_inputs.is_empty() {
                return Err(
                    "pipeline declares no bootstrap.source — no --key args allowed".to_string(),
                );
            }
        }
    }

    // The definition name is the YAML file stem.
    let definition_name = pipeline_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("gremlin");

    // Generate a gremlin id, re-rolling if the state directory already exists.
    let gremlin_id = generate_id(definition_name)?;

    // Resolve the pipeline's base_ref so the worktree branches from the
    // configured branch/tag rather than always from HEAD.
    let base_ref = pipeline.base_ref.clone();
    let base_ref_sha = if base_ref.is_empty() || base_ref == "HEAD" {
        String::new()
    } else {
        git::resolve_base_ref(&base_ref, Some(&project_root))
            .map(|(_name, sha)| sha)
            .map_err(|e| format!("failed to resolve base_ref {base_ref:?}: {e}"))?
    };
    let base_ref_opt = if base_ref.is_empty() {
        None
    } else {
        Some(base_ref.as_str())
    };
    let base_ref_sha_opt = if base_ref_sha.is_empty() {
        None
    } else {
        Some(base_ref_sha.as_str())
    };

    // Create the gremlin: state dir, worktree, initial state.json.
    let gremlin = Gremlin::create(
        &gremlin_id,
        &pipeline_path,
        None,
        None,
        None,
        &stage_inputs,
        false,
        None,
        base_ref_opt,
        base_ref_sha_opt,
    )
    .map_err(|e| format!("failed to create gremlin: {e}"))?;

    // Record the launch command in metadata so `gremlins ls` can show it.
    {
        let launch_cmd = std::iter::once(definition.to_string())
            .chain(raw_args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let mut cli_meta = Map::new();
        cli_meta.insert("launch_cmd".to_string(), Value::String(launch_cmd));
        let mut meta_field = Map::new();
        meta_field.insert("cli".to_string(), Value::Object(cli_meta));
        gremlin.state.patch(&[], &meta_field);
    }

    // Snapshot the fully expanded pipeline YAML into the state directory so
    // the run is hermetic — all prompts, stage-definitions, and recipes are
    // inlined, making the snapshot independent of the original project.
    let hermetic = gremlin.state_dir.join("pipeline.yaml");
    let expanded = expand::parse_pipeline_file(&pipeline_path, &project_root)
        .map_err(|e| format!("failed to expand pipeline: {e}"))?;
    let yaml_str = serde_yaml::to_string(&expanded)
        .map_err(|e| format!("failed to serialize pipeline: {e}"))?;
    fs::write(&hermetic, yaml_str).map_err(|e| format!("failed to snapshot pipeline: {e}"))?;

    // Create an empty log file that the child will append to.
    let log_path = gremlin.state_dir.join("log");
    fs::write(&log_path, "").map_err(|e| format!("failed to create log: {e}"))?;

    // Spawn the child process.
    spawn::spawn_gremlin(&gremlin_id, None)?;

    println!("{gremlin_id}");
    Ok(())
}

/// Turn `["--key1", "value1", "--key2", "value2"]` into a map.
///
/// Every key must be followed by a value.  A `--key` that is the final
/// argument or followed by another `--key` is rejected.
fn parse_stage_inputs(raw: &[String]) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    let mut i = 0;
    while i < raw.len() {
        let key = raw[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected --key value, got {}", raw[i]))?;
        if key.is_empty() {
            return Err("-- with no key name".to_string());
        }
        i += 1;
        let value = if i < raw.len() && !raw[i].starts_with("--") {
            let v = raw[i].clone();
            i += 1;
            v
        } else {
            return Err(format!(
                "--{key} requires a value (found {})",
                if i < raw.len() {
                    &raw[i]
                } else {
                    "end of arguments"
                }
            ));
        };
        map.insert(key.to_string(), value);
    }
    Ok(map)
}

/// Generate a "<definition_name>-<4-hex>" id that does not collide with an
/// existing state directory.
///
/// The directory is atomically reserved via `create_dir` so concurrent
/// launches cannot land on the same id.  If creation fails because the
/// directory already exists, the loop re-rolls.
fn generate_id(name: &str) -> Result<String, String> {
    let state_root = config::state_root();
    loop {
        let hex = state::token_hex(2); // 4 hex chars
        let id = format!("{name}-{hex}");
        if state_root.join(&id).exists() {
            continue;
        }
        match std::fs::create_dir(state_root.join(&id)) {
            Ok(()) => return Ok(id),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to create state dir: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// _run
// ---------------------------------------------------------------------------

async fn run_gremlin(id: &str, resume_from: Option<&str>) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    // Reconstruct the handle from the persisted state directory.
    let mut gremlin = Gremlin::from(id).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("no state at") || msg.contains("gremlin_id contains illegal") {
            format!("unknown gremlin {id:?} — use `gremlins ls` to list gremlins")
        } else {
            format!("gremlin {id}: {msg}")
        }
    })?;

    // Set the resume point when the caller provided one.
    if let Some(stage) = resume_from {
        gremlin.resume_from = Some(stage.to_string());
    }

    // Write our PID — the launcher wrote its own, but we are the process
    // that actually runs the pipeline.
    let mut fields = serde_json::Map::new();
    fields.insert(
        "pid".to_string(),
        serde_json::Value::from(std::process::id() as i64),
    );
    gremlin.state.patch(&[], &fields);

    // Put ourselves in our own process group so `stop` can signal the
    // entire group and reach any child processes spawned by the pipeline.
    #[cfg(unix)]
    unsafe {
        libc::setpgid(0, 0);
    }

    // Redirect stdout and stderr to the gremlin's log file so all output
    // is captured.
    let log_path = gremlin.state_dir.join("log");
    redirect_stdio_to_log(&log_path)?;

    // Run every stage to completion.  The library's run loop handles
    // terminal-state bookkeeping regardless of outcome.
    let exit_code = match gremlin.run().await {
        Ok(ec) => ec,
        Err(e) => {
            // A failure before the stage loop (bootstrap, pipeline loading)
            // returns through `run()` without calling `finish`, leaving
            // `state.json` as "running" with no terminal marker.  Write
            // terminal state here so the run does not appear permanently
            // live.
            gremlin.state.write_terminal_state(1);
            return Err(format!("gremlin {id}: {e}"));
        }
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// stdio redirection
// ---------------------------------------------------------------------------

/// Redirect stdout and stderr to the gremlin's log file.
///
/// On Unix the file descriptor is dup'd directly via `dup2`.  On other
/// platforms logging to file is unimplemented for now — stderr is
/// captured by the parent process.
fn redirect_stdio_to_log(log_path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let log_file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(log_path)
            .map_err(|e| format!("failed to open log: {e}"))?;

        // SAFETY: This is the standard POSIX dup2 dance for output redirection.
        // The log file descriptor is leaked intentionally — it must outlive
        // the process's own stdout/stderr.
        use std::os::fd::AsRawFd;
        let log_fd = log_file.as_raw_fd();
        if unsafe { libc::dup2(log_fd, libc::STDOUT_FILENO) } < 0 {
            return Err("failed to redirect stdout".to_string());
        }
        if unsafe { libc::dup2(log_fd, libc::STDERR_FILENO) } < 0 {
            return Err("failed to redirect stderr".to_string());
        }
        // The log_file handle is dropped here, but the fd was dup'd so it
        // remains open via fds 1 and 2.
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = log_path;
        // Non-Unix redirection not yet implemented — stderr is captured by
        // the parent process.
        Ok(())
    }
}
