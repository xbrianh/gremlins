//! Agent system-prompt assembly.
//!
//! [`SystemPrompt`] is the single place that knows how to compose the harness
//! prompt (or a stage-supplied override) with the structural landscape block.
//! Agent runners never touch the landscape module directly; they ask this
//! module to compute a landscape and assemble the final prompt for them.

use std::path::PathBuf;

use crate::executor::gremlin::Gremlin;
use crate::executor::state::FileSystemStateStore;
use crate::landscape::{LandscapeBlock, LandscapeError, LandscapeExplorer};

const LANDSCAPE_CACHE_FILE: &str = "landscape.cache.json";

/// The result of a landscape build: the formatted block ready for injection
/// together with summary stats for logging.
struct LandscapeSnapshot {
    block: String,
    file_count: usize,
    item_count: usize,
}

/// Assembles the final system prompt for one agent stage.
pub(crate) struct SystemPrompt {
    /// The base harness prompt (`agent_system_prompt`, `chat_system.md`, or a
    /// stage-supplied `system_prompt` override).
    base: String,
    /// The pre-formatted landscape block, if any files were outlined.
    landscape: Option<String>,
}

impl SystemPrompt {
    pub(crate) fn new(base: String) -> Self {
        Self {
            base,
            landscape: None,
        }
    }

    /// Attach a landscape block. An empty block is a no-op: an agent whose
    /// workspace has no recognized files gets the plain base prompt.
    pub(crate) fn with_landscape(mut self, block: String) -> Self {
        self.landscape = (!block.is_empty()).then_some(block);
        self
    }

    /// Assemble the final prompt: `base`, followed by the landscape block when
    /// one is present.
    pub(crate) fn assemble(self) -> String {
        match self.landscape {
            Some(landscape) => format!("{}\n\n{}", self.base, landscape),
            None => self.base,
        }
    }
}

/// Compute the gremlin's stage-start landscape block.
///
/// The block is a one-shot snapshot of `gremlin.cwd()` at call time. A missing
/// workspace degrades to an empty block, never an error; any other build
/// failure is logged (to both the daemon log and the gremlin's `log_tx` so the
/// operator sees it in `gremlins log`) before degrading the same way.
///
/// `has_outline_tool` controls whether the block advertises the `Landscape`
/// tool; stages/backends without that tool must not be told to use it.
///
/// The synchronous directory walk runs on a blocking thread pool so the tokio
/// runtime is never stalled on filesystem I/O.
pub(crate) async fn landscape_block(gremlin: &Gremlin, has_outline_tool: bool) -> String {
    let snapshot = build_landscape_snapshot(
        gremlin.cwd(),
        landscape_cache_path(gremlin),
        gremlin.runtime_config.log_tx.clone(),
        has_outline_tool,
    )
    .await;

    // Emit a log line so the operator can see the landscape working. A dropped
    // block has `file_count == 0`, and its warning is sent to `log_tx` by
    // `build_landscape_snapshot`, so an operator sees the failure rather than
    // a line that looks identical to a disabled landscape.
    if snapshot.file_count > 0 {
        let tokens = crate::landscape::count_tokens(&snapshot.block).unwrap_or(0);
        if let Some(tx) = &gremlin.runtime_config.log_tx {
            let _ = tx.send(format!(
                "[landscape] {} files, {} items, {} tokens",
                snapshot.file_count, snapshot.item_count, tokens
            ));
        }
    }

    snapshot.block
}

