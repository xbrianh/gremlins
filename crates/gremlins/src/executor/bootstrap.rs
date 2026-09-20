//! Worktree bootstrap: the shell commands and `gremlins:` DSL that run before
//! a definition's stages.
//!
//! Every first-start launch has to prepare its checkout — a virtualenv, a
//! dependency sync, an artifact bound from a CLI flag — before any stage can
//! use it. [`run_definition_bootstrap`] is that preparation, in the order the
//! Python executor ran it:
//!
//! 1. `bootstrap.cmds` — per-worktree setup, run in the worktree.
//! 2. `bootstrap.launch_cmds` — launch-only commands. An entry that parses as
//!    a `gremlins:` DSL call runs inline; everything else is a shell command,
//!    `{var}`-substituted and joined with `&&`.
//! 3. `bootstrap.cli_out` — artifact bindings computed at launch, run through a
//!    synthetic [`Exec`] so they share the stage layer's URI and commit rules.
//!
//! The DSL exists so a launch can *bind* a source value into the registry —
//! today only `gremlins:bind_artifact(uri, source_key)` — without shelling out
//! and reaching back into the harness. Its parsers are deliberately small and
//! pure; the one handler is the only thing that touches the registry.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::artifacts::uri::Uri;
use crate::core::proc::run_shell_async;
use crate::executor::gremlin::Gremlin;
use crate::executor::run::{loop_iter_of, truncate};
use crate::executor::RunError;
use crate::schemas::bootstrap::substitute_bootstrap_vars;
use crate::stages::exec::{commit_exec, prepare_exec, run_shell, Exec};

/// `gremlins:<name>(<args>)`, anchored at the start — the DSL marker is a
/// prefix, never an infix, so a shell line that merely mentions one is not a
/// DSL call.
static GREMLINS_CMD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^gremlins:([a-z_]+)\(([^)]*)\)").unwrap());

// ---------------------------------------------------------------------------
// Shell runner
// ---------------------------------------------------------------------------

