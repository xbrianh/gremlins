use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};
use gremlins::config;
use gremlins::core::discovery;
use gremlins::executor::gremlin::{validate_gremlin_id, Gremlin};
use gremlins::executor::state::{self, StateData};
use gremlins::schemas::bootstrap;
use gremlins::schemas::pipeline::Pipeline;
use serde_json::Value;

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
    #[command(hide = true, name = "_run")]
    Run {
        /// Gremlin id to resume or start fresh.
        id: String,
    },
    /// List gremlins from the state root as a plain-column table.
    Show {
        /// Only list gremlins whose `project_root` is the current directory.
        #[arg(long)]
        here: bool,
    },
    /// Stop a running gremlin.
    Stop {
        /// Gremlin id to stop.
        id: String,
    },
    /// `gremlins <id>` — print detailed status for one gremlin.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Cmds::Launch { definition, args }) => launch(&definition, &args).await,
        Some(Cmds::Run { id }) => run_gremlin(&id).await,
        Some(Cmds::Show { here }) => show(here),
        Some(Cmds::Stop { id }) => stop(&id),
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
// show & status
// ---------------------------------------------------------------------------

/// List gremlins whose state directories contain a `state.json`.
///
/// Closed gremlins (those with a `closed` marker next to the state file) are
/// skipped without comment: they were cleaned with `remove_state_dir=false` and
/// no longer represent live runs.
fn show(here: bool) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    let cwd = std::env::current_dir()
        .map(|path| path.canonicalize().unwrap_or(path))
        .unwrap_or_else(|_| PathBuf::from("."));

    let headers = ["ID", "STATUS", "STAGE", "PIPELINE", "PROJECT"];
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

        rows.push(vec![
            id,
            map_field_display(&state_map, "status"),
            map_field_display(&state_map, "stage"),
            pipeline_display_name(&data),
            project.to_string(),
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
    // the only case that gets the `gremlins show` suggestion.
    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins show` to list gremlins"
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
            "unknown gremlin {id:?} — use `gremlins show` to list gremlins"
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
        // PID is null or absent — the gremlin has already stopped on its own.
        println!("gremlin {id} is already stopped");
        gremlin.state.write_terminal_state(-1);
        return Ok(());
    }

    let pid = pid_raw as libc::pid_t;

    // Send SIGTERM.
    let mut exit_code = -15i32;
    unsafe {
        let ret = libc::kill(pid, libc::SIGTERM);
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                // ESRCH: the process is already gone — treat as already stopped.
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
        // kill(pid, 0) is the standard existence check — fails with ESRCH
        // if the process is gone, succeeds if it still exists.
        if libc::kill(pid, 0) == 0 {
            let ret = libc::kill(pid, libc::SIGKILL);
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
                    if libc::kill(pid, 0) != 0 {
                        break;
                    }
                }
                if libc::kill(pid, 0) == 0 {
                    return Err(format!(
                        "gremlin {id}: process {pid} did not exit after SIGKILL"
                    ));
                }
            }
        }
    }

    gremlin.state.write_terminal_state(exit_code);
    println!("gremlin {id} stopped");
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
    let pipeline_path = discovery::resolve_pipeline_path(definition, project_root)
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
    let gremlin_id = generate_id(definition_name);

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
        None,
        None,
    )
    .map_err(|e| format!("failed to create gremlin: {e}"))?;

    // Snapshot the resolved pipeline YAML into the state directory so the run
    // is hermetic — later stages and resumptions read this copy, not the
    // original which may have moved or changed.
    let hermetic = gremlin.state_dir.join("pipeline.yaml");
    fs::copy(&pipeline_path, &hermetic).map_err(|e| format!("failed to snapshot pipeline: {e}"))?;

    // Create an empty log file that the child will append to.
    let log_path = gremlin.state_dir.join("log");
    fs::write(&log_path, "").map_err(|e| format!("failed to create log: {e}"))?;

    // Spawn the child process: stdin is /dev/null, stdout and stderr go to
    // the gremlin's log file.
    let log_file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .map_err(|e| format!("failed to open log: {e}"))?;

    let stdout_file = log_file
        .try_clone()
        .map_err(|e| format!("failed to clone log handle: {e}"))?;

    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;

    Command::new(current_exe)
        .arg("_run")
        .arg(&gremlin_id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| format!("failed to spawn gremlin: {e}"))?;

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
fn generate_id(name: &str) -> String {
    let state_root = config::state_root();
    loop {
        let hex = state::token_hex(2); // 4 hex chars
        let id = format!("{name}-{hex}");
        if state_root.join(&id).exists() {
            continue;
        }
        match std::fs::create_dir(state_root.join(&id)) {
            Ok(()) => return id,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// _run
// ---------------------------------------------------------------------------

async fn run_gremlin(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    // Reconstruct the handle from the persisted state directory.
    let mut gremlin = Gremlin::from(id).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("no state at") || msg.contains("gremlin_id contains illegal") {
            format!("unknown gremlin {id:?} — use `gremlins show` to list gremlins")
        } else {
            format!("gremlin {id}: {msg}")
        }
    })?;

    // Write our PID — the launcher wrote its own, but we are the process
    // that actually runs the pipeline.
    let mut fields = serde_json::Map::new();
    fields.insert(
        "pid".to_string(),
        serde_json::Value::from(std::process::id() as i64),
    );
    gremlin.state.patch(&[], &fields);

    // Redirect stdout and stderr to the gremlin's log file so all output
    // is captured.
    let log_path = gremlin.state_dir.join("log");
    redirect_stdio_to_log(&log_path)?;

    // Run every stage to completion.  The library's run loop handles
    // terminal-state bookkeeping regardless of outcome.
    let exit_code = gremlin
        .run()
        .await
        .map_err(|e| format!("gremlin {id}: {e}"))?;
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