/// Build and format a landscape block for one workspace root.
///
/// `cwd` and `cache_path` are owned up front so the blocking closure needs no
/// borrow of the gremlin. A missing workspace produces an empty snapshot; any
/// other failure is logged (daemon + `log_tx`) and also degrades to an empty
/// snapshot.
///
/// `file_count`/`item_count` are computed from the files that actually enter
/// the formatted block (files need at least three outline items), so the log
/// line never reports a `[landscape] 538 files, …` summary for a block that was
/// trimmed to nothing.
async fn build_landscape_snapshot(
    cwd: PathBuf,
    cache_path: Option<PathBuf>,
    log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    has_outline_tool: bool,
) -> LandscapeSnapshot {
    let root = cwd.clone();
    let walk_log_tx = log_tx.clone();
    let outlines = tokio::task::spawn_blocking(move || {
        match LandscapeExplorer.build(&cwd, usize::MAX, cache_path.as_deref()) {
            Ok(outlines) => outlines,
            Err(LandscapeError::Read(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Vec::new()
            }
            Err(error) => {
                let msg = format!("landscape build failed for {}: {error}", cwd.display());
                send_log(&walk_log_tx, &msg);
                Vec::new()
            }
        }
    })
    .await
    .unwrap_or_else(|error| {
        let msg = format!("landscape build task failed: {error}");
        send_log(&log_tx, &msg);
        Vec::new()
    });

    let budget: u64 = crate::config::max_landscape_tokens();
    let max_tokens: usize = usize::try_from(budget).unwrap_or(usize::MAX);
    let landscape = match crate::landscape::format_landscape_block(
        &outlines,
        &root,
        max_tokens,
        has_outline_tool,
    ) {
        Ok(block) => block,
        Err(LandscapeError::BudgetExceeded) => {
            let msg = format!(
                "[landscape] block for {} could not be trimmed to fit the {} token budget; omitting",
                root.display(),
                budget,
            );
            send_log(&log_tx, &msg);
            LandscapeBlock::empty()
        }
        Err(other) => {
            send_log(
                &log_tx,
                &format!("landscape block formatting failed: {other}"),
            );
            LandscapeBlock::empty()
        }
    };

    LandscapeSnapshot {
        block: landscape.block,
        file_count: landscape.file_count,
        item_count: landscape.item_count,
    }
}

fn send_log(log_tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>, msg: &str) {
    if let Some(tx) = log_tx {
        let _ = tx.send(msg.to_string());
    }
    log::warn!("{msg}");
}

/// Resolve the on-disk cache blob path for landscape builds.
///
/// Cache I/O is only valid for a concrete filesystem state store. Other store
/// backends (and the fallback `state_uri_display()` value, which may be a URI
/// rather than a filesystem path) simply don't get a persistent cache, so
/// returning `None` on a failed downcast is safer than treating a URI as a
/// `PathBuf`.
fn landscape_cache_path(gremlin: &Gremlin) -> Option<PathBuf> {
    let store = gremlin.state.store_ref();
    store
        .as_any()
        .and_then(|any| any.downcast_ref::<FileSystemStateStore>())
        .map(|fs| fs.root_dir().join(LANDSCAPE_CACHE_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::clients::client::Client;
    use crate::definition::StaticDefinition;
    use crate::executor::gremlin::{validate_gremlin_id, ScratchDir};
    use crate::executor::state::StateData;

    /// A Gremlin whose state store is a plain filesystem directory, so cache
    /// path resolution can be exercised without launching a run.
    fn fs_gremlin(state_dir: PathBuf) -> Gremlin {
        Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            definition_path: None,
            model_override: None,
            definition: Box::new(StaticDefinition::stub()),
            workdir: None,
            project_root: PathBuf::new(),
            state: StateData::from_store(
                Some("gr-test".to_string()),
                Box::new(FileSystemStateStore::from_path(state_dir)),
            ),
            env: std::collections::HashMap::new(),
            client: Client::parse("cmd/true").unwrap(),
            loop_iter: "1".to_string(),
            stage_inputs: std::collections::HashMap::new(),
            runtime_config: crate::executor::gremlin::RuntimeConfig::default(),
            cancel_token: None,
            interactive_session: None,
            scratch_dir: ScratchDir::Persistent(PathBuf::from("/tmp/scratch")),
            clean_cmds: Vec::new(),
            enclosing_task_models: None,
            enclosing_model: None,
        }
    }

    #[test]
    fn assembles_base_alone_without_landscape() {
        let prompt = SystemPrompt::new("base prompt".to_string()).assemble();
        assert_eq!(prompt, "base prompt");
    }

    #[test]
    fn assembles_landscape_after_base() {
        let prompt = SystemPrompt::new("base prompt".to_string())
            .with_landscape("<!-- LANDSCAPE -->".to_string())
            .assemble();
        assert_eq!(prompt, "base prompt\n\n<!-- LANDSCAPE -->");
    }

    #[test]
    fn empty_landscape_is_a_noop() {
        let prompt = SystemPrompt::new("base prompt".to_string())
            .with_landscape(String::new())
            .assemble();
        assert_eq!(prompt, "base prompt");
    }

    #[tokio::test]
    async fn missing_cwd_produces_empty_block() {
        let snapshot = build_landscape_snapshot(
            PathBuf::from("/definitely/not/present/landscape-cwd"),
            None,
            None,
            true,
        )
        .await;
        assert_eq!(snapshot.block, "");
        assert_eq!(snapshot.file_count, 0);
        assert_eq!(snapshot.item_count, 0);
    }

    #[tokio::test]
    async fn dropped_block_reports_zero_stats() {
        // A file with only one outline item is below the three-item threshold:
        // the formatted block is empty, and the stats must report 0 rather
        // than a `[landscape] 1 files, 1 items, 0 tokens`-style line that hides
        // the fact that nothing was injected.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("thin.rs"), "fn only() {}\n").unwrap();

        let snapshot = build_landscape_snapshot(dir.path().to_path_buf(), None, None, true).await;
        assert_eq!(snapshot.block, "");
        assert_eq!(snapshot.file_count, 0);
        assert_eq!(snapshot.item_count, 0);
    }

    #[tokio::test]
    async fn nonempty_outline_flows_through_assemble() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() {}\nstruct Config {}\nimpl Config {}\n",
        )
        .unwrap();

        let snapshot = build_landscape_snapshot(dir.path().to_path_buf(), None, None, true).await;
        assert!(snapshot.block.contains("<!-- BEGIN LANDSCAPE BLOCK -->"));
        assert!(snapshot
            .block
            .contains(&format!("<!-- root: {} -->\n", dir.path().display())));
        assert!(snapshot
            .block
            .lines()
            .any(|line| line.starts_with("main.rs:")));
        assert!(!snapshot
            .block
            .contains(&format!("{}:", dir.path().join("main.rs").display())));
        assert!(snapshot.block.contains("1|fn|main"));
        assert!(snapshot.block.contains("2|struct|Config"));
        assert!(snapshot.block.contains("3|impl|Config"));
        assert_eq!(snapshot.file_count, 1);
        assert_eq!(snapshot.item_count, 3);

        let prompt = SystemPrompt::new("base prompt".to_string())
            .with_landscape(snapshot.block.clone())
            .assemble();
        assert_eq!(prompt, format!("base prompt\n\n{}", snapshot.block));
    }

    #[test]
    fn filesystem_store_cache_path_lands_under_state_root() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let gremlin = fs_gremlin(state_dir.clone());
        assert_eq!(
            landscape_cache_path(&gremlin),
            Some(state_dir.join(LANDSCAPE_CACHE_FILE))
        );
    }
}
