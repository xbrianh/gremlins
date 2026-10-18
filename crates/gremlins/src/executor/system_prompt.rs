//! Agent system-prompt assembly.
//!
//! [`SystemPrompt`] is the single place that knows how to compose the harness
//! prompt (or a stage-supplied override) with the structural landscape block.
//! Agent runners never touch the landscape module directly; they ask this
//! module to compute a landscape and assemble the final prompt for them.

use std::path::PathBuf;

use crate::executor::gremlin::Gremlin;
use crate::executor::state::FileSystemStateStore;
use crate::landscape::{LandscapeError, LandscapeExplorer};

const LANDSCAPE_CACHE_FILE: &str = "landscape.cache.json";

/// The result of a landscape build: the formatted block ready for injection
/// together with summary stats for logging.
struct LandscapeSnapshot {
    block: String,
    file_count: usize,
    item_count: usize,
}

/// Assembles the final system prompt for one agent stage.
#[derive(Debug, Clone)]
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
/// failure is logged before degrading the same way.
///
/// The synchronous directory walk runs on a blocking thread pool so the tokio
/// runtime is never stalled on filesystem I/O.
pub(crate) async fn landscape_block(gremlin: &Gremlin) -> String {
    let snapshot = build_landscape_snapshot(gremlin.cwd(), landscape_cache_path(gremlin)).await;

    // Emit a log line so the operator can see the landscape is working.
    if snapshot.file_count > 0 {
        let kb = snapshot.block.len() as f64 / 1024.0;
        if let Some(tx) = &gremlin.runtime_config.log_tx {
            let _ = tx.send(format!(
                "[landscape] {} files, {} items, {:.1} KB",
                snapshot.file_count, snapshot.item_count, kb
            ));
        }
    }

    snapshot.block
}

/// Build and format a landscape block for one workspace root.
///
/// `cwd` and `cache_path` are owned up front so the blocking closure needs no
/// borrow of the gremlin. A missing workspace produces an empty snapshot; any
/// other failure is logged and also degrades to an empty snapshot.
async fn build_landscape_snapshot(cwd: PathBuf, cache_path: Option<PathBuf>) -> LandscapeSnapshot {
    let root = cwd.clone();
    let outlines = tokio::task::spawn_blocking(move || {
        match LandscapeExplorer.build(&cwd, usize::MAX, cache_path.as_deref()) {
            Ok(outlines) => outlines,
            Err(LandscapeError::Read(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Vec::new()
            }
            Err(error) => {
                log::warn!("landscape build failed for {}: {error}", cwd.display());
                Vec::new()
            }
        }
    })
    .await
    .unwrap_or_else(|error| {
        log::warn!("landscape build task failed: {error}");
        Vec::new()
    });

    let file_count = outlines.iter().filter(|o| !o.items.is_empty()).count();
    let item_count: usize = outlines.iter().map(|o| o.items.len()).sum();
    let budget: u64 = crate::config::max_landscape_bytes();
    let max_bytes: usize = usize::try_from(budget).unwrap_or(usize::MAX);
    let block = match crate::landscape::format_landscape_block(&outlines, &root, max_bytes) {
        Ok(block) => block,
        Err(LandscapeError::BudgetExceeded) => {
            log::warn!(
                "landscape block for {} could not be trimmed to fit the {} byte budget; omitting",
                root.display(),
                budget,
            );
            String::new()
        }
        Err(other) => {
            log::warn!("landscape block formatting failed: {other}");
            String::new()
        }
    };

    LandscapeSnapshot {
        block,
        file_count,
        item_count,
    }
}

/// Resolve the on-disk cache blob path for landscape builds.
///
/// Prefer the concrete filesystem store's real root (ephemeral runs live in a
/// `TempDir`); otherwise fall back to the `StateData` display path when it is
/// non-empty.
fn landscape_cache_path(gremlin: &Gremlin) -> Option<PathBuf> {
    let store = gremlin.state.store_ref();
    if let Some(fs) = store
        .as_any()
        .and_then(|any| any.downcast_ref::<FileSystemStateStore>())
    {
        return Some(fs.root_dir().join(LANDSCAPE_CACHE_FILE));
    }

    let display = gremlin.state.state_uri_display();
    if display.is_empty() {
        None
    } else {
        Some(PathBuf::from(display).join(LANDSCAPE_CACHE_FILE))
    }
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
        let snapshot =
            build_landscape_snapshot(PathBuf::from("/definitely/not/present/landscape-cwd"), None)
                .await;
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

        let snapshot = build_landscape_snapshot(dir.path().to_path_buf(), None).await;
        assert!(snapshot.block.contains("<!-- BEGIN LANDSCAPE BLOCK -->"));
        assert!(snapshot
            .block
            .contains(&format!("<!-- root: {} -->\n", dir.path().display())));
        assert!(snapshot.block.lines().any(|line| line == "main.rs:"));
        assert!(!snapshot
            .block
            .contains(&format!("{}:", dir.path().join("main.rs").display())));
        assert!(snapshot.block.contains("1|fn|main\n"));
        assert!(snapshot.block.contains("2|struct|Config\n"));
        assert!(snapshot.block.contains("3|impl|Config\n"));
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
