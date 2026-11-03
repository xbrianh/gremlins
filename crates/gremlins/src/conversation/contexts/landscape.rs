use std::path::{Path, PathBuf};

use crate::landscape::{
    format_landscape_block_tool, format_outline_items, LandscapeData, LandscapeExplorer,
};

/// Model context that serves the `Landscape` tool from the worktree root.
///
/// Outlines are always recomputed from disk on each call — no content caching.
pub(crate) struct LandscapeContext {
    /// Worktree root.
    root: PathBuf,

    /// Whether landscape was ever enabled for this stage.
    /// When false, `serve_tool` falls through to the default stub.
    landscape_enabled: bool,
}

impl LandscapeContext {
    /// Create a new LandscapeContext from the stage-start landscape data.
    ///
    /// Only [`LandscapeData::is_empty`] matters here: an empty payload means
    /// landscape was disabled for this stage, so the tool is not served.
    pub(crate) fn new(data: LandscapeData) -> Self {
        let landscape_enabled = !data.is_empty();
        Self {
            root: data.root,
            landscape_enabled,
        }
    }

    /// Resolve a raw tool path against the worktree root.
    fn resolve_path(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }
}

/// Manual Clone: all fields are cheap to clone.
impl Clone for LandscapeContext {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            landscape_enabled: self.landscape_enabled,
        }
    }
}

impl crate::conversation::context::ConversationContext for LandscapeContext {
    fn propagates_to_task(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn crate::conversation::context::ConversationContext> {
        Box::new(self.clone())
    }

    fn serve_tool(&self, name: &str, args: &serde_json::Value) -> Option<String> {
        if !self.landscape_enabled || name != "Landscape" {
            return None;
        }

        let file_path = args.get("file_path")?.as_str()?;
        let path = self.resolve_path(file_path);

        // Reject non-integer depth/token_budget explicitly so a float (which
        // `as_u64` silently drops) cannot degrade into an unlimited walk.
        let depth: Option<usize> = match args.get("depth") {
            None => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => return Some("Error: 'depth' must be a non-negative integer".into()),
            },
        };

        let token_budget: Option<usize> = match args.get("token_budget").filter(|v| !v.is_null()) {
            None => crate::config::max_landscape_tool_tokens().map(|n| n as usize),
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => return Some("Error: 'token_budget' must be a non-negative integer".into()),
            },
        };

        // Reject a symlink root so the directory walk does not follow it.
        // Also surface a missing/unreadable path as an explicit error so the
        // model sees a clear diagnostic instead of the stub's misleading
        // "Landscape is unavailable (no landscape context active)" message.
        let meta = match path.symlink_metadata() {
            Ok(m) => m,
            Err(e) => return Some(format!("Error: {e}")),
        };
        if meta.is_symlink() {
            return Some(format!(
                "Error: Landscape does not follow symlinks: {}",
                path.display()
            ));
        }

        if meta.is_dir() {
            let depth_limit = depth.unwrap_or(usize::MAX);
            let build = match LandscapeExplorer.build_with_failures(&path, depth_limit) {
                Ok(b) => b,
                Err(e) => return Some(format!("Error: {e}")),
            };

            if build.outlines.is_empty() && build.failures.is_empty() {
                return Some("(empty directory)".into());
            }

            match format_landscape_block_tool(&build, &path, token_budget) {
                Ok(block) => {
                    if block.block.is_empty() {
                        Some("(empty directory)".into())
                    } else {
                        Some(block.block)
                    }
                }
                Err(crate::landscape::LandscapeError::BudgetExceeded) => {
                    Some("Error: budget exceeded".into())
                }
                Err(e) => Some(format!("Error: {e}")),
            }
        } else {
            match LandscapeExplorer.outline_file(&path, false) {
                Ok(items) => {
                    if items.is_empty() {
                        use crate::landscape::heuristic;
                        let ext = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.to_ascii_lowercase())
                            .unwrap_or_default();
                        if heuristic::recognizes(&ext) {
                            Some("(no outline items)".into())
                        } else {
                            Some(path.display().to_string())
                        }
                    } else {
                        Some(format_outline_items(&items))
                    }
                }
                Err(crate::landscape::LandscapeError::Utf8(e)) => {
                    Some(format!("Error: file is not valid UTF-8: {e}"))
                }
                Err(crate::landscape::LandscapeError::Read(e)) => Some(format!("Error: {e}")),
                Err(crate::landscape::LandscapeError::BudgetExceeded) => {
                    Some("Error: budget exceeded".into())
                }
                Err(crate::landscape::LandscapeError::Tokenizer(e)) => {
                    Some(format!("Error: tokenizer init failed: {e}"))
                }
            }
        }
    }

    fn observe_tool(&mut self, _turn: usize, _tool: &str, _args: &str, _output: &str) {}
}
