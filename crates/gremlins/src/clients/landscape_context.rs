use std::path::{Path, PathBuf};

use crate::landscape::{LandscapeData, LandscapeExplorer};

/// Mutable model context that maintains a living landscape of the worktree.
///
/// Owns a [`LandscapeData`] holding the full-tree outlines and signal sections
/// from stage start, plus per-directory cached file listings that are refreshed
/// on tool touches and pruned by turn decay.
///
/// On each tool observation (Read/Grep/Landscape/Write/Edit/Glob), the tool's
/// parent directory is re-explored on disk so the landscape always reflects
/// current file state. Directories untouched for more than `decay_turns`
/// turns are pruned at [`finalize_turn`]. When `decay_turns` is 0 pruning
/// is disabled entirely.
#[derive(Clone)]
pub(crate) struct LandscapeContext {
    /// The living landscape data.
    data: LandscapeData,
    /// Per-turn transcript blocks (indexed by turn number).
    turn_transcripts: Vec<String>,
    /// Token budget for landscape rendering.
    max_tokens: usize,
    /// Turn-decay window: directories untouched for more turns than this are pruned.
    /// When `0`, pruning is disabled.
    decay_turns: usize,
    /// Whether landscape was ever enabled for this stage.
    /// Set from `data.is_empty()` at construction — when false, all observations
    /// are no-ops and transcripts are always empty.  Remains false even after
    /// every directory entry is decay-pruned, so re-touches can re-explore.
    landscape_enabled: bool,
}

impl LandscapeContext {
    /// Create a new LandscapeContext from the stage-start landscape data.
    ///
    /// Calls [`LandscapeData::seed`] to populate the initial directory entries.
    /// When `data` is empty (no outlines, no signal sections), no seeding is
    /// performed and all transcripts will be empty.
    pub(crate) fn new(mut data: LandscapeData) -> Self {
        let landscape_enabled = !data.is_empty();
        if landscape_enabled {
            data.seed();
        }

        let max_tokens =
            usize::try_from(crate::config::max_landscape_tokens()).unwrap_or(usize::MAX);
        let decay_turns = usize::try_from(crate::config::landscape_decay_turns()).unwrap_or(5);

        let mut ctx = Self {
            data,
            turn_transcripts: Vec::new(),
            max_tokens,
            decay_turns,
            landscape_enabled,
        };
        // Seed turn 0 so the transcript length invariant holds before the
        // first finalize_turn call (mirrors ToolLedger seeding).
        ctx.capture_turn_transcript(0);
        ctx.log_stats();
        ctx
    }

    /// Observe a tool call, touching the appropriate directory.
    ///
    /// Read/Grep/Landscape/Write/Edit/Glob all touch the tool's immediate
    /// parent directory. The directory is re-explored on disk via
    /// `LandscapeExplorer`.
    fn observe_tool(&mut self, turn: usize, tool: &str, args_json: &str) {
        // When landscape was never enabled, every observation is a no-op.
        if !self.landscape_enabled {
            return;
        }

        let args: serde_json::Value =
            serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);

