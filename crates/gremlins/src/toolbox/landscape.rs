use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use super::landscape_fn::LandscapeExplorerFn;
use crate::clients::tools::{
    blocking_string, io_enforce, parse_args, req_str, resolve, AgentLoopRuntime, ToolHandler,
};

pub(crate) struct LandscapeTool;

/// Module-level explorer, set once at startup. The `LandscapeTool` handler
/// reads it on every invocation — the concrete implementation lives in
/// `crate::landscape` and is injected from `executor/run.rs`.
static EXPLORER: OnceLock<Arc<dyn LandscapeExplorerFn>> = OnceLock::new();

pub(crate) fn set_landscape_explorer(explorer: Arc<dyn LandscapeExplorerFn>) {
    // Silently ignore re-sets so tests in the same process can each
    // install the explorer without coordination.
    let _ = EXPLORER.set(explorer);
}

/// Fallback used in tests and when the static hasn't been initialized
/// (panics with a clear message at the actual call site).
fn get_explorer() -> &'static Arc<dyn LandscapeExplorerFn> {
    EXPLORER
        .get()
        .expect("landscape explorer not initialized — call set_landscape_explorer during start")
}

#[async_trait]
impl ToolHandler for LandscapeTool {
    async fn invoke(&self, _id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        landscape_invoke(ctx, args_json).await
    }
}

pub(crate) async fn landscape_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || landscape_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

pub(crate) fn landscape_sync(
    cwd: Option<&Path>,
    roots: Option<&[PathBuf]>,
    args_json: &str,
) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let file_path = match req_str(&args, "file_path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = resolve(file_path, cwd);
    if let Some(roots) = roots {
        if let Some(err) = io_enforce(&path, roots) {
            return err;
        }
    }

    let depth: Option<usize> = if args.get("depth").is_some() {
        let n = match args.get("depth").and_then(|v| v.as_u64()) {
            Some(n) => n as usize,
            None => return "Error: 'depth' must be a non-negative integer".to_string(),
        };
        Some(n)
    } else {
        None
    };

    let token_budget: Option<usize> =
        if let Some(v) = args.get("token_budget").filter(|v| !v.is_null()) {
            match v.as_u64() {
                Some(n) => Some(n as usize),
                None => {
                    return "Error: 'token_budget' must be a non-negative integer".to_string();
                }
            }
        } else {
            // Model omitted token_budget — fall back to config / env var.
            crate::config::max_landscape_tool_tokens().map(|n| n as usize)
        };

    // Reject a symlink root so the directory walk does not follow it.
    let meta = match path.symlink_metadata() {
        Ok(m) => m,
        Err(e) => return format!("Error: {e}"),
    };
    if meta.is_symlink() {
        return format!(
            "Error: Landscape does not follow symlinks: {}",
            path.display()
        );
    }

    let explorer = get_explorer();

    if meta.is_dir() {
        let depth_limit = depth.unwrap_or(usize::MAX);
        match explorer.explore_directory(&path, depth_limit, token_budget) {
            Ok(block) => {
                if block.is_empty() {
                    "(empty directory)".to_string()
                } else {
                    block
                }
            }
            Err(e) => e,
        }
    } else {
        match explorer.explore_file(&path) {
            Ok(outline) => outline,
            Err(e) => e,
        }
    }
}
