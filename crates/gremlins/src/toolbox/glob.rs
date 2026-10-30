use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::tools::{blocking_string, parse_args, req_str, AgentLoopRuntime, ToolHandler};
use crate::core::paths::{io_enforce, resolve};

pub(crate) struct GlobTool;

#[async_trait]
impl ToolHandler for GlobTool {
    async fn invoke(&self, _id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        glob_invoke(ctx, args_json).await
    }
}

pub(crate) async fn glob_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || glob_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn glob_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pattern = match req_str(&args, "pattern") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let search_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let base = resolve(search_path, cwd);
    let full = base.join(pattern);
    let pattern_str = full.to_string_lossy();
    let mut matches: Vec<String> = match glob::glob(&pattern_str) {
        Ok(paths) => paths
            .filter_map(|p| p.ok())
            // Drop matches that resolve outside any allowed root (e.g. via a symlink).
            // `None` roots means containment is disabled (AllowAll).
            .filter(|p| roots.is_none_or(|roots| io_enforce(p, roots).is_none()))
            .map(|p| p.display().to_string())
            .collect(),
        Err(e) => return format!("Error: {e}"),
    };
    matches.sort();
    if matches.is_empty() {
        "(no matches)".into()
    } else {
        matches.join("\n")
    }
}
