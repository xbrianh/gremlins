use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use gremlins::config;
use gremlins::core::discovery;
use gremlins::core::proc::run_shell_async;
use gremlins::definition::{ExecutorStage, GremlinDefinition, StaticDefinition};
use gremlins::executor::exec_runner::prepare_exec;
use gremlins::executor::gremlin::{system_env, validate_gremlin_id, Gremlin};
use gremlins::executor::socket::{self, GremlinsDaemonLock};
use gremlins::executor::state::FileSystemStateStore;
use gremlins::executor::state::{self, BlobMode, StateData, StateStore};
use gremlins::executor::supervisor;
use gremlins::schemas::bootstrap;
use serde_json::Value;

mod spawn;
mod tui;

#[derive(Parser)]
#[command(name = "gremlins", about = "AI-backed gremlin definition runner", version = gremlins::VERSION)]
struct Cli {
    /// Increase log verbosity (once for info, twice for debug).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Option<Cmds>,
}

#[derive(Subcommand)]
enum Cmds {
    /// Start a definition as a detached background gremlin.
    Launch {
        /// Gremlin definition: a bare name (resolved under .gremlins/) or a path.
        definition: String,
        /// Free-form --key value pairs passed to bootstrap sources.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Run with temp-backed storage — no footprint on disk after completion.
        #[arg(long)]
        ephemeral: bool,
        /// Model spec supplying `default_model` only when the definition YAML has none.
        #[arg(long)]
        model: Option<String>,
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
    /// Interactively debug a running gremlin in an agent stage.
    Debug {
        /// Gremlin id to debug.
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
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        /// Remove every gremlin in a terminal state (done/stopped/orphaned).
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Run the definition's land block in the current working directory.
    Land {
        /// Gremlin id whose land block to run.
        id: String,
    },
    /// Expand a modular definition into a self-contained YAML monolith.
    Bundle {
        /// Gremlin definition: a bare name (resolved under .gremlins/) or a path.
        definition: String,
    },
    #[command(hide = true, name = "serve")]
    Serve {
        /// Lock file descriptor (internal, inherited from parent).
        #[arg(hide = true)]
        lock_fd: i32,
    },
    /// Print CLI and daemon build versions.
    Version,
    /// `gremlins <id>` — print detailed status for one gremlin.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}
#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Compute the default log level from the parsed command and flag.
    // `serve` (the executor daemon) keeps `info` — its stderr goes to
    // executor.log, not the terminal. Every other command defaults to
    // `warn` (silent for info) unless -v/-vv raises it.
    let default_level = match &cli.command {
        Some(Cmds::Serve { .. }) => "info",
        _ => match cli.verbose {
            0 => "warn",
            1 => "info",
            _ => "debug",
        },
    };

    env_logger::Builder::from_env(
        env_logger::Env::new().filter_or("GREMLINS_LOG_LEVEL", default_level),
    )
    .format_timestamp_millis()
    .write_style(env_logger::WriteStyle::Never)
    .init();

    let result = match cli.command {
        Some(Cmds::Launch {
            definition,
            args,
            ephemeral,
            model,
        }) => launch(&definition, args, ephemeral, model).await,
        Some(Cmds::Ls { here }) => ls(here).await,
        Some(Cmds::Info { id }) => info(&id).await,
        Some(Cmds::Stop { id }) => stop(&id).await,
        Some(Cmds::Resume { id }) => resume(&id).await,
        Some(Cmds::Log { id }) => log_gremlin(&id).await,
        Some(Cmds::Debug { id }) => debug_gremlin(&id).await,
        Some(Cmds::Clean { id, keep }) => clean(&id, keep).await,
        Some(Cmds::Rm { id, all }) => {
            if all {
                rm_all().await
            } else if let Some(ref id) = id {
                rm_one(id).await
            } else {
                Err("gremlins rm requires an <id> or --all".to_string())
            }
        }
        Some(Cmds::Land { id }) => land(&id).await,
        Some(Cmds::Bundle { definition }) => bundle(&definition).await,
        Some(Cmds::Serve { lock_fd }) => serve_daemon(lock_fd).await,
        Some(Cmds::Version) => version_cmd().await,
        Some(Cmds::External(args)) => status_external(&args).await,
        None => match tui::run().await {
            Ok(()) => Ok(()),
            Err(tui::RunError::Reported) => std::process::exit(1),
            Err(tui::RunError::Unreported(e)) => Err(e),
        },
    };
    if let Err(e) = result {
        eprintln!("gremlins: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Socket helpers
// ---------------------------------------------------------------------------

/// Send a request to the executor and return the response.
///
/// Checks CLI/daemon version match before dispatching the caller's request.
async fn executor_request(request: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = spawn::ensure_executor().await?;
    spawn::check_version(&mut stream).await?;
    spawn::send_request(&mut stream, request).await
}

/// Print CLI and daemon build versions.
async fn version_cmd() -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    println!("gremlins CLI {}", gremlins::VERSION);

    match spawn::connect().await {
        Ok(mut stream) => {
            match spawn::send_request(&mut stream, serde_json::json!({"op": "version"})).await {
                Ok(resp) => {
                    if resp.get("type").and_then(|v| v.as_str()) == Some("error") {
                        let msg = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        if msg.contains("unknown op") {
                            println!(
                                "gremlins daemon too old to report its version (CLI is {})",
                                gremlins::VERSION,
                            );
                        } else {
                            println!("gremlins daemon error: {msg}");
                        }
                    } else {
                        let daemon_version = resp
                            .get("version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        if daemon_version == "unknown" {
                            println!("gremlins daemon version unknown");
                        } else {
                            println!("gremlins daemon {}", daemon_version);
                        }
                    }
                }
                Err(_) => println!("no daemon running"),
            }
        }
        Err(_) => println!("no daemon running"),
    }

    Ok(())
}

/// Run the executor daemon.
///
/// Called via `gremlins serve <lock_fd>` (a hidden subcommand, spawned as a
/// detached child by the first CLI that claims the executor lock).
/// Reconstructs the lock file from the inherited fd, binds the socket, and
/// runs the supervisor accept loop until the last gremlin drains.
async fn serve_daemon(lock_fd: i32) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    let state_root = config::state_root();

    // Validate the inherited fd and reconstruct the lock. When this
    // process exits the lock file descriptor closes and the kernel
    // releases the lock.
    let _lock = GremlinsDaemonLock::from_inherited_fd(lock_fd, &state_root)?;

    let listener = socket::bind_socket(&state_root)?;

    // Signal the parent CLI that we are ready. This must be the first
    // and only output on stdout — the parent reads this line as a
    // deterministic readiness signal.
    println!("ready");

    // Redirect stderr to the executor log file so debug logs persist.
    // After this point all log output goes to the log file.
    let log_path = state_root.join("executor.log");
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::os::fd::IntoRawFd;
        let fd = file.into_raw_fd();
        unsafe { libc::dup2(fd, 2) };
        unsafe { libc::close(fd) };
    }

    log::info!(
        "executor: listening on {}",
        socket::socket_path(&state_root).display()
    );

    supervisor::run_supervisor(listener, state_root, _lock).await;
    Ok(())
}

/// Check if a response is an error.
fn check_error(response: &Value) -> Result<(), String> {
    if response.get("type").and_then(|v| v.as_str()) == Some("error") {
        let msg = response
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(msg.to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ls & status
// ---------------------------------------------------------------------------

async fn ls(here: bool) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    let cwd = std::env::current_dir()
        .map(|path| path.canonicalize().unwrap_or(path))
        .unwrap_or_else(|_| PathBuf::from("."));

    // Try the executor first — but only connect, don't become one.
    let response = match spawn::connect().await {
        Ok(mut stream) => {
            spawn::check_version(&mut stream).await?;
            match spawn::send_request(&mut stream, serde_json::json!({"op": "ls"})).await {
                Ok(r) => r,
                Err(_) => return ls_direct(here, &cwd),
            }
        }
        Err(_) => return ls_direct(here, &cwd),
    };

    check_error(&response)?;

    let gremlins = response
        .get("gremlins")
        .and_then(|v| v.as_array())
        .ok_or("invalid ls response")?;

    let headers = [
        "ID",
        "STATUS",
        "STAGE",
        "DATE",
        "PROJECT",
        "LAUNCH",
        "TOKENS IN",
        "TOKENS OUT",
        "REASONING",
        "CACHE",
        "NEW",
        "TURNS",
        "COST",
    ];
    let mut rows: Vec<Vec<String>> = Vec::new();

    for entry in gremlins {
        let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let status = entry.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let stage = entry.get("stage").and_then(|v| v.as_str()).unwrap_or("");
        let started_at = entry
            .get("started_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let project = entry.get("project").and_then(|v| v.as_str()).unwrap_or("");
        let launch = entry.get("launch").and_then(|v| v.as_str()).unwrap_or("");

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

        let prompt_tokens = entry.get("prompt_tokens").and_then(|v| v.as_u64());
        let completion_tokens = entry.get("completion_tokens").and_then(|v| v.as_u64());
        let reasoning_tokens = entry.get("reasoning_tokens").and_then(|v| v.as_u64());
        let cached_input_tokens = entry.get("cached_input_tokens").and_then(|v| v.as_u64());
        let cache_creation_input_tokens = entry
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64());
        let turns = entry.get("turns").and_then(|v| v.as_u64());
        let total_cost_usd = entry.get("cost_usd").and_then(|v| v.as_f64());

        rows.push(vec![
            id.to_string(),
            status.to_string(),
            stage.to_string(),
            started_at.to_string(),
            project.to_string(),
            launch.to_string(),
            crate::tui::format_token_count(prompt_tokens),
            crate::tui::format_token_count(completion_tokens),
            crate::tui::format_token_count(reasoning_tokens),
            crate::tui::format_cache_pct(prompt_tokens, cached_input_tokens),
            crate::tui::format_token_count(cache_creation_input_tokens),
            turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
            crate::tui::format_cost_usd(total_cost_usd),
        ]);
    }

    rows.sort_by(|a, b| a[3].cmp(&b[3]));
    print_table(&headers, &rows);
    Ok(())
}

/// Fallback: scan state directories directly (no executor running).
fn ls_direct(here: bool, cwd: &Path) -> Result<(), String> {
    let headers = [
        "ID",
        "STATUS",
        "STAGE",
        "DATE",
        "PROJECT",
        "LAUNCH",
        "TOKENS IN",
        "TOKENS OUT",
        "REASONING",
        "CACHE",
        "NEW",
        "TURNS",
        "COST",
    ];
    let mut rows: Vec<Vec<String>> = Vec::new();

    for (id, state_json_path) in state::list_state_dirs() {
        let Some(state_dir) = state_json_path.parent() else {
            continue;
        };
        if state_dir.join("closed").is_file() {
            continue;
        }

        let Some(state_map) = read_state_object(&state_json_path) else {
            continue;
        };

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
            if project_path != *cwd {
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

        let token_usage = state_map.get("token_usage").and_then(|v| v.as_object());
        let prompt_tokens = token_usage
            .and_then(|tu| tu.get("prompt_tokens"))
            .and_then(|v| v.as_u64());
        let completion_tokens = token_usage
            .and_then(|tu| tu.get("completion_tokens"))
            .and_then(|v| v.as_u64());
        let reasoning_tokens = token_usage
            .and_then(|tu| tu.get("reasoning_tokens"))
            .and_then(|v| v.as_u64());
        let cached_input_tokens = token_usage
            .and_then(|tu| tu.get("cached_input_tokens"))
            .and_then(|v| v.as_u64());
        let cache_creation_input_tokens = token_usage
            .and_then(|tu| tu.get("cache_creation_input_tokens"))
            .and_then(|v| v.as_u64());
        let turns = token_usage
            .and_then(|tu| tu.get("turns"))
            .and_then(|v| v.as_u64());
        let total_cost_usd = state_map.get("total_cost_usd").and_then(|v| v.as_f64());

        rows.push(vec![
            id,
            direct_status_display(&state_map, "status"),
            map_field_display(&state_map, "stage"),
            map_field_display(&state_map, "started_at"),
            project.to_string(),
            launch,
            crate::tui::format_token_count(prompt_tokens),
            crate::tui::format_token_count(completion_tokens),
            crate::tui::format_token_count(reasoning_tokens),
            crate::tui::format_cache_pct(prompt_tokens, cached_input_tokens),
            crate::tui::format_token_count(cache_creation_input_tokens),
            turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
            crate::tui::format_cost_usd(total_cost_usd),
        ]);
    }

    rows.sort_by(|a, b| a[3].cmp(&b[3]));
    print_table(&headers, &rows);
    Ok(())
}

/// The fallback arm for `gremlins <id>`: an unknown subcommand is exactly the
/// single-id status command, provided it was given exactly one token.
async fn status_external(args: &[OsString]) -> Result<(), String> {
    // Check if the first arg looks like a mistyped subcommand — do this
    // before the arity check so single-token typos (e.g. `gremlins lnch`)
    // get a suggestion instead of being treated as a gremlin ID.
    if let Some(first) = args.first() {
        let first = first.to_string_lossy();
        if let Some(closest) = closest_subcommand(&first) {
            return Err(format!(
                "unknown subcommand \"{first}\" — did you mean \"{closest}\"?"
            ));
        }
    }
    if args.len() != 1 {
        return Err("expected exactly one gremlin id".to_string());
    }
    let id = args[0].to_string_lossy();
    status(&id).await
}

/// Known subcommand names (including hidden `serve`).
const KNOWN_SUBCOMMANDS: &[&str] = &[
    "launch", "ls", "info", "stop", "resume", "log", "debug", "clean", "rm", "land", "serve",
];

/// If `arg` looks like a mistyped subcommand, return the closest match by
/// Levenshtein distance (threshold ≤ 3). Returns `None` when the arg contains
/// a `/` (looks like a path), is longer than 16 chars, matches a known
/// subcommand exactly, or has no close match.
fn closest_subcommand(arg: &str) -> Option<String> {
    if arg.contains('/') {
        return None;
    }
    if arg.len() > 16 {
        return None;
    }
    if KNOWN_SUBCOMMANDS.contains(&arg) {
        return None;
    }
    let (closest, dist) = KNOWN_SUBCOMMANDS
        .iter()
        .map(|&cmd| (cmd, levenshtein(arg, cmd)))
        .min_by_key(|&(_, d)| d)
        .unwrap();
    if dist <= 3 {
        Some(closest.to_string())
    } else {
        None
    }
}

/// Compute the Levenshtein (edit) distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();

    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr: Vec<usize> = vec![0; m + 1];

    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[m]
}

/// Print the detailed status block for one gremlin.
async fn status(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    // Try executor first — but only connect, don't become one.
    let response = match spawn::connect().await {
        Ok(mut stream) => {
            spawn::check_version(&mut stream).await?;
            match spawn::send_request(&mut stream, serde_json::json!({"op": "status", "id": id}))
                .await
            {
                Ok(r) => r,
                Err(_) => return status_direct(id),
            }
        }
        Err(_) => return status_direct(id),
    };

    check_error(&response)?;

    println!(
        "id:            {}",
        response.get("id").and_then(|v| v.as_str()).unwrap_or("")
    );
    println!(
        "status:        {}",
        response
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    println!(
        "stage:         {}",
        response.get("stage").and_then(|v| v.as_str()).unwrap_or("")
    );
    println!(
        "definition:    {}",
        response
            .get("definition")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    println!(
        "project_root:  {}",
        response
            .get("project_root")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    println!(
        "workdir:       {}",
        response
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    println!(
        "state_dir:     {}",
        response
            .get("state_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    println!(
        "artifact_dir:  {}",
        response
            .get("artifact_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    println!(
        "started_at:    {}",
        response
            .get("started_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    println!("ended_at:      {}", value_display(response.get("ended_at")));
    println!(
        "exit_code:     {}",
        value_display(response.get("exit_code"))
    );
    println!("pid:           {}", value_display(response.get("pid")));
    println!(
        "model:        {}",
        response.get("model").and_then(|v| v.as_str()).unwrap_or("")
    );
    println!(
        "attempt:       {}",
        response
            .get("attempt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    println!(
        "kind:          {}",
        response.get("kind").and_then(|v| v.as_str()).unwrap_or("")
    );
    Ok(())
}

/// Fallback: read status directly from state.json.
fn status_direct(id: &str) -> Result<(), String> {
    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let gremlin = Gremlin::from(id).map_err(|e| format!("gremlin {id}: {e}"))?;

    println!("id:            {}", gremlin.id);
    println!(
        "status:        {}",
        direct_status_display_value(gremlin.state.read_field("status").as_ref())
    );
    println!("stage:         {}", field_display(&gremlin.state, "stage"));
    println!("definition:    {}", definition_display_name(&gremlin.state));
    println!("project_root:  {}", gremlin.project_root.display());
    println!(
        "workdir:       {}",
        gremlin
            .workdir
            .as_ref()
            .map(|w| w.path().display().to_string())
            .unwrap_or_default()
    );
    println!("state_dir:     {}", config::state_root().join(id).display());
    println!(
        "artifact_dir:  {}",
        config::state_root().join(id).join("artifacts").display()
    );
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
    println!("model:        {}", field_display(&gremlin.state, "model"));
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

async fn info(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    // Try executor first — but only connect, don't become one.
    let response = match spawn::connect().await {
        Ok(mut stream) => {
            spawn::check_version(&mut stream).await?;
            match spawn::send_request(&mut stream, serde_json::json!({"op": "info", "id": id}))
                .await
            {
                Ok(r) => r,
                Err(_) => return info_direct(id),
            }
        }
        Err(_) => return info_direct(id),
    };

    check_error(&response)?;

    println!(
        "{}",
        serde_json::to_string_pretty(&response).map_err(|e| format!("failed to serialize: {e}"))?
    );
    Ok(())
}

fn info_direct(id: &str) -> Result<(), String> {
    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins show` to list gremlins"
        ));
    }

    let gremlin = Gremlin::from(id).map_err(|e| format!("gremlin {id}: {e}"))?;

    let workdir = gremlin
        .workdir
        .as_ref()
        .map(|w| w.path().display().to_string())
        .unwrap_or_default();

    let payload = serde_json::json!({
        "id": gremlin.id.as_str(),
        "status": direct_status_display_value(Some(&gremlin.state.read_field("status").unwrap_or(Value::Null))),
        "stage": gremlin.state.read_str("stage"),
        "definition": definition_display_name(&gremlin.state),
        "project_root": gremlin.project_root.display().to_string(),
        "workdir": workdir,
        "state_dir": config::state_root().join(id).display().to_string(),
        "artifact_dir": config::state_root().join(id).join("artifacts").display().to_string(),
        "scratch_dir": config::scratch_root(Some(id)).display().to_string(),
        "log_file": config::state_root().join(id).join("log").display().to_string(),
        "started_at": gremlin.state.read_str("started_at"),
        "ended_at": gremlin.state.read_field("ended_at").unwrap_or(Value::Null),
        "exit_code": gremlin.state.read_field("exit_code").unwrap_or(Value::Null),
        "pid": gremlin.state.read_field("pid").unwrap_or(Value::Null),
        "model": gremlin.state.read_str("model"),
        "attempt": gremlin.state.read_str("attempt"),
        "kind": gremlin.state.read_str("kind"),
        "token_usage": gremlin.state.read_field("token_usage").unwrap_or(Value::Null),
        "total_cost_usd": gremlin.state.read_field("total_cost_usd").unwrap_or(Value::Null),
        "bail_info": gremlin
            .state
            .stage_error()
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

async fn stop(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let response = executor_request(serde_json::json!({"op": "stop", "id": id})).await?;
    check_error(&response)?;

    let status = response
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let message = response
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !message.is_empty() {
        println!("gremlin {id}: {message}");
    } else {
        println!("gremlin {id} {status}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// resume
// ---------------------------------------------------------------------------

async fn resume(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let response = executor_request(serde_json::json!({"op": "resume", "id": id})).await?;
    check_error(&response)?;

    println!("{id}");
    Ok(())
}

/// Stream a gremlin's log over the executor socket.
///
/// When stdout is a terminal, `follow:true` keeps the stream open (like `tail -f`).
/// When stdout is a pipe, `follow:false` dumps existing lines and exits.
async fn log_gremlin(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let follow = std::io::stdout().is_terminal();

    let mut stream = match spawn::connect().await {
        Ok(s) => s,
        Err(_) => {
            return Err("no executor running — start one with `gremlins launch ...`".to_string())
        }
    };

    spawn::check_version(&mut stream).await?;

    let request = serde_json::json!({"op": "log", "id": id, "follow": follow});
    socket::write_json_line(&mut stream, &request).await?;

    let mut reader = tokio::io::BufReader::new(&mut stream);
    while let Some(line) = socket::read_json_line(&mut reader).await? {
        if line.get("type").and_then(|v| v.as_str()) == Some("error") {
            let msg = line
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(msg.to_string());
        }
        if let Some(text) = line.get("line").and_then(|v| v.as_str()) {
            println!("{text}");
        }
    }

    Ok(())
}

/// Interactive debug session for a running gremlin.
///
/// Connects to the executor socket, sends `{"op": "debug", "id": "..."}`,
/// and enters an interactive line-based loop. Plain text becomes `{"op": "talk", "text": "..."}`.
/// Slash commands: `/continue`, `/bail <reason>`, `/quit`.
async fn debug_gremlin(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let mut stream = match spawn::connect().await {
        Ok(s) => s,
        Err(_) => {
            return Err("no executor running — start one with `gremlins launch ...`".to_string())
        }
    };

    spawn::check_version(&mut stream).await?;

    let request = serde_json::json!({"op": "debug", "id": id});
    log::debug!("debug: sending request: {request}");
    socket::write_json_line(&mut stream, &request).await?;
    log::debug!("debug: request sent, waiting for agent to pause…");

    eprintln!("debug: waiting for agent to pause…");

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);

    // Spawn a blocking stdin reader thread so we can watch for /quit
    // during the ready-wait phase without blocking the async runtime.
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            match std::io::BufRead::read_line(&mut stdin.lock(), &mut line_buf) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line_buf.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if stdin_tx.send(trimmed).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Wait for ready, but also watch for /quit from stdin.
    // Use a channel-based socket reader so the read future is never
    // dropped when stdin input wins the select — no buffered data is lost.
    let (sock_tx, mut sock_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<Option<Value>, String>>();
    tokio::spawn(async move {
        loop {
            let result = socket::read_json_line(&mut reader).await;
            let done = result.as_ref().ok().is_none_or(|o| o.is_none());
            let _ = sock_tx.send(result);
            if done {
                break;
            }
        }
    });

    let mut stdin_closed = false;
    loop {
        let line = if stdin_closed {
            match sock_rx.recv().await {
                Some(Ok(v)) => v,
                Some(Err(e)) => return Err(e),
                None => return Err("connection closed before ready".to_string()),
            }
        } else {
            tokio::select! {
                sock_result = sock_rx.recv() => {
                    match sock_result {
                        Some(Ok(v)) => v,
                        Some(Err(e)) => return Err(e),
                        None => return Err("connection closed before ready".to_string()),
                    }
                }
                stdin_line = stdin_rx.recv() => {
                    match stdin_line {
                        Some(l) if l == "/quit" => {
                            let cmd = serde_json::json!({"op": "bail", "reason": "operator stopped"});
                            let _ = socket::write_json_line(&mut write_half, &cmd).await;
                            eprintln!("debug: cancelled");
                            return Ok(());
                        }
                        Some(l) if l.starts_with("/quit ") => {
                            let reason = l.strip_prefix("/quit ").unwrap().trim();
                            let cmd = serde_json::json!({"op": "bail", "reason": reason});
                            let _ = socket::write_json_line(&mut write_half, &cmd).await;
                            eprintln!("debug: cancelled");
                            return Ok(());
                        }
                        Some(l) if l == "/continue" || l == "/exit" => {
                            let cmd = serde_json::json!({"op": "quit"});
                            let _ = socket::write_json_line(&mut write_half, &cmd).await;
                            eprintln!("debug: resumed");
                            return Ok(());
                        }
                        Some(_) => continue, // discard other input during wait
                        None => {
                            stdin_closed = true;
                            continue;
                        }
                    }
                }
            }
        };

        match line {
            Some(ref l) if l.get("type").and_then(|v| v.as_str()) == Some("error") => {
                let msg = l
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                log::debug!("debug: received error: {msg}");
                return Err(msg.to_string());
            }
            Some(ref l) if l.get("type").and_then(|v| v.as_str()) == Some("ready") => {
                log::debug!("debug: received ready for {id}");
                eprintln!("debug: connected to gremlin {id}");
                break;
            }
            Some(ref other) => {
                if other.get("type").and_then(|v| v.as_str()) == Some("status") {
                    let stage = other.get("stage").and_then(|v| v.as_str()).unwrap_or("?");
                    eprintln!("debug: daemon status: {stage}");
                    continue;
                }
                log::debug!("debug: ignoring message during ready-wait: {other}");
                continue;
            }
            None => {
                log::debug!("debug: connection closed before ready");
                return Err("connection closed before ready".to_string());
            }
        }
    }

    log::debug!("debug: connected to {id}, entering interactive loop");

    // Tail the last ~20 lines of the gremlin log for immediate context.
    // Read only the tail of the file (append-only log, no rotation) so
    // startup time and memory stay bounded regardless of log length.
    let log_path = config::state_root().join(id).join("log");
    if let Ok(lines) = tail_log_lines(&log_path, 20).await {
        for line in &lines {
            eprintln!("log: {line}");
        }
    }

    // Drain any stdin lines queued during the ready-wait phase so they
    // don't leak into the active-session loop as spurious commands.
    while stdin_rx.try_recv().is_ok() {}

    // Spawn a reader task to print agent events from the socket channel.
    let read_handle = tokio::spawn(async move {
        while let Some(Ok(Some(line))) = sock_rx.recv().await {
            let typ = line.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match typ {
                "turn_complete" => {
                    let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let tool_calls = line.get("tool_calls").and_then(|v| v.as_array());
                    if !text.is_empty() {
                        eprintln!("{text}");
                    }
                    if let Some(tcs) = tool_calls {
                        for tc in tcs {
                            if let Some(name) = tc.as_str() {
                                eprintln!("  [tool: {name}]");
                            }
                        }
                    }
                    eprintln!("debug: turn complete — agent paused");
                }
                "done" => {
                    eprintln!("debug: agent called Done");
                }
                "ended" => {
                    let reason = line.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                    eprintln!("debug: session ended ({reason})");
                    break;
                }
                "stream_chunk" => {
                    // Stream chunks are silently consumed in the debug CLI path;
                    // the full text arrives in turn_complete.
                }
                "error" => {
                    let msg = line.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    eprintln!("debug: error: {msg}");
                    break;
                }
                _ => {}
            }
        }
    });

    // Read stdin lines from the channel and forward to the socket.
    loop {
        let trimmed = match stdin_rx.recv().await {
            Some(line) => line,
            None => {
                let cmd = serde_json::json!({"op": "quit"});
                socket::write_json_line(&mut write_half, &cmd).await?;
                break;
            }
        };

        let cmd = if trimmed == "/continue" || trimmed == "/exit" {
            serde_json::json!({"op": "quit"})
        } else if let Some(rest) = trimmed.strip_prefix("/quit ") {
            serde_json::json!({"op": "bail", "reason": rest.trim()})
        } else if trimmed == "/quit" {
            serde_json::json!({"op": "bail", "reason": "operator stopped"})
        } else if trimmed == "/step" {
            serde_json::json!({"op": "continue"})
        } else if trimmed == "/help" {
            eprintln!("available commands:");
            eprintln!("  /quit [reason]  — bail the stage (default: operator stopped)");
            eprintln!("  /continue       — exit interactive mode, agent resumes");
            eprintln!("  /exit           — alias for /continue");
            eprintln!("  /step           — run one turn, then re-pause");
            eprintln!("  /help           — show this help");
            eprintln!("  <anything else> — send message to agent");
            continue;
        } else {
            serde_json::json!({"op": "talk", "text": trimmed})
        };

        let is_terminal = trimmed == "/quit"
            || trimmed.starts_with("/quit ")
            || trimmed == "/continue"
            || trimmed == "/exit";
        socket::write_json_line(&mut write_half, &cmd).await?;
        if is_terminal {
            break;
        }
    }

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), read_handle).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

/// Remove a single gremlin by id.
async fn rm_one(id: &str) -> Result<(), String> {
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
        match is_live_in_executor(id).await {
            Ok(true) => {
                return Err(format!(
                    "gremlin {id} is running — use `gremlins stop {id}` first"
                ));
            }
            Err(e) => return Err(e),
            Ok(false) => {}
        }
    }

    // Load clean_cmds from the run's hermetic definition snapshot — no
    // bootstrap.env sourcing, no client build, no stage traversal. Clean
    // commands must run even when the full runtime init would fail.
    let mut gremlin = gremlin;
    if let Err(e) = gremlin.attach_clean_cmds() {
        log::warn!("gremlin {id}: could not load definition for clean commands: {e}");
    }

    gremlin.clean(true).await;
    println!("gremlin {id} removed");
    Ok(())
}

/// Remove every gremlin in a terminal state (done/stopped/orphaned).
async fn rm_all() -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    let entries = state::list_state_dirs();
    if entries.is_empty() {
        return Ok(());
    }

    let mut removed = 0usize;
    let mut skipped = 0usize;
    let mut executor_reachable: Option<bool> = None;

    for (id, state_json_path) in &entries {
        let (outcome, next_reachable) = rm_all_entry(id, state_json_path, executor_reachable).await;
        executor_reachable = next_reachable;
        match outcome {
            RmAllEntry::Removed => removed += 1,
            RmAllEntry::Skipped(msg) => {
                println!("gremlin {id}: skipped ({msg})");
                skipped += 1;
            }
            RmAllEntry::Ignored => {}
        }
    }

    if removed > 0 || skipped > 0 {
        log::info!("rm --all: {removed} removed, {skipped} skipped");
    }
    Ok(())
}

/// Per-entry outcome for `rm --all`.
enum RmAllEntry {
    /// Entry was removed.
    Removed,
    /// Entry was skipped with a reason.
    Skipped(String),
    /// Entry was not eligible (closed marker, unknown status, etc.) — no message.
    Ignored,
}

/// Evaluate a single gremlin entry for `rm --all` eligibility and, if eligible,
/// remove it. Returns the outcome and the (possibly updated) executor
/// reachability flag so the caller can thread it across iterations.
async fn rm_all_entry(
    id: &str,
    state_json_path: &Path,
    mut executor_reachable: Option<bool>,
) -> (RmAllEntry, Option<bool>) {
    let Some(state_dir) = state_json_path.parent() else {
        return (RmAllEntry::Ignored, executor_reachable);
    };

    // Skip entries that have already been `clean --keep`'d.
    if state_dir.join("closed").is_file() {
        return (RmAllEntry::Ignored, executor_reachable);
    }

    // Read status from state.json.
    let state_map = read_state_object(state_json_path);
    let status = state_map
        .as_ref()
        .and_then(|m| m.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("");

    match status {
        "done" | "stopped" => {
            // Terminal — safe to remove.
        }
        "running" => {
            // Lazily check executor reachability on first running entry.
            let reachable = match executor_reachable {
                Some(r) => r,
                None => {
                    let r = spawn::connect().await.is_ok();
                    executor_reachable = Some(r);
                    r
                }
            };
            if reachable {
                match is_live_in_executor(id).await {
                    Ok(true) => {
                        return (
                            RmAllEntry::Skipped("still running".to_string()),
                            executor_reachable,
                        );
                    }
                    Err(e) => {
                        return (RmAllEntry::Skipped(e), executor_reachable);
                    }
                    Ok(false) => {
                        // Orphaned — safe to remove.
                    }
                }
            } else {
                return (
                    RmAllEntry::Skipped("executor unreachable".to_string()),
                    executor_reachable,
                );
            }
        }
        _ => {
            // Unknown or empty status — skip.
            return (RmAllEntry::Ignored, executor_reachable);
        }
    }

    // Delegate to the single-id helper. Errors are logged, not fatal.
    match rm_one(id).await {
        Ok(()) => (RmAllEntry::Removed, executor_reachable),
        Err(e) => (RmAllEntry::Skipped(e), executor_reachable),
    }
}

/// Check whether a gremlin is truly live in the executor's run_map.
/// Returns `Ok(true)` if live, `Ok(false)` if confirmed not live,
/// or `Err(...)` if the executor is unreachable and liveness cannot be determined.
async fn is_live_in_executor(id: &str) -> Result<bool, String> {
    let mut stream = spawn::connect()
        .await
        .map_err(|e| format!("cannot reach executor daemon to verify gremlin {id}: {e}"))?;
    let response = spawn::send_request(&mut stream, serde_json::json!({"op": "status", "id": id}))
        .await
        .map_err(|e| format!("executor request failed while checking gremlin {id}: {e}"))?;
    Ok(response.get("status").and_then(|v| v.as_str()) == Some("running"))
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

async fn clean(id: &str, keep: bool) -> Result<(), String> {
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
                println!("gremlin {id} is already cleaned");
                return Ok(());
            }
            return Err(format!("gremlin {id}: {e}"));
        }
    };

    let status = gremlin.state.read_str("status");
    if status == "running" {
        match is_live_in_executor(id).await {
            Ok(true) => {
                return Err(format!(
                    "gremlin {id} is running — use `gremlins stop {id}` first"
                ));
            }
            Err(e) => return Err(e),
            Ok(false) => {}
        }
    }

    // Load clean_cmds from the run's hermetic definition snapshot — no
    // bootstrap.env sourcing, no client build, no stage traversal. Clean
    // commands must run even when the full runtime init would fail.
    let mut gremlin = gremlin;
    if let Err(e) = gremlin.attach_clean_cmds() {
        log::warn!("gremlin {id}: could not load definition for clean commands: {e}");
    }

    gremlin.clean(!keep).await;
    println!("gremlin {id} cleaned");
    Ok(())
}

// ---------------------------------------------------------------------------
// land
// ---------------------------------------------------------------------------

async fn land(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    let state_dir = config::state_root().join(id);
    if !state_dir.is_dir() || !state_dir.join("state.json").is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let default_model = config::global_config()
        .ok()
        .and_then(|c| c.default_model().map(String::from));
    let store = FileSystemStateStore::from_path(state_dir.to_path_buf());

    // Load the hermetic definition through the store blob.
    let definition = {
        let mut blob = store
            .open("definition.yaml", BlobMode::Read)
            .map_err(|e| format!("gremlin {id}: definition snapshot not found: {e}"))?;
        use std::io::Read;
        let mut bytes = Vec::new();
        blob.read_to_end(&mut bytes)
            .map_err(|e| format!("gremlin {id}: failed to read definition snapshot: {e}"))?;
        StaticDefinition::from_expanded_bytes(&bytes, None, default_model.as_deref())
            .map_err(|e| format!("gremlin {id}: failed to load definition: {e}"))?
    };

    let exec = match definition.land() {
        Some(ExecutorStage::Exec { stage, .. }) => stage,
        Some(_) => {
            return Err(format!(
                "gremlin {id}: land stage is not an exec (internal error)"
            ));
        }
        None => {
            return Err(format!("gremlin {id}: definition has no land block"));
        }
    };

    let raw = state::read_state_json(Some(&state_dir.join("state.json")));

    if raw.get("status").and_then(Value::as_str) == Some("running") {
        match is_live_in_executor(id).await {
            Ok(true) => {
                return Err(format!(
                    "gremlin {id} is running — use `gremlins stop {id}` first"
                ));
            }
            Err(e) => return Err(e),
            Ok(false) => {}
        }
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

    let prepared = prepare_exec(&exec, &store, &store, "", &HashMap::new(), None)
        .await
        .map_err(|e| format!("gremlin {id}: {e}"))?;

    if prepared.cmds.is_empty() {
        return Err(format!("gremlin {id}: land block has no commands"));
    }

    let joined = prepared.cmds.join(" && ");
    let cwd =
        std::env::current_dir().map_err(|e| format!("failed to get current directory: {e}"))?;

    let mut env: HashMap<String, String> = std::env::vars().collect();
    let scratch_dir = config::scratch_root(Some(id));
    env.extend(system_env(
        id,
        &project_root,
        worktree.as_deref(),
        &scratch_dir,
    ));
    for (k, v) in &prepared.substitution_env {
        env.insert(k.clone(), v.clone());
    }

    let result = run_shell_async(
        &joined,
        Some(&cwd),
        Some(&env),
        prepared.timeout,
        None,
        None,
    )
    .await
    .map_err(|e| format!("gremlin {id}: land: {e}"))?;

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

/// Print a bundled (self-contained) YAML for the given definition to stdout.
async fn bundle(definition: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;
    let project_root = config::project_root();
    let definition_path = discovery::resolve_definition_path(definition, project_root.clone())
        .map_err(|e| format!("definition not found: {e}"))?;

    let bundled = gremlins::definition::bundle_definition(&definition_path, &project_root)
        .map_err(|e| format!("failed to bundle: {e}"))?;

    let yaml_str =
        serde_yaml::to_string(&bundled).map_err(|e| format!("failed to serialize: {e}"))?;

    print!("{yaml_str}");
    Ok(())
}
// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// In the direct/fallback paths the executor isn't running, so any gremlin
/// whose state.json says "running" is definitively orphaned.
fn direct_status_display(state: &serde_json::Map<String, Value>, field: &str) -> String {
    direct_status_display_value(state.get(field))
}

fn direct_status_display_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) if s == "running" => "orphan".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

fn field_display(state: &StateData, field: &str) -> String {
    value_display(state.read_field(field).as_ref())
}

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

fn read_state_object(path: &Path) -> Option<serde_json::Map<String, Value>> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    match value {
        Value::Object(map) if !map.is_empty() => Some(map),
        _ => None,
    }
}

fn definition_display_name(state: &StateData) -> String {
    let recorded = field_display(state, "definition_path");
    if let Some(stem) = Path::new(&recorded)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
    {
        return stem.to_string();
    }
    field_display(state, "kind")
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.len());
            }
        }
    }

    let last = widths.len().saturating_sub(1);

    let header_line = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            if index == last {
                header.to_string()
            } else {
                let width = widths.get(index).copied().unwrap_or(0);
                format!("{header:<width$}")
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    println!("{header_line}");

    for row in rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                if index == last {
                    cell.clone()
                } else {
                    let width = widths.get(index).copied().unwrap_or(0);
                    format!("{cell:<width$}")
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        println!("{line}");
    }
}

// ---------------------------------------------------------------------------
// launch
// ---------------------------------------------------------------------------

async fn launch(
    definition: &str,
    mut raw_args: Vec<String>,
    ephemeral: bool,
    cli_model: Option<String>,
) -> Result<(), String> {
    // `--model` is a reserved launch option, but the free-form bootstrap-arg
    // list is a trailing var-arg, so clap captures `--model` there once any
    // bootstrap input has been seen. Pull it back out before bootstrap
    // parsing so `gremlins launch demo --goal text --model cmd/true` works.
    let model_override = extract_model_override(&mut raw_args, cli_model)?;
    let model_override = model_override.as_deref();

    // Parse --key value pairs up front so malformed trailing args fail fast,
    // before any config or definition resolution.
    parse_stage_inputs(&raw_args)?;

    // Bootstrap the global config so path resolvers work.
    config::init_global().map_err(|e| e.to_string())?;

    // Resolve the definition to a definition path.
    let project_root = config::project_root();
    let definition_path = discovery::resolve_definition_path(definition, project_root.clone())
        .map_err(|e| format!("definition not found: {e}"))?;

    // Load the definition just enough to validate --key args against
    // bootstrap.source.
    let default_model = config::global_config()
        .ok()
        .and_then(|c| c.default_model().map(String::from));
    let gremlin_def = StaticDefinition::from_yaml_file(
        &definition_path,
        model_override,
        default_model.as_deref(),
    )
    .map_err(|e| format!("invalid definition: {e}"))?;

    // Resolve `filepath`-typed bootstrap sources to absolute paths so the
    // daemon can bind them regardless of its own working directory. The
    // precedence mirrors `gremlins::executor::bootstrap::bind_artifact`: the
    // invoking cwd wins, then `project_root`.
    let resolved_args = match &gremlin_def.bootstrap.source {
        Some(source) => {
            let cwd = std::env::current_dir().map_err(|e| format!("current directory: {e}"))?;
            resolve_filepath_args(&raw_args, source, &project_root, &cwd)
        }
        None => raw_args.to_vec(),
    };

    // Parse --key value pairs from the (possibly rewritten) free-form args,
    // so validation sees the same absolute paths sent to the daemon.
    let stage_inputs = parse_stage_inputs(&resolved_args)?;

    match &gremlin_def.bootstrap.source {
        Some(source) => {
            let declared: Vec<String> = source.all_sources();
            for key in stage_inputs.keys() {
                if !declared.iter().any(|d| d == key) {
                    return Err(format!(
                        "unknown input {key:?} — definition declares sources: {}",
                        declared.join(", ")
                    ));
                }
            }
            bootstrap::validate_source_values(source, &stage_inputs).map_err(|e| format!("{e}"))?;
        }
        None => {
            if !stage_inputs.is_empty() {
                return Err(
                    "definition declares no bootstrap.source — no --key args allowed".to_string(),
                );
            }
        }
    }

    // Send launch request to executor.
    let response = executor_request(serde_json::json!({
        "op": "launch",
        "definition": definition,
        "args": resolved_args,
        "project_root": project_root.to_string_lossy(),
        "ephemeral": ephemeral,
        "model": model_override,
    }))
    .await?;

    check_error(&response)?;

    let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("");
    println!("{id}");
    Ok(())
}

/// Rewrite `--key value` args so that `filepath`-typed bootstrap sources carry
/// absolute paths. Other args pass through verbatim, in order.
///
/// Resolution precedence mirrors `gremlins::executor::bootstrap::bind_artifact`:
/// an already-absolute path is kept as-is; otherwise the value is rooted at
/// `cwd` when that names an existing file, then at `project_root` when that
/// names an existing file. Values that resolve to no existing file are left
/// untouched so validation still reports the original `expected an existing
/// file` error.
fn resolve_filepath_args(
    args: &[String],
    source: &bootstrap::InputSources,
    project_root: &Path,
    cwd: &Path,
) -> Vec<String> {
    let mut resolved = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let token = &args[i];
        resolved.push(token.clone());

        if let Some(key) = token.strip_prefix("--") {
            let is_filepath = source
                .sources
                .get(key)
                .is_some_and(|src| src.types.iter().any(|t| t == "filepath"));
            if is_filepath && i + 1 < args.len() && !args[i + 1].starts_with("--") {
                i += 1;
                let value = &args[i];
                resolved.push(resolve_filepath_value(value, project_root, cwd));
            }
        }
        i += 1;
    }
    resolved
}

/// Resolve one `filepath` source value to an absolute path, mirroring
/// `bind_artifact`'s cwd-first, then `project_root` precedence. A relative
/// `project_root` is anchored to `cwd` before joining so the resulting path is
/// absolute even when `GREMLINS_PROJECT_ROOT` (or an equivalent override) is
/// relative.
fn resolve_filepath_value(value: &str, project_root: &Path, cwd: &Path) -> String {
    let path = Path::new(value);
    if path.is_absolute() {
        return value.to_string();
    }

    let from_cwd = cwd.join(value);
    if from_cwd.is_file() {
        return from_cwd.to_string_lossy().into_owned();
    }

    let from_project = absolute_from(project_root, cwd).join(value);
    if from_project.is_file() {
        return from_project.to_string_lossy().into_owned();
    }

    value.to_string()
}

/// Return `path` as an absolute path, joining it to `cwd` when it is relative.
/// An already-absolute path is returned unchanged.
fn absolute_from(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Remove the reserved `--model` option from the free-form bootstrap args.
///
/// Supports both `--model value` and `--model=value` spellings. The
/// `cli_model` value (if clap happened to parse the flag before the trailing
/// var-arg began) is used as the initial value, so supplying `--model` twice is
/// still caught.
fn extract_model_override(
    args: &mut Vec<String>,
    cli_model: Option<String>,
) -> Result<Option<String>, String> {
    let mut model = cli_model;
    let mut bootstrap_args = Vec::with_capacity(args.len());
    let mut i = 0;

    while i < args.len() {
        let token = &args[i];

        if let Some(value) = token.strip_prefix("--model=") {
            if model.is_some() {
                return Err("--model may only be supplied once".to_string());
            }
            if value.is_empty() {
                return Err("--model requires a value".to_string());
            }
            model = Some(value.to_string());
        } else if token == "--model" {
            if model.is_some() {
                return Err("--model may only be supplied once".to_string());
            }
            i += 1;
            let value = args
                .get(i)
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| "--model requires a value".to_string())?;
            model = Some(value.clone());
        } else {
            bootstrap_args.push(token.clone());
        }

        i += 1;
    }

    *args = bootstrap_args;
    Ok(model)
}

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

/// Read the last `n` lines from a file without loading the entire file.
/// Reads backwards in chunks, so cost is O(n) not O(file_size).
async fn tail_log_lines(path: &Path, n: usize) -> Result<Vec<String>, String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut f = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open log: {e}"))?;

    let file_len = f
        .metadata()
        .await
        .map_err(|e| format!("stat log: {e}"))?
        .len();

    if file_len == 0 {
        return Ok(Vec::new());
    }

    // Read the last chunk (up to 8 KiB) from the end of the file.
    let chunk_size = 8192usize.min(file_len as usize);
    let mut buf = vec![0u8; chunk_size];
    f.seek(std::io::SeekFrom::End(-(chunk_size as i64)))
        .await
        .map_err(|e| format!("seek log: {e}"))?;
    f.read_exact(&mut buf)
        .await
        .map_err(|e| format!("read log tail: {e}"))?;

    // If the chunk doesn't start at a line boundary, skip the partial first line.
    let start = if file_len > chunk_size as u64 {
        // We didn't read from the beginning, so the first byte may be mid-line.
        // Skip to the first newline.
        buf.iter()
            .position(|&b| b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0)
    } else {
        0
    };

    let text = std::str::from_utf8(&buf[start..]).map_err(|e| format!("utf-8: {e}"))?;
    let all_lines: Vec<&str> = text.lines().collect();
    let keep = if all_lines.len() > n {
        &all_lines[all_lines.len() - n..]
    } else {
        &all_lines
    };

    Ok(keep.iter().map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gremlins::schemas::bootstrap::{InputSource, InputSources};
    use std::fs;

    #[test]
    fn launch_parser_reserves_model_after_bootstrap_inputs() {
        let cli = Cli::try_parse_from([
            "gremlins", "launch", "demo", "--goal", "text", "--model", "cmd/true",
        ])
        .expect("launch args must parse");

        let Some(Cmds::Launch {
            definition,
            mut args,
            model,
            ..
        }) = cli.command
        else {
            panic!("expected launch command");
        };

        assert_eq!(definition, "demo");
        let model_override = extract_model_override(&mut args, model).unwrap();
        assert_eq!(model_override.as_deref(), Some("cmd/true"));
        assert_eq!(args, vec!["--goal".to_string(), "text".to_string()]);
    }

    #[test]
    fn extract_model_override_rejects_duplicate_model() {
        let mut args = vec![
            "--goal".to_string(),
            "text".to_string(),
            "--model".to_string(),
            "openai/gpt-4".to_string(),
            "--model=cmd/true".to_string(),
        ];
        let err = extract_model_override(&mut args, None).unwrap_err();
        assert!(err.contains("once"), "got: {err}");
    }

    fn inputs(plan_types: &[&str]) -> InputSources {
        let sources = [
            (
                "plan".to_string(),
                InputSource::new(
                    "plan".to_string(),
                    plan_types.iter().map(|t| (*t).to_string()).collect(),
                    true,
                )
                .unwrap(),
            ),
            (
                "instructions".to_string(),
                InputSource::new("instructions".to_string(), vec!["string".to_string()], true)
                    .unwrap(),
            ),
        ]
        .into_iter()
        .collect();
        InputSources::new(sources)
    }

    #[test]
    fn filepath_args_resolve_relative_to_cwd_first() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("cwd");
        let project_root = tmp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&project_root).unwrap();
        fs::write(cwd.join("plan.md"), "ctor plan").unwrap();
        fs::write(project_root.join("plan.md"), "project plan").unwrap();

        let args = resolve_filepath_args(
            &["--plan".to_string(), "plan.md".to_string()],
            &inputs(&["filepath"]),
            &project_root,
            &cwd,
        );
        assert_eq!(
            args,
            vec![
                "--plan".to_string(),
                cwd.join("plan.md").to_string_lossy().into_owned()
            ]
        );
    }

    #[test]
    fn filepath_args_fall_back_to_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("cwd");
        let project_root = tmp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(project_root.join("plans")).unwrap();
        fs::write(project_root.join("plans/plan.md"), "plan").unwrap();

        let args = resolve_filepath_args(
            &["--plan".to_string(), "plans/plan.md".to_string()],
            &inputs(&["filepath"]),
            &project_root,
            &cwd,
        );
        assert_eq!(
            args,
            vec![
                "--plan".to_string(),
                project_root
                    .join("plans/plan.md")
                    .to_string_lossy()
                    .into_owned()
            ]
        );
    }

    #[test]
    fn filepath_args_anchor_relative_project_root_to_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let project_root = PathBuf::from("project");
        fs::create_dir_all(cwd.join("project/plans")).unwrap();
        fs::write(cwd.join("project/plans/plan.md"), "plan").unwrap();

        let args = resolve_filepath_args(
            &["--plan".to_string(), "plans/plan.md".to_string()],
            &inputs(&["filepath"]),
            &project_root,
            &cwd,
        );
        assert_eq!(
            args,
            vec![
                "--plan".to_string(),
                cwd.join("project/plans/plan.md")
                    .to_string_lossy()
                    .into_owned()
            ]
        );
    }

    #[test]
    fn filepath_args_pass_through_absolute_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("cwd");
        let project_root = tmp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&project_root).unwrap();

        let args = resolve_filepath_args(
            &[
                "--plan".to_string(),
                "/already/absolute.md".to_string(),
                "--plan".to_string(),
                "nope.md".to_string(),
            ],
            &inputs(&["filepath"]),
            &project_root,
            &cwd,
        );
        assert_eq!(
            args,
            vec![
                "--plan".to_string(),
                "/already/absolute.md".to_string(),
                "--plan".to_string(),
                "nope.md".to_string(),
            ]
        );
    }

    #[test]
    fn filepath_args_leave_string_sources_and_ordering_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("cwd");
        let project_root = tmp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&project_root).unwrap();
        fs::write(cwd.join("plan.md"), "plan").unwrap();

        let args = resolve_filepath_args(
            &[
                "--instructions".to_string(),
                "write more tests".to_string(),
                "--plan".to_string(),
                "plan.md".to_string(),
                "--instructions".to_string(),
                "also refactor".to_string(),
            ],
            &inputs(&["filepath", "string"]),
            &project_root,
            &cwd,
        );
        assert_eq!(
            args,
            vec![
                "--instructions".to_string(),
                "write more tests".to_string(),
                "--plan".to_string(),
                cwd.join("plan.md").to_string_lossy().into_owned(),
                "--instructions".to_string(),
                "also refactor".to_string(),
            ]
        );
    }
}
