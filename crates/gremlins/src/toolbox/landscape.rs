use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::conversation::Conversation;
use crate::clients::tools::{
    blocking_string, parse_args, req_str, AgentLoopRuntime, ToolHandler, ToolOutcome,
};
use crate::core::paths::{enforce, io_enforce, resolve};
use crate::landscape::{format_landscape_block_tool, format_outline_items, LandscapeExplorer};

pub(crate) struct LandscapeTool;

#[async_trait]
impl ToolHandler for LandscapeTool {
    fn title_detail(&self, args: &serde_json::Value) -> Option<String> {
        let path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| format!("file_path={s:?}"));
        let depth = args
            .get("depth")
            .and_then(|v| v.as_u64())
            .map(|d| format!("depth={d}"));
        match (path, depth) {
            (Some(p), Some(d)) => Some(format!("{p}, {d}")),
            (Some(p), None) => Some(p),
            _ => None,
        }
    }

    fn sandbox_check(&self, ctx: &AgentLoopRuntime, args: &serde_json::Value) -> Option<String> {
        let roots = ctx.sandbox.roots()?;
        enforce(
            roots,
            args.get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("."),
            ctx.cwd.as_deref(),
        )
    }

    fn output_cap(&self) -> bool {
        true
    }

    async fn invoke(
        &self,
        _id: &str,
        ctx: &AgentLoopRuntime,
        _conv: &dyn Conversation,
        args_json: &str,
    ) -> ToolOutcome {
        // This registry handler only runs when no LandscapeContext served the
        // call. Provide the same directory/file outline behavior as the context
        // so Landscape keeps working even for a stage without that context.
        let cwd = ctx.cwd.clone();
        let roots = ctx.sandbox.roots().map(|r| r.to_vec());
        let args_json = args_json.to_string();
        let out =
            blocking_string(move || landscape_sync(cwd.as_deref(), roots.as_deref(), &args_json))
                .await;
        ToolOutcome::text(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// No config and no explicit `token_budget` → unlimited (no cap error).
    #[test]
    fn token_budget_no_config_no_explicit_is_unlimited() {
        let _sandbox = crate::test_support::Sandbox::new();
        crate::config::clear_global();
        crate::config::init_global().unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}").unwrap();
        let result = landscape_sync(Some(dir.path()), None, r#"{"file_path":"."}"#);
        assert!(!result.contains("Error: budget exceeded"), "{result}");
        assert!(!result.is_empty());
        crate::config::clear_global();
    }

    /// Caller supplies `token_budget` → it overrides the config default.
    #[test]
    fn token_budget_explicit_overrides() {
        let _sandbox = crate::test_support::Sandbox::with_config(Some(
            r#"{"landscape": {"max-tool-tokens": 0}}"#,
        ));
        crate::config::clear_global();
        crate::config::init_global().unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn main() {}\nstruct S;\nimpl S {}",
        )
        .unwrap();
        let result = landscape_sync(
            Some(dir.path()),
            None,
            r#"{"file_path":".","token_budget":999999}"#,
        );
        assert!(!result.contains("Error: budget exceeded"), "{result}");
        assert!(!result.is_empty());
        crate::config::clear_global();
    }

    /// Token budget of zero returns `Error: budget exceeded`.
    #[test]
    fn token_budget_zero_exceeds() {
        let _sandbox = crate::test_support::Sandbox::new();
        crate::config::clear_global();
        crate::config::init_global().unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}").unwrap();
        let result = landscape_sync(
            Some(dir.path()),
            None,
            r#"{"file_path":".","token_budget":0}"#,
        );
        assert!(result.contains("Error: budget exceeded"), "{result}");
        crate::config::clear_global();
    }

    /// With no explicit `token_budget`, the configured default
    /// (`landscape.max-tool-tokens`) applies. A zero default caps everything,
    /// proving the fallback is consulted.
    #[test]
    fn token_budget_falls_back_to_config_default() {
        let _sandbox = crate::test_support::Sandbox::with_config(Some(
            r#"{"landscape": {"max-tool-tokens": 0}}"#,
        ));
        crate::config::clear_global();
        crate::config::init_global().unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}").unwrap();
        let result = landscape_sync(Some(dir.path()), None, r#"{"file_path":"."}"#);
        assert!(result.contains("Error: budget exceeded"), "{result}");

        crate::config::clear_global();
    }
}
