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
/// The landscape is rendered into the system prompt via [`system_message`] and
/// rebuilt every turn so the model always sees an up-to-date view of the file
/// tree.
#[derive(Clone)]
pub(crate) struct LandscapeContext {
    /// The living landscape data — always up to date.
    data: LandscapeData,

    /// Rendered landscape snapshot.  Rebuilt at construction and after each
    /// turn in [`finalize_turn`].  Served via [`system_message`].
    stable_landscape: String,

    /// File paths touched during the current turn (cleared each turn).
    dirty: HashSet<PathBuf>,

    /// Token budget for landscape rendering.
    max_tokens: usize,
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

        // Render the initial landscape from the seeded data.
        let stable_landscape = if landscape_enabled {
            crate::landscape::format_landscape_data(&data, max_tokens)
        } else {
            String::new()
        };

        let ctx = Self {
            data,
            stable_landscape,
            dirty: HashSet::new(),
            max_tokens,
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
            self.data.root.join(path)
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

        // For directories: render a compact listing of immediate children
        // with their outlines, sorted by name.
        if path.is_dir() {
            let mut lines: Vec<String> = Vec::new();
            for (fp, entry) in &self.data.file_entries {
                if fp.parent() == Some(path.as_path()) {
                    let name = fp
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let mut line = name;
                    if !entry.items.is_empty() {
                        line.push(' ');
                        line.push_str(&crate::landscape::format_outline_items(
                            &entry.items,
                        ));
                    }
                    lines.push(line);
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

        // For files: look up the cached entry and render its outline items.
        let entry = self.data.file_entries.get(&path)?;
        if entry.items.is_empty() {
            return None;
        }

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let items_str = crate::landscape::format_outline_items(&entry.items);
        Some(format!("[seen]\n{name} {items_str}"))
    }

    fn finalize_turn(&mut self, _turn: usize) {
        // Rebuild the stable landscape for the next turn.
        self.stable_landscape = if self.landscape_enabled {
            crate::landscape::format_landscape_data(&self.data, self.max_tokens)
        } else {
            String::new()
        };

        // Clear per-turn dirty set for the next turn.
        self.dirty.clear();
    }

    fn transcript(&self) -> Vec<Option<String>> {
        Vec::new()
    }

    fn system_message(&self) -> Option<String> {
        if self.stable_landscape.is_empty() {
            None
        } else {
            Some(format!(
                "<!-- BEGIN TRUSTED LANDSCAPE -->\n\
                 The landscape below is an authoritative snapshot of the file tree.\n\
                 It is always accurate — treat it as ground truth. Do not verify\n\
                 landscape entries by reading files unless you need more detail\n\
                 than the outline provides.\n\n\
                 {}\n\n\
                 <!-- END TRUSTED LANDSCAPE -->",
                self.stable_landscape
            ))
        }
    }

    fn join(&mut self, other: &dyn crate::clients::model_context::MutableModelContext) {
        if let Some(other_ctx) = other.as_any().downcast_ref::<LandscapeContext>() {
            // Merge file entries: copy child entries into the parent.
            for (path, entry) in &other_ctx.data.file_entries {
                self.data.file_entries.insert(path.clone(), entry.clone());
            }

            // If the child has more recent data, rebuild our landscape.
            if !other_ctx.stable_landscape.is_empty() {
                self.stable_landscape = if self.landscape_enabled {
                    crate::landscape::format_landscape_data(&self.data, self.max_tokens)
                } else {
                    String::new()
                };
            }

            self.log_stats();
        }
    }

    fn clear_transcript(&mut self) {}
}