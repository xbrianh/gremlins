use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use gremlins::artifacts::registry::FileSystemArtifactRegistry;
use gremlins::config;
use gremlins::core::discovery;
use gremlins::core::proc::run_shell_async;
use gremlins::definition::{ExecutorStage, GremlinDefinition, StaticDefinition};
use gremlins::executor::exec_runner::prepare_exec;
use gremlins::executor::gremlin::{system_env, validate_gremlin_id, Gremlin};
use gremlins::executor::socket::{self, GremlinsDaemonLock};
use gremlins::executor::state::{self, StateData};
use gremlins::executor::supervisor;
use gremlins::schemas::bootstrap;
use serde_json::Value;

mod spawn;

#[derive(Parser)]
#[command(name = "gremlins", about = "AI-backed gremlin definition runner")]
struct Cli {
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
        id: String,
    },
    /// Run the definition's land block in the current working directory.
    Land {
        /// Gremlin id whose land block to run.
        id: String,
    },
    /// Validate a definition without executing it.
    Validate {
        /// Gremlin definition: a bare name (resolved under .gremlins/) or a path.
        definition: String,
    },
    #[command(hide = true, name = "serve")]
    Serve {
        /// Lock file descriptor (internal, inherited from parent).
        #[arg(hide = true)]
        lock_fd: i32,
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
        Some(Cmds::Ls { here }) => ls(here).await,
        Some(Cmds::Info { id }) => info(&id).await,
        Some(Cmds::Stop { id }) => stop(&id).await,
        Some(Cmds::Resume { id }) => resume(&id).await,
        Some(Cmds::Log { id }) => log_gremlin(&id).await,
        Some(Cmds::Debug { id }) => debug_gremlin(&id).await,
        Some(Cmds::Clean { id, keep }) => clean(&id, keep).await,
        Some(Cmds::Rm { id }) => rm(&id).await,
        Some(Cmds::Land { id }) => land(&id).await,
        Some(Cmds::Validate { definition }) => validate(&definition).await,
        Some(Cmds::Serve { lock_fd }) => serve_daemon(lock_fd).await,
        Some(Cmds::External(args)) => status_external(&args).await,
        None => {
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
// Socket helpers
// ---------------------------------------------------------------------------

/// Ensure an executor is running, becoming one if needed.
/// Returns a connected stream.
async fn ensure_executor() -> Result<tokio::net::UnixStream, String> {
    config::init_global().map_err(|e| e.to_string())?;
    spawn::bind_or_connect().await
}

/// Send a request to the executor and return the response.
async fn executor_request(request: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = ensure_executor().await?;
    spawn::send_request(&mut stream, request).await
}

/// Run the executor daemon.
///
/// Called via `gremlins serve <lock_fd>` (a hidden subcommand, spawned as a
/// detached child by the first CLI that claims the executor lock).
/// Reconstructs the lock file from the inherited fd, binds the socket, and
/// runs the supervisor accept loop until the last gremlin drains.
async fn serve_daemon(lock_fd: i32) -> Result<(), String> {
    let level = std::env::var("GREMLINS_LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&level))
        .format_timestamp_millis()
        .init();

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

    let headers = ["ID", "STATUS", "STAGE", "DATE", "PROJECT", "LAUNCH"];
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

        rows.push(vec![
            id.to_string(),
            status.to_string(),
            stage.to_string(),
            started_at.to_string(),
            project.to_string(),
            launch.to_string(),
        ]);
    }

    print_table(&headers, &rows);
    Ok(())
}

/// Fallback: scan state directories directly (no executor running).
fn ls_direct(here: bool, cwd: &Path) -> Result<(), String> {
    let headers = ["ID", "STATUS", "STAGE", "DATE", "PROJECT", "LAUNCH"];
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

        rows.push(vec![
            id,
            direct_status_display(&state_map, "status"),
            map_field_display(&state_map, "stage"),
            map_field_display(&state_map, "started_at"),
            project.to_string(),
            launch,
        ]);
    }

    print_table(&headers, &rows);
    Ok(())
}

/// The fallback arm for `gremlins <id>`: an unknown subcommand is exactly the
/// single-id status command, provided it was given exactly one token.
async fn status_external(args: &[OsString]) -> Result<(), String> {
    if args.len() != 1 {
        return Err("expected exactly one gremlin id".to_string());
    }
    let id = args[0].to_string_lossy();
    status(&id).await
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
        "client:        {}",
        response
            .get("client")
            .and_then(|v| v.as_str())
            .unwrap_or("")
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

async fn info(id: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    validate_gremlin_id(id).map_err(|_| {
        format!("invalid gremlin id {id:?} — ids may contain only letters, numbers, '-', and '_'")
    })?;

    // Try executor first — but only connect, don't become one.
    let response = match spawn::connect().await {
        Ok(mut stream) => {
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
        .worktree
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();

    let payload = serde_json::json!({
        "id": gremlin.id.as_str(),
        "status": direct_status_display_value(Some(&gremlin.state.read_field("status").unwrap_or(Value::Null))),
        "stage": gremlin.state.read_str("stage"),
        "definition": definition_display_name(&gremlin.state),
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

    let request = serde_json::json!({"op": "debug", "id": id});
    socket::write_json_line(&mut stream, &request).await?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);

    // Wait for debug_ready.
    loop {
        let line = socket::read_json_line(&mut reader).await?;
        match line {
            Some(ref l) if l.get("type").and_then(|v| v.as_str()) == Some("error") => {
                let msg = l
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Err(msg.to_string());
            }
            Some(ref l) if l.get("type").and_then(|v| v.as_str()) == Some("debug_ready") => {
                eprintln!("debug: connected to gremlin {id}");
                break;
            }
            Some(_) => continue,
            None => return Err("connection closed before debug_ready".to_string()),
        }
    }

    // Spawn a reader task to print agent events.
    let read_handle = tokio::spawn(async move {
        loop {
            match socket::read_json_line(&mut reader).await {
                Ok(Some(line)) => {
                    let typ = line.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match typ {
                        "debug_turn_complete" => {
                            eprintln!("debug: turn complete — agent paused");
                        }
                        "debug_paused" => {
                            eprintln!("debug: agent paused");
                        }
                        "debug_ended" => {
                            let reason = line.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                            eprintln!("debug: session ended ({reason})");
                            break;
                        }
                        "error" => {
                            let msg = line.get("message").and_then(|v| v.as_str()).unwrap_or("");
                            eprintln!("debug: error: {msg}");
                            break;
                        }
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });

    // Read stdin lines and forward to the socket.
    let stdin = std::io::stdin();
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        let n = std::io::BufRead::read_line(&mut stdin.lock(), &mut line_buf)
            .map_err(|e| format!("stdin: {e}"))?;
        if n == 0 {
            let cmd = serde_json::json!({"op": "quit"});
            socket::write_json_line(&mut write_half, &cmd).await?;
            break;
        }
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        let cmd = if let Some(_rest) = trimmed.strip_prefix("/continue") {
            serde_json::json!({"op": "continue"})
        } else if let Some(rest) = trimmed.strip_prefix("/bail") {
            let reason = rest.trim();
            serde_json::json!({"op": "bail", "reason": if reason.is_empty() { "operator bailed" } else { reason }})
        } else if trimmed == "/quit" {
            serde_json::json!({"op": "quit"})
        } else {
            serde_json::json!({"op": "talk", "text": trimmed})
        };

        let is_quit = trimmed == "/quit";
        socket::write_json_line(&mut write_half, &cmd).await?;
        if is_quit {
            break;
        }
    }

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), read_handle).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

async fn rm(id: &str) -> Result<(), String> {
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

    gremlin.clean(true);
    println!("gremlin {id} removed");
    Ok(())
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

    gremlin.clean(!keep);
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
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return Err(format!(
            "unknown gremlin {id:?} — use `gremlins ls` to list gremlins"
        ));
    }

    let definition_path = state_dir.join("definition.yaml");
    if !definition_path.is_file() {
        return Err(format!(
            "gremlin {id}: definition snapshot not found at {}",
            definition_path.display()
        ));
    }
    let default_client = config::global_config()
        .ok()
        .and_then(|c| c.default_client().map(String::from));
    let definition =
        StaticDefinition::from_yaml_file(&definition_path, None, default_client.as_deref())
            .map_err(|e| format!("gremlin {id}: failed to load definition: {e}"))?;

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

    let raw = state::read_state_json(Some(&state_file));

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
    let overlay_dir = config::project_overlay_dir(&project_root);

    let artifact_dir = state_dir.join("artifacts");
    let registry = FileSystemArtifactRegistry::new(artifact_dir.clone());

    let prepared = prepare_exec(&exec, &registry, &registry, "", &HashMap::new())
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
        &state_dir,
        id,
        &project_root,
        worktree.as_deref(),
        &overlay_dir,
        &scratch_dir,
    ));
    for (k, v) in &prepared.substitution_env {
        env.insert(k.clone(), v.clone());
    }

    let result = run_shell_async(&joined, Some(&cwd), Some(&env), prepared.timeout, None)
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

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

async fn validate(definition: &str) -> Result<(), String> {
    config::init_global().map_err(|e| e.to_string())?;

    let project_root = config::project_root();
    let definition_path = discovery::resolve_definition_path(definition, project_root.clone())
        .map_err(|e| format!("definition not found: {e}"))?;

    let default_client = config::global_config()
        .ok()
        .and_then(|c| c.default_client().map(String::from));
    let gremlin_def =
        StaticDefinition::from_yaml_file(&definition_path, None, default_client.as_deref())
            .map_err(|e| format!("invalid definition: {e}"))?;

    let mut gremlin = Gremlin::for_dry_run(gremlin_def);

    match gremlin.run(None).await {
        Ok(0) => Ok(()),
        Ok(exit_code) => {
            let stage = gremlin.state.read_str("stage");
            let detail = gremlin
                .state
                .read_bail_info()
                .and_then(|info| info.get("detail").cloned())
                .and_then(|v| {
                    if v.is_string() {
                        Some(v.as_str().unwrap().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            if !detail.is_empty() {
                eprintln!("stage {stage}: {detail}");
            }
            Err(format!(
                "definition validation failed with exit code {exit_code}"
            ))
        }
        Err(gremlins::executor::RunError::StageFailed { stage, message }) => {
            eprintln!("stage {stage}: {message}");
            Err("definition validation failed".to_string())
        }
        Err(error) => {
            eprintln!("{error}");
            Err("definition validation failed".to_string())
        }
    }
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

    // Resolve the definition to a definition path.
    let project_root = config::project_root();
    let definition_path = discovery::resolve_definition_path(definition, project_root.clone())
        .map_err(|e| format!("definition not found: {e}"))?;

    // Load the definition just enough to validate --key args against
    // bootstrap.source.
    let default_client = config::global_config()
        .ok()
        .and_then(|c| c.default_client().map(String::from));
    let gremlin_def =
        StaticDefinition::from_yaml_file(&definition_path, None, default_client.as_deref())
            .map_err(|e| format!("invalid definition: {e}"))?;

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
        "args": raw_args,
        "project_root": project_root.to_string_lossy(),
    }))
    .await?;

    check_error(&response)?;

    let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("");
    println!("{id}");
    Ok(())
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