        let dir: Option<PathBuf> = match tool {
            "Read" | "Write" | "Edit" => {
                let file_path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
                if file_path.is_empty() {
                    None
                } else {
                    Path::new(file_path).parent().map(|p| p.to_path_buf())
                }
            }
            "Grep" | "Glob" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    None
                } else {
                    let p = Path::new(path);
                    if p.is_dir() {
                        Some(p.to_path_buf())
                    } else {
                        // `p.parent()` returns `Some("")` for `"."` on Unix —
                        // fall back to `"."` so the root directory is touched.
                        p.parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .map(|parent| parent.to_path_buf())
                            .or_else(|| Some(PathBuf::from(".")))
                    }
                }
            }
            "Landscape" => {
                let file_path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
                if file_path.is_empty() {
                    None
                } else {
                    // The Landscape tool resolves `file_path` against the
                    // worktree root and accepts either a directory or a file.
                    // Mirror that here: touch the directory itself, or the
                    // file's parent directory, so the living landscape cache
                    // is refreshed for exactly the location the tool explored.
                    let path = Path::new(file_path);
                    let resolved = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        self.data.root.join(path)
                    };
                    if resolved.is_dir() {
                        Some(resolved)
                    } else {
                        resolved
                            .parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .map(|parent| parent.to_path_buf())
                            .or_else(|| Some(self.data.root.clone()))
                    }
                }
            }
            _ => None,
        };

        if let Some(ref dir) = dir {
            if !dir.as_os_str().is_empty() {
                let explorer = LandscapeExplorer;
                self.data.touch_dir(dir, &explorer, turn);
                self.log_stats();
            }
        }
    }

    /// Emit landscape statistics at info level, plus the full directory list
    /// at debug level.
    fn log_stats(&self) {
        if !self.landscape_enabled {
            return;
        }

        let dir_count = self.data.directory_entries.len();
        let file_count: usize = self
            .data
            .directory_entries
            .values()
            .map(|e| e.sorted_files.len())
            .sum();
        let item_count: usize = self
            .data
            .directory_entries
            .values()
            .map(|e| e.outline_items.iter().map(|(_, items)| items.len()).sum::<usize>())
            .sum();

        log::info!(
            "landscape context updated: {dir_count} directories, {file_count} files, {item_count} outline items"
        );

        let mut dirs: Vec<&std::path::PathBuf> = self.data.directory_entries.keys().collect();
        dirs.sort();
        log::debug!(
            "landscape context directories:\n{}",
            dirs.iter()
                .map(|d| format!("  {}", d.display()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Capture the current formatted landscape state as a turn transcript block.
    fn capture_turn_transcript(&mut self, turn_num: usize) {
        while self.turn_transcripts.len() <= turn_num {
            self.turn_transcripts.push(String::new());
        }
        let rendered = if self.data.directory_entries.is_empty() {
            String::new()
        } else {
            crate::landscape::format_landscape_data(&self.data, self.max_tokens)
        };
        self.turn_transcripts[turn_num] = rendered;
    }
}

impl super::model_context::MutableModelContext for LandscapeContext {
    fn propagates_to_task(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn clone_box(&self) -> Box<dyn super::model_context::MutableModelContext> {
        Box::new(self.clone())
    }

    fn observe(&mut self, turn: usize, tool: &str, args: &str, _output: &str) {
        self.observe_tool(turn, tool, args);
    }

    fn finalize_turn(&mut self, turn: usize) {
        // Prune decayed directories. When decay_turns is 0 the guard
        // disables pruning entirely so every entry persists regardless
        // of the time since its last touch.
        if self.decay_turns > 0 {
            self.data.prune_decayed(turn, self.decay_turns);
        }
        // Capture the transcript for this turn.
        self.capture_turn_transcript(turn);
    }

    fn transcript(&self) -> Vec<Option<String>> {
        self.turn_transcripts
            .iter()
            .map(|s| if s.is_empty() { None } else { Some(s.clone()) })
            .collect()
    }

    fn join(&mut self, other: &dyn super::model_context::MutableModelContext) {
        if let Some(other_ctx) = other.as_any().downcast_ref::<LandscapeContext>() {
            // Merge directory entries: re-stamp child entries with the
            // parent's current turn so they share the parent's decay
            // clock rather than the child's independent turn space.
            let current_turn = self.turn_transcripts.len().saturating_sub(1);
            for (dir, entry) in &other_ctx.data.directory_entries {
                let should_insert = match self.data.directory_entries.get(dir) {
                    Some(existing) => existing.last_touched_turn < entry.last_touched_turn,
                    None => true,
                };
                if should_insert {
                    let mut new_entry = entry.clone();
                    new_entry.last_touched_turn = current_turn;
                    self.data.directory_entries.insert(dir.clone(), new_entry);
                }
            }
            // Append child transcripts at the parent's current turn index
            // rather than interleaving by the child's (unrelated) indices.
            if self.turn_transcripts.is_empty() {
                self.turn_transcripts.push(String::new());
            }
            let parent_idx = self.turn_transcripts.len() - 1;
            for theirs in &other_ctx.turn_transcripts {
                if !theirs.is_empty() && self.turn_transcripts[parent_idx].is_empty() {
                    self.turn_transcripts[parent_idx] = theirs.clone();
                }
            }
            self.log_stats();
        }
    }
}
