use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::tools::{blocking_string, parse_args, req_str, AgentLoopRuntime, ToolHandler};
use crate::core::paths::{io_enforce, resolve};
use crate::landscape::{format_landscape_block_tool, format_outline_items, LandscapeExplorer};

pub(crate) struct LandscapeTool;

#[async_trait]
impl ToolHandler for LandscapeTool {
    async fn invoke(&self, _id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        // This registry handler only runs when no LandscapeContext served the
        // call. Provide the same directory/file outline behavior as the context
        // so Landscape keeps working even for a stage without that context.
        let cwd = ctx.cwd.clone();
        let roots = ctx.sandbox.roots().map(|r| r.to_vec());
        let args_json = args_json.to_string();
        blocking_string(move || landscape_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
    }
}

fn landscape_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
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

    let depth: Option<usize> = match args.get("depth") {
        None => None,
        Some(v) => match v.as_u64() {
            Some(n) => Some(n as usize),
            None => return "Error: 'depth' must be a non-negative integer".into(),
        },
    };

    let token_budget: Option<usize> = match args.get("token_budget").filter(|v| !v.is_null()) {
        None => crate::config::max_landscape_tool_tokens().map(|n| n as usize),
        Some(v) => match v.as_u64() {
            Some(n) => Some(n as usize),
            None => return "Error: 'token_budget' must be a non-negative integer".into(),
        },
    };

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

    if meta.is_dir() {
        let build = match LandscapeExplorer.build_with_failures(&path, depth.unwrap_or(usize::MAX))
        {
            Ok(b) => b,
            Err(e) => return format!("Error: {e}"),
        };
        if build.outlines.is_empty() && build.failures.is_empty() {
            return "(empty directory)".into();
        }
        match format_landscape_block_tool(&build, &path, token_budget) {
            Ok(block) => {
                if block.block.is_empty() {
                    "(empty directory)".into()
                } else {
                    block.block
                }
            }
            Err(crate::landscape::LandscapeError::BudgetExceeded) => {
                "Error: budget exceeded".into()
            }
            Err(e) => format!("Error: {e}"),
        }
    } else {
        match LandscapeExplorer.outline_file(&path, false) {
            Ok(items) => {
                if items.is_empty() {
                    path.display().to_string()
                } else {
                    format_outline_items(&items)
                }
            }
            Err(crate::landscape::LandscapeError::Utf8(e)) => {
                format!("Error: file is not valid UTF-8: {e}")
            }
            Err(crate::landscape::LandscapeError::Read(e)) => format!("Error: {e}"),
            Err(crate::landscape::LandscapeError::BudgetExceeded) => {
                "Error: budget exceeded".into()
            }
            Err(crate::landscape::LandscapeError::Tokenizer(e)) => {
                format!("Error: tokenizer init failed: {e}")
            }
        }
    }
}