/// Run `cmds` in `cwd` under `env`. A non-zero exit is a
/// [`RunError::BootstrapFailed`].
///
/// Blank lines are dropped and the rest joined with `&&`, so a multi-line
/// bootstrap block is one shell invocation that stops at the first failure.
/// `GREMLINS_BOOTSTRAP_CWD` is injected alongside the gremlin's resolved
/// environment, so bootstrap sees the same system variables as every stage —
/// plus the one variable that tells it where it is.
pub async fn run_bootstrap(
    cmds: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<(), RunError> {
    let cmds: Vec<&str> = cmds
        .iter()
        .filter(|cmd| !cmd.trim().is_empty())
        .map(|cmd| cmd.trim_end())
        .collect();
    if cmds.is_empty() {
        return Ok(());
    }

    // Canonicalize: on macOS /var and /tmp are symlinks to /private/…,
    // so the path the caller hands us may differ from the physical path
    // the kernel reports to `pwd` when the child process starts.
    let cwd = std::fs::canonicalize(cwd).map_err(|error| RunError::BootstrapFailed {
        exit_code: 1,
        stderr: format!("canonicalize cwd: {error}"),
    })?;

    let mut env = env.clone();
    env.insert(
        "GREMLINS_BOOTSTRAP_CWD".to_string(),
        cwd.to_string_lossy().into_owned(),
    );

    let result = run_shell_async(&cmds.join(" && "), Some(&cwd), Some(&env), None)
        .await
        .map_err(|error| RunError::BootstrapFailed {
            exit_code: 1,
            stderr: error.to_string(),
        })?;

    if result.returncode != 0 {
        let detail = failure_detail(&result.stdout, &result.stderr);
        log::error!(
            "bootstrap failed (exit {}): {}",
            result.returncode,
            truncate(&detail, 2000)
        );
        return Err(RunError::BootstrapFailed {
            exit_code: result.returncode,
            stderr: truncate(&detail, 500),
        });
    }

    log::info!("bootstrap ok");
    Ok(())
}

/// The failure text for a non-zero bootstrap exit.
///
/// `stderr` is the interesting stream, but a command that fails silently on it
/// still said something on stdout — and a stream carrying only whitespace says
/// nothing at all, so it is no reason to ignore the other one.
fn failure_detail(stdout: &[u8], stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::from_utf8_lossy(stdout).trim().to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// gremlins: DSL parsing
// ---------------------------------------------------------------------------

/// Parse a `gremlins:` DSL call out of a launch command.
///
/// Returns `(name, args)` for a well-formed call and `None` for anything else —
/// a plain shell command, or a line with trailing junk after the closing
/// paren, which is a shell command that happens to look like one.
pub fn parse_gremlins_command(raw: &str) -> Option<(String, Vec<String>)> {
    let caps = GREMLINS_CMD_RE.captures(raw)?;
    let cmd_name = caps.get(1)?.as_str().to_string();
    let args_raw = caps.get(2)?.as_str();

    // The whole string must be the call: anything after `)` means this was
    // never meant as DSL.
    if !raw[caps.get(0)?.end()..].trim().is_empty() {
        return None;
    }

    Some((cmd_name, split_dsl_args(args_raw)))
}

/// Split comma-separated DSL arguments, honouring quotes.
///
/// Commas inside a quoted run are literal, the quotes themselves are stripped,
/// and each piece is trimmed. Every piece between commas is an argument,
/// including an empty one — `a,,b` is three, and a trailing comma opens a last
/// empty argument — so the arity check sees the argument list the caller
/// actually wrote. An explicitly quoted empty string is an argument too; only
/// an entirely blank `args_raw` yields no arguments at all.
pub fn split_dsl_args(args_raw: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    // Whether the piece being built was quoted or followed a comma: either way
    // an empty tail is a written argument, not absent whitespace.
    let mut piece_declared = false;

    for ch in args_raw.chars() {
        match quote {
            Some(open) if ch == open => {
                quote = None;
                piece_declared = true;
            }
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                piece_declared = true;
            }
            None if ch == ',' => {
                parts.push(current.trim().to_string());
                current.clear();
                piece_declared = true;
            }
            None => current.push(ch),
        }
    }

    let tail = current.trim();
    if !tail.is_empty() || piece_declared {
        parts.push(tail.to_string());
    }
    parts
}

/// Validate `bind_artifact`'s arguments and unpack them as `(source_key, uri)`.
///
/// The artifact key is the URI itself; `source_key` names the bootstrap source
/// whose value gets bound. Optional sources are *not* handled here — an absent
/// or empty value is a silent no-op in the handler, not a parse error.
pub fn parse_bind_artifact_args(args: &[String]) -> Result<(String, String), String> {
    if args.len() != 2 {
        return Err(format!(
            "bind_artifact requires 2 arguments (uri, source_key), got {}",
            args.len()
        ));
    }
    let uri_str = &args[0];
    let source_key = &args[1];

    if uri_str.is_empty() {
        return Err("bind_artifact: uri must be non-empty".to_string());
    }
    if source_key.is_empty() {
        return Err("bind_artifact: source_key must be non-empty".to_string());
    }
    if !uri_str.contains("://") {
        return Err(format!(
            "bind_artifact: first argument {uri_str:?} does not look like a URI \
             (expected 'artifact://...'); use bind_artifact(uri, source_key)"
        ));
    }

    Ok((source_key.clone(), uri_str.clone()))
}

// ---------------------------------------------------------------------------
// gremlins: DSL execution
// ---------------------------------------------------------------------------

/// Dispatch a parsed DSL call to its handler.
///
/// A match, not a table: there is one command, and a table would be a
/// one-entry indirection. Adding a second means adding an arm here and a
/// handler beside it.
fn run_dsl_command(cmd_name: &str, args: &[String], gremlin: &mut Gremlin) -> Result<(), RunError> {
    match cmd_name {
        "bind_artifact" => {
            let (source_key, uri_str) =
                parse_bind_artifact_args(args).map_err(|message| RunError::BootstrapFailed {
                    exit_code: 1,
                    stderr: message,
                })?;
            bind_artifact(&source_key, &uri_str, gremlin)
        }
        unknown => Err(RunError::BootstrapFailed {
            exit_code: 1,
            stderr: format!("unknown gremlins: command {unknown:?}; known: bind_artifact"),
        }),
    }
}

/// Resolve a bootstrap source value and bind it as the artifact at `uri_str`.
///
/// A value that names an existing file — relative to the process, or to the
/// project root — is copied into the registry; anything else is inline text and
/// written. An absent or empty source is an optional source with nothing to
/// bind, so it is a no-op rather than an error.
fn bind_artifact(source_key: &str, uri_str: &str, gremlin: &mut Gremlin) -> Result<(), RunError> {
    let value = match gremlin.stage_inputs.get(source_key) {
        Some(value) if !value.is_empty() => value.clone(),
        _ => return Ok(()),
    };

    let uri = Uri::parse(uri_str).map_err(|error| RunError::BootstrapFailed {
        exit_code: 1,
        stderr: error.to_string(),
    })?;

    let direct = Path::new(&value);
    let from_project = (!gremlin.project_root.as_os_str().is_empty())
        .then(|| gremlin.project_root.join(&value))
        .filter(|path| path.is_file());

    let bound = if direct.is_file() {
        gremlin.registry.copy_into_registry(&uri, direct)
    } else if let Some(path) = from_project {
        gremlin.registry.copy_into_registry(&uri, &path)
    } else {
        gremlin.registry.write_into_registry(&uri, &value)
    };

    bound
        .map_err(|error| RunError::BootstrapFailed {
            exit_code: 1,
            stderr: error.to_string(),
        })
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Definition bootstrap
// ---------------------------------------------------------------------------

/// Run a gremlin's bootstrap, in order, against its own worktree.
///
/// Every piece of state it needs — the worktree, the resolved environment, the
/// stage inputs, the registry — already lives on `gremlin`, so the caller is a
/// single guard and a single call.
pub async fn run_definition_bootstrap(gremlin: &mut Gremlin) -> Result<(), RunError> {
    // Snapshot the bootstrap block: the DSL step borrows `gremlin` mutably for
    // its registry, so the commands cannot stay borrowed from the definition.
    let bootstrap = gremlin.definition.bootstrap.clone();
    let cwd = gremlin.cwd();
    let env = gremlin.env.clone();

    if !bootstrap.cmds.is_empty() {
        run_bootstrap(&bootstrap.cmds, &cwd, &env).await?;
    }

    if !bootstrap.launch_cmds.is_empty() {
        log::info!("running {} launch command(s)", bootstrap.launch_cmds.len());

        // Every declared source key gets a value, even an absent one, so an
        // optional `{plan}` substitutes to an empty string instead of leaking
        // the literal placeholder into a shell command.
        let mut values: HashMap<String, String> = gremlin.stage_inputs.clone();
        if let Some(source) = &bootstrap.source {
            for key in source.all_sources() {
                values.entry(key).or_default();
            }
        }

        let mut shell_cmds: Vec<String> = Vec::new();
        for command in &bootstrap.launch_cmds {
            match parse_gremlins_command(command) {
                Some((cmd_name, args)) => {
                    log::info!("launch DSL: {}({})", cmd_name, args.join(", "));
                    run_dsl_command(&cmd_name, &args, gremlin)?;
                }
                None => shell_cmds.push(substitute_bootstrap_vars(command, &cwd, &values)),
            }
        }

        if !shell_cmds.is_empty() {
            run_bootstrap(&shell_cmds, &cwd, &env).await?;
        }
    }

    if !bootstrap.cli_out.is_empty() {
        log::info!("running {} cli_out binding(s)", bootstrap.cli_out.len());
        run_cli_out(gremlin, &bootstrap.cli_out, &cwd, &env).await?;
    }

    Ok(())
}

/// Bind `cli_out`'s artifacts through a synthetic `bootstrap` exec stage.
///
/// Routing through the stage layer rather than the registry directly is what
/// gives launch-time bindings the same URI resolution, optional-bind handling,
/// and commit verification as any other stage's outputs. The exec has no
/// commands — it exists only for its `bind` map — so the shell phase is a
/// no-op and the commit phase does all the work.
async fn run_cli_out(
    gremlin: &mut Gremlin,
    cli_out: &HashMap<String, String>,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<(), RunError> {
    let exec = Exec {
        name: "bootstrap".to_string(),
        options: HashMap::new(),
        interpolation_map: HashMap::new(),
        bind_map: cli_out.clone(),
    };
    let loop_iter = loop_iter_of(&gremlin.loop_stack);
    let framework_subs = HashMap::from([
        ("name".to_string(), exec.name.clone()),
        ("model".to_string(), gremlin.client.model().to_string()),
        ("cwd".to_string(), cwd.to_string_lossy().into_owned()),
        ("base_ref".to_string(), gremlin.base_ref.clone()),
    ]);

    let failed = |error: String| RunError::BootstrapFailed {
        exit_code: 1,
        stderr: error,
    };

    let mut prepared = prepare_exec(&exec, &gremlin.registry, &loop_iter, &framework_subs)
        .map_err(|error| failed(error.to_string()))?;
    prepared.cwd = cwd.to_path_buf();
    prepared.artifact_dir = gremlin.artifact_dir.clone();
    prepared.state_dir = gremlin.state_dir.clone();
    prepared.env = env.clone();

    run_shell(&prepared)
        .await
        .map_err(|error| failed(error.to_string()))?;
    commit_exec(&prepared, &gremlin.registry).map_err(|error| failed(error.to_string()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::artifacts::registry::ArtifactRegistry;
    use crate::clients::client::Client;
    use crate::executor::gremlin::validate_gremlin_id;
    use crate::executor::state::{self, StateData};
    use crate::schemas::bootstrap::Bootstrap;
    use crate::schemas::gremlin_definition::GremlinDefinition;

    // --- DSL parsing ---

    #[test]
    fn parses_a_well_formed_command() {
        let (name, args) =
            parse_gremlins_command(r#"gremlins:bind_artifact("artifact://plan.md", plan)"#)
                .expect("a DSL call");
        assert_eq!(name, "bind_artifact");
        assert_eq!(args, vec!["artifact://plan.md", "plan"]);
    }

    #[test]
    fn parses_a_command_with_no_arguments() {
        let (name, args) = parse_gremlins_command("gremlins:bind_artifact()").expect("a DSL call");
        assert_eq!(name, "bind_artifact");
        assert!(args.is_empty());
    }

    #[test]
    fn plain_shell_is_not_a_command() {
        assert!(parse_gremlins_command("uv sync").is_none());
        assert!(parse_gremlins_command("").is_none());
        // The marker must be a prefix.
        assert!(parse_gremlins_command("echo gremlins:bind_artifact(a, b)").is_none());
    }

    #[test]
    fn trailing_junk_disqualifies_a_command() {
        assert!(parse_gremlins_command("gremlins:bind_artifact(a, b) && echo hi").is_none());
        assert!(parse_gremlins_command("gremlins:bind_artifact(a, b)   ").is_some());
    }

    #[test]
    fn command_name_must_be_lowercase() {
        assert!(parse_gremlins_command("gremlins:Bind(1, 2)").is_none());
        assert!(parse_gremlins_command("gremlins:bind-artifact(1, 2)").is_none());
    }

    #[test]
    fn splits_arguments_on_commas_outside_quotes() {
        assert_eq!(split_dsl_args("a, b ,c"), vec!["a", "b", "c"]);
        assert_eq!(split_dsl_args(r#""a, b", c"#), vec!["a, b", "c"]);
        assert_eq!(
            split_dsl_args("'single, quoted', x"),
            vec!["single, quoted", "x"]
        );
        assert_eq!(split_dsl_args(""), Vec::<String>::new());
        assert_eq!(split_dsl_args("   "), Vec::<String>::new());
        // Empty pieces are arguments whether interior or trailing: a trailing
        // comma declares one, so arity checks can see the malformed call.
        assert_eq!(split_dsl_args("a,,b"), vec!["a", "", "b"]);
        assert_eq!(split_dsl_args("a,"), vec!["a", ""]);
        // So is an explicitly quoted empty string — quotes mean an argument
        // was written, not whitespace.
        assert_eq!(split_dsl_args(r#"a, """#), vec!["a", ""]);
        assert_eq!(split_dsl_args("a, ''"), vec!["a", ""]);
        assert_eq!(split_dsl_args(r#""""#), vec![""]);
    }

    #[test]
    fn bind_artifact_args_are_validated() {
        let ok = vec!["artifact://plan.md".to_string(), "plan".to_string()];
        assert_eq!(
            parse_bind_artifact_args(&ok).unwrap(),
            ("plan".to_string(), "artifact://plan.md".to_string())
        );

        let one = vec!["artifact://plan.md".to_string()];
        assert!(parse_bind_artifact_args(&one)
            .unwrap_err()
            .contains("requires 2 arguments"));

        let empty_uri = vec![String::new(), "plan".to_string()];
        assert!(parse_bind_artifact_args(&empty_uri)
            .unwrap_err()
            .contains("uri must be non-empty"));

        let empty_key = vec!["artifact://plan.md".to_string(), String::new()];
        assert!(parse_bind_artifact_args(&empty_key)
            .unwrap_err()
            .contains("source_key must be non-empty"));

        let no_scheme = vec!["plan.md".to_string(), "plan".to_string()];
        assert!(parse_bind_artifact_args(&no_scheme)
            .unwrap_err()
            .contains("does not look like a URI"));

        // A trailing comma declares a third, empty argument: three args, so
        // the exact-arity check rejects it rather than silently reading two.
        let trailing_comma = split_dsl_args(r#""artifact://plan.md", plan,"#);
        assert_eq!(trailing_comma.len(), 3);
        assert!(parse_bind_artifact_args(&trailing_comma)
            .unwrap_err()
            .contains("requires 2 arguments"));
    }

    // --- failure detail ---

    #[test]
    fn failure_detail_prefers_stderr() {
        assert_eq!(failure_detail(b"", b"boom\n"), "boom");
        assert_eq!(failure_detail(b"chatter", b"boom"), "boom");
    }

    #[test]
    fn failure_detail_falls_back_to_stdout() {
        // An absent stderr and a whitespace-only one are equally silent.
        assert_eq!(failure_detail(b"only stdout", b""), "only stdout");
        assert_eq!(failure_detail(b"only stdout", b"\n\t  "), "only stdout");
        assert_eq!(failure_detail(b"", b"\n"), "");
    }

    // --- run_bootstrap ---

    #[tokio::test]
    async fn run_bootstrap_is_a_noop_without_commands() {
        let dir = tempfile::tempdir().unwrap();
        let env = std::env::vars().collect();
        assert!(run_bootstrap(&[], dir.path(), &env).await.is_ok());
        assert!(run_bootstrap(&["   ".to_string()], dir.path(), &env)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_bootstrap_reports_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let env = std::env::vars().collect();
        let error = run_bootstrap(&["exit 7".to_string()], dir.path(), &env)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RunError::BootstrapFailed { exit_code: 7, .. }
        ));
    }

    #[tokio::test]
    async fn run_bootstrap_reports_stdout_when_stderr_is_blank() {
        let dir = tempfile::tempdir().unwrap();
        let env = std::env::vars().collect();
        // stderr says nothing but a newline; the reason is on stdout.
        let error = run_bootstrap(
            &["printf 'the reason'; printf '\\n' >&2; exit 9".to_string()],
            dir.path(),
            &env,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("the reason"), "got: {error}");
    }

    #[tokio::test]
    async fn run_bootstrap_injects_the_cwd_variable() {
        let dir = tempfile::tempdir().unwrap();
        let env = std::env::vars().collect();
        run_bootstrap(
            &["test \"$GREMLINS_BOOTSTRAP_CWD\" = \"$(pwd)\"".to_string()],
            dir.path(),
            &env,
        )
        .await
        .unwrap();
    }

    // --- definition bootstrap ---

    /// A gremlin with a worktree and a seeded state directory: everything
    /// `run_definition_bootstrap` touches.
    fn test_gremlin(
        bootstrap: Bootstrap,
        stage_inputs: HashMap<String, String>,
    ) -> (tempfile::TempDir, Gremlin) {
        let tmp = tempfile::tempdir().unwrap();
        let artifact_dir = tmp.path().join("scratch").join("artifacts");
        let state_dir = tmp.path().join("state").join("gr-test");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();

        let data = serde_json::json!({
            "id": "gr-test",
            "attempt": "test-0001",
            "stage": "starting",
            "client": "cmd:true",
        });
        state::write_state(&state_dir, data.as_object().unwrap()).unwrap();

        let mut state_data = StateData::new(Some("gr-test".to_string()));
        state_data.state_file = Some(state_dir.join("state.json"));

        let gremlin = Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            state_dir,
            artifact_dir: artifact_dir.clone(),
            definition_path: None,
            client_override: None,
            definition: GremlinDefinition {
                name: "test".to_string(),
                path: PathBuf::from("test.yaml"),
                default_client: "cmd:true".to_string(),
                base_ref: "main".to_string(),
                bootstrap,
                stages: Vec::new(),
                land: None,
            },
            registry: ArtifactRegistry::new(artifact_dir),
            worktree: Some(worktree),
            worktree_parent: None,
            project_root: tmp.path().to_path_buf(),
            base_ref_sha: String::new(),
            base_ref: "main".to_string(),
            resume_from: None,
            state: state_data,
            env: std::env::vars().collect(),
            client: Client::parse("cmd:true").unwrap(),
            stage_inputs,
            loop_stack: Vec::new(),
        };
        (tmp, gremlin)
    }

    #[tokio::test]
    async fn cmds_run_in_the_worktree() {
        let bootstrap = Bootstrap {
            cmds: vec!["echo hello > marker.txt".to_string()],
            ..Default::default()
        };
        let (tmp, mut gremlin) = test_gremlin(bootstrap, HashMap::new());

        run_definition_bootstrap(&mut gremlin).await.unwrap();

        assert!(tmp.path().join("worktree").join("marker.txt").is_file());
    }

    #[tokio::test]
    async fn launch_dsl_binds_a_source_file() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("plan.md");
        std::fs::write(&source, "# plan\n").unwrap();

        let bootstrap = Bootstrap {
            launch_cmds: vec![r#"gremlins:bind_artifact("artifact://plan.md", plan)"#.to_string()],
            ..Default::default()
        };
        let inputs = HashMap::from([("plan".to_string(), source.to_string_lossy().into_owned())]);
        let (_tmp, mut gremlin) = test_gremlin(bootstrap, inputs);

        run_definition_bootstrap(&mut gremlin).await.unwrap();

        assert!(gremlin.registry.is_live("artifact://plan.md"));
        assert_eq!(
            gremlin
                .registry
                .content("artifact://plan.md", None)
                .unwrap(),
            "# plan\n"
        );
    }

    #[tokio::test]
    async fn launch_dsl_writes_inline_text() {
        let bootstrap = Bootstrap {
            launch_cmds: vec![r#"gremlins:bind_artifact("artifact://note.txt", note)"#.to_string()],
            ..Default::default()
        };
        let inputs = HashMap::from([("note".to_string(), "hello".to_string())]);
        let (_tmp, mut gremlin) = test_gremlin(bootstrap, inputs);

        run_definition_bootstrap(&mut gremlin).await.unwrap();

        assert_eq!(
            gremlin
                .registry
                .content("artifact://note.txt", None)
                .unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn launch_dsl_skips_an_absent_source() {
        let bootstrap = Bootstrap {
            launch_cmds: vec![r#"gremlins:bind_artifact("artifact://plan.md", plan)"#.to_string()],
            ..Default::default()
        };
        let (_tmp, mut gremlin) = test_gremlin(bootstrap, HashMap::new());

        run_definition_bootstrap(&mut gremlin).await.unwrap();

        assert!(!gremlin.registry.is_live("artifact://plan.md"));
    }

    #[tokio::test]
    async fn unknown_dsl_command_fails() {
        let bootstrap = Bootstrap {
            launch_cmds: vec!["gremlins:nonsense(a, b)".to_string()],
            ..Default::default()
        };
        let (_tmp, mut gremlin) = test_gremlin(bootstrap, HashMap::new());

        let error = run_definition_bootstrap(&mut gremlin).await.unwrap_err();
        assert!(error.to_string().contains("unknown gremlins: command"));
    }

    #[tokio::test]
    async fn launch_shell_commands_are_substituted() {
        let bootstrap = Bootstrap {
            launch_cmds: vec!["echo {greeting} > greeting.txt".to_string()],
            ..Default::default()
        };
        let inputs = HashMap::from([("greeting".to_string(), "hi".to_string())]);
        let (tmp, mut gremlin) = test_gremlin(bootstrap, inputs);

        run_definition_bootstrap(&mut gremlin).await.unwrap();

        let written =
            std::fs::read_to_string(tmp.path().join("worktree").join("greeting.txt")).unwrap();
        assert_eq!(written.trim(), "hi");
    }

    #[tokio::test]
    async fn cli_out_registers_artifacts() {
        let bootstrap = Bootstrap {
            cli_out: HashMap::from([("pr".to_string(), "artifact://pr.txt".to_string())]),
            ..Default::default()
        };
        let (_tmp, mut gremlin) = test_gremlin(bootstrap, HashMap::new());

        // The synthetic exec verifies the bound file exists, so the producer's
        // output is staged first — exactly as a real cli_out follows its cmds.
        let uri = Uri::parse("artifact://pr.txt").unwrap();
        let path = gremlin.registry.path_for_uri(&uri).unwrap();
        std::fs::write(&path, "123").unwrap();

        run_definition_bootstrap(&mut gremlin).await.unwrap();

        assert!(gremlin.registry.is_live("artifact://pr.txt"));
    }

    #[tokio::test]
    async fn failing_cmds_are_reported() {
        let bootstrap = Bootstrap {
            cmds: vec!["exit 3".to_string()],
            ..Default::default()
        };
        let (_tmp, mut gremlin) = test_gremlin(bootstrap, HashMap::new());

        let error = run_definition_bootstrap(&mut gremlin).await.unwrap_err();
        assert!(matches!(
            error,
            RunError::BootstrapFailed { exit_code: 3, .. }
        ));
    }

    #[tokio::test]
    async fn an_empty_bootstrap_does_nothing() {
        let (_tmp, mut gremlin) = test_gremlin(Bootstrap::default(), HashMap::new());
        assert!(run_definition_bootstrap(&mut gremlin).await.is_ok());
    }
}
