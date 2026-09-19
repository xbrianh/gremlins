use std::collections::HashMap;
use std::fs;
use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};
use gremlins::config;
use gremlins::core::discovery;
use gremlins::executor::gremlin::Gremlin;
use gremlins::executor::state;
use gremlins::schemas::bootstrap;
use gremlins::schemas::pipeline::Pipeline;

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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Cmds::Launch { definition, args }) => launch(&definition, &args).await,
        Some(Cmds::Run { id }) => run_gremlin(&id).await,
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
