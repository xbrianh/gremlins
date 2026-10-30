use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::tools::{blocking_string, parse_args, req_str, AgentLoopRuntime, ToolHandler};
use crate::core::paths::{io_enforce, resolve};

pub(crate) struct ReadTool;

#[async_trait]
impl ToolHandler for ReadTool {
    async fn invoke(&self, _id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        read_invoke(ctx, args_json).await
    }
}

pub(crate) async fn read_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || read_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn read_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
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
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    if content.is_empty() {
        return String::new();
    }
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let mut lines: Vec<&str> = content.split_inclusive('\n').collect();
    if offset > 0 {
        lines = lines.into_iter().skip(offset).collect();
    }
    if let Some(limit) = limit {
        lines.truncate(limit);
    }
    let mut numbered = String::new();
    for (idx, line) in lines.iter().enumerate() {
        // `offset` is a 0-indexed skip count, so the file line number is
        // offset + window index + 1 — never the window-relative index.
        numbered.push_str(&format!("{}|{line}", offset + idx + 1));
    }
    numbered
}
