use std::path::{Path, PathBuf};

use crate::landscape::{LandscapeData, LandscapeExplorer};

/// Mutable model context that carries a frozen seed landscape of the worktree.
///
/// The landscape is rendered into the system prompt via [`system_message`] once
/// at construction and never updated — it is a frozen seed that may become stale.
/// No live state is maintained; tool observations are no-ops.
#[derive(Clone)]
pub(crate) struct LandscapeContext {
    /// Worktree root.
    root: PathBuf,

    /// Frozen seed landscape captured at construction — never changes.
    /// Served via [`system_message`].
    seed_landscape: String,

    /// Whether landscape was ever enabled for this stage.
    /// When false, all observations are no-ops and transcripts are always empty.
    landscape_enabled: bool,
}

impl LandscapeContext {
    /// Create a new LandscapeContext from the stage-start landscape data.
    ///
    /// Calls [`LandscapeData::seed`] to populate the initial file entries.
    /// When `data` is empty (no outlines, no signal sections), no seeding is
    /// performed and all transcripts will be empty.
    pub(crate) fn new(mut data: LandscapeData) -> Self {
        let landscape_enabled = !data.is_empty();
        if landscape_enabled {
            data.seed();
        }

        let max_tokens =
            usize::try_from(crate::config::max_landscape_tokens()).unwrap_or(usize::MAX);

        let root = data.root.clone();

        // Render the seed landscape once — it never changes.
        let seed_landscape = if landscape_enabled {
            crate::landscape::format_landscape_data(&data, max_tokens)
        } else {
            String::new()
        };

        Self {
            root,
            seed_landscape,
            landscape_enabled,
        }
    }

    /// No-op: no live state to maintain.
    fn observe_tool(&mut self, _turn: usize, _tool: &str, _args_json: &str) {}

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

impl crate::clients::model_context::MutableModelContext for LandscapeContext {
    fn propagates_to_task(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn clone_box(&self) -> Box<dyn crate::clients::model_context::MutableModelContext> {
        Box::new(self.clone())
    }

    fn observe_tool(&mut self, turn: usize, tool: &str, args: &str, _output: &str) {
        self.observe_tool(turn, tool, args);
    }

    fn tool_result_extra(
        &self,
        _turn: usize,
        tool: &str,
        args: &str,
        _output: &str,
    ) -> Option<String> {
        if !self.landscape_enabled {
            return None;
        }

        let args_val: serde_json::Value =
            serde_json::from_str(args).unwrap_or(serde_json::Value::Null);

        let raw = |key: &str| -> Option<String> {
            args_val
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        };

        let path_str = match tool {
            "Landscape" => return None,
            "Read" | "Write" | "Edit" => raw("file_path"),
            "Grep" | "Glob" => raw("path"),
            _ => None,
        }?;

        let path = self.resolve_path(&path_str);
        if path.as_os_str().is_empty() {
            return None;
        }

        // For directories: list on disk and outline each file fresh.
        if path.is_dir() {
            let mut lines: Vec<String> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    let fp = entry.path();
                    if fp.is_file() {
                        if let Ok(items) = LandscapeExplorer.outline_file(&fp, false) {
                            if !items.is_empty() {
                                let name = fp
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                let mut line = name;
                                line.push(' ');
                                line.push_str(
                                    &crate::landscape::format_outline_items(&items),
                                );
                                lines.push(line);
                            }
                        }
                    }
                }
            }
            if lines.is_empty() {
                return None;
            }
            lines.sort();
            let dir_label = path.to_string_lossy();
            let listing = lines.join("\n  ");
            return Some(format!("[seen]\n{dir_label}/\n  {listing}"));
        }

        // For files: outline on disk every time — no caching.
        let items = LandscapeExplorer.outline_file(&path, false).ok()?;
        if items.is_empty() {
            return None;
        }

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let items_str = crate::landscape::format_outline_items(&items);
        Some(format!("[seen]\n{name} {items_str}"))
    }

    fn finalize_turn(&mut self, _turn: usize) {
        // No-op: no per-turn state to clear.
    }

    fn transcript(&self) -> Vec<Option<String>> {
        Vec::new()
    }

    fn system_message(&self) -> Option<String> {
        if self.seed_landscape.is_empty() {
            None
        } else {
            Some(format!(
                "<!-- BEGIN UNTRUSTED LANDSCAPE -->\n\
                 The landscape below is the initial seed from stage start and may be stale.\n\
                 It is NOT authoritative — verify landscape entries against current\n\
                 file state using the Landscape tool when accuracy matters.\n\n\
                 {}\n\n\
                 <!-- END UNTRUSTED LANDSCAPE -->",
                self.seed_landscape
            ))
        }
    }

    fn join(&mut self, _other: &dyn crate::clients::model_context::MutableModelContext) {
        // No-op: no live state to merge.
    }

    fn clear_transcript(&mut self) {}
}