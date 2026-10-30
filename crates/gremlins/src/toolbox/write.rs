use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::tools::{blocking_string, parse_args, req_str, AgentLoopRuntime, ToolHandler};
use crate::core::paths::{io_enforce, resolve};

pub(crate) struct WriteTool;

#[async_trait]
impl ToolHandler for WriteTool {
    async fn invoke(&self, _id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        write_invoke(ctx, args_json).await
    }
}

pub(crate) async fn write_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || write_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn write_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let file_path = match req_str(&args, "file_path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let content = match req_str(&args, "content") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = resolve(file_path, cwd);
    if let Some(roots) = roots {
        if let Some(err) = io_enforce(&path, roots) {
            return err;
        }
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("Error: {e}");
        }
    }
    match std::fs::write(&path, content) {
        Ok(()) => "OK".into(),
        Err(e) => format!("Error: {e}"),
    }
}
