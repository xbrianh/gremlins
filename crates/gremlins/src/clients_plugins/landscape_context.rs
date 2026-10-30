use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::core::logging::send_log;
use crate::landscape::{LandscapeData, LandscapeExplorer};

/// Mutable model context that maintains a living landscape of the worktree.
///
/// Owns a [`LandscapeData`] holding the full-tree outlines and signal sections
/// from stage start, plus a per-file cache that is refreshed on tool touches.
///
/// On each tool observation (Read/Grep/Landscape/Write/Edit/Glob), the touched
/// file (or directory) is re-explored on disk so the landscape always reflects
/// current file state.
///
/// The landscape is rendered into the system prompt via [`system_message`] once
/// at construction and never updated — it is a frozen seed that may become stale.
#[derive(Clone)]
pub(crate) struct LandscapeContext {
    /// The living landscape data — always up to date.
    data: LandscapeData,

    /// Worktree root, copied from `data.root` at construction.
    /// Used by [`tool_result_extra`] so it never touches `self.data`.
    root: PathBuf,

    /// Frozen seed landscape captured at construction — never changes.
    /// Served via [`system_message`].
    seed_landscape: String,

    /// File paths touched during the current turn (cleared each turn).
    dirty: HashSet<PathBuf>,

    /// Whether landscape was ever enabled for this stage.
    /// Set from `data.is_empty()` at construction — when false, all
    /// observations are no-ops and transcripts are always empty.
    landscape_enabled: bool,
    /// Per-gremlin log channel for landscape statistics.
    log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl LandscapeContext {
    /// Create a new LandscapeContext from the stage-start landscape data.
    ///
    /// Calls [`LandscapeData::seed`] to populate the initial file entries.
    /// When `data` is empty (no outlines, no signal sections), no seeding is
    /// performed and all transcripts will be empty.
    pub(crate) fn new(
        mut data: LandscapeData,
        log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Self {
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

        let ctx = Self {
            data,
            root,
            seed_landscape,
            dirty: HashSet::new(),
            landscape_enabled,
            log_tx,
        };
        ctx.log_stats();
        ctx
    }

    /// Observe a tool call, touching the appropriate file or directory.
    ///
    /// Read/Write/Edit/Grep/Glob/Landscape all supply a path argument.
    /// We resolve the path, then operate on the file if it's a file (or
    /// doesn't exist yet), or landscape the directory if it's a directory.
    fn observe_tool(&mut self, _turn: usize, tool: &str, args_json: &str) {
        // When landscape was never enabled, every observation is a no-op.
        if !self.landscape_enabled {
            return;
        }

        let args: serde_json::Value =
            serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);

        let raw = |key: &str| -> Option<String> {
            args.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        };

        // Extract the relevant path argument based on the tool.
        let path_str = match tool {
            "Read" | "Write" | "Edit" | "Landscape" => raw("file_path"),
            "Grep" | "Glob" => raw("path"),
            _ => None,
        };

        if let Some(path_str) = path_str {
            let path = self.resolve_path(&path_str);
            if !path.as_os_str().is_empty() {
                let explorer = LandscapeExplorer;
                if path.is_dir() {
                    self.data.touch_dir(&path, &explorer);
                    // Mark all files currently in this directory as dirty
                    // so the delta captures the full updated listing.
                    for file_path in self.data.file_entries.keys() {
                        if file_path.parent() == Some(path.as_path()) {
                            self.dirty.insert(file_path.clone());
                        }
                    }
                } else {
                    self.data.touch_file(&path, &explorer);
                    self.dirty.insert(path);
                }
                self.log_stats();
            }
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


    /// Emit landscape statistics.
    fn log_stats(&self) {
        if !self.landscape_enabled {
            return;
        }

        let dir_count = self
            .data
            .file_entries
            .keys()
            .filter_map(|p| p.parent())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let file_count = self.data.file_entries.len();
        let item_count: usize = self.data.file_entries.values().map(|e| e.items.len()).sum();

        send_log(
            &self.log_tx,
            log::Level::Debug,
            format!("landscape context updated: {dir_count} directories, {file_count} files, {item_count} outline items"),
        );
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
            "Read" | "Write" | "Edit" | "Landscape" => raw("file_path"),
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
        // Clear per-turn dirty set for the next turn.
        self.dirty.clear();
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

    fn join(&mut self, other: &dyn crate::clients::model_context::MutableModelContext) {
        if let Some(other_ctx) = other.as_any().downcast_ref::<LandscapeContext>() {
            // Merge file entries: copy child entries into the parent.
            for (path, entry) in &other_ctx.data.file_entries {
                self.data.file_entries.insert(path.clone(), entry.clone());
            }

            // Merge file entries but leave our frozen seed landscape unchanged.
            if !other_ctx.seed_landscape.is_empty() {
                self.log_stats();
            }
        }
    }

    fn clear_transcript(&mut self) {}
}