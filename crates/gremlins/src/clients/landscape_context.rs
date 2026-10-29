use std::path::{Path, PathBuf};

use crate::executor::send_log;
use crate::landscape::{LandscapeData, LandscapeExplorer};

/// Mutable model context that maintains a living landscape of the worktree.
///
/// Owns a [`LandscapeData`] holding the full-tree outlines and signal sections
/// from stage start, plus a per-file cache that is refreshed on tool touches
/// and pruned by turn decay.
///
/// On each tool observation (Read/Grep/Landscape/Write/Edit/Glob), the touched
/// file (or directory) is re-explored on disk so the landscape always reflects
/// current file state. Files untouched for more than `decay_turns` turns are
/// pruned at [`finalize_turn`]. When `decay_turns` is 0 pruning is disabled
/// entirely.
#[derive(Clone)]
pub(crate) struct LandscapeContext {
    /// The living landscape data.
    data: LandscapeData,
    /// Per-turn transcript blocks (indexed by turn number).
    turn_transcripts: Vec<String>,
    /// Token budget for landscape rendering.
    max_tokens: usize,
    /// Turn-decay window: files untouched for more turns than this are pruned.
    /// When `0`, pruning is disabled.
    decay_turns: usize,
    /// Whether landscape was ever enabled for this stage.
    /// Set from `data.is_empty()` at construction — when false, all observations
    /// are no-ops and transcripts are always empty.  Remains false even after
    /// every file entry is decay-pruned, so re-touches can re-explore.
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
        let decay_turns = usize::try_from(crate::config::landscape_decay_turns()).unwrap_or(5);

        let mut ctx = Self {
            data,
            turn_transcripts: Vec::new(),
            max_tokens,
            decay_turns,
            landscape_enabled,
            log_tx,
        };
        // Seed turn 0 so the transcript length invariant holds before the
        // first finalize_turn call (mirrors ToolLedger seeding).
        ctx.capture_turn_transcript(0);
        ctx.log_stats();
        ctx
    }

    /// Observe a tool call, touching the appropriate file or directory.
    ///
    /// Read/Write/Edit/Landscape touch the specific file (re-outlining it via
    /// `LandscapeExplorer`); Grep/Glob/Landscape-on-a-directory touch the
    /// directory (re-exploring its direct children).
    fn observe_tool(&mut self, turn: usize, tool: &str, args_json: &str) {
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

        // Each entry is `(touch_as_file, path)`. Paths are resolved against
        // the worktree root so file_entries keys stay absolute and comparable.
        let target: Option<(bool, PathBuf)> = match tool {
            "Read" | "Write" | "Edit" => raw("file_path").map(|p| (true, self.resolve_path(&p))),
            "Grep" | "Glob" => raw("path").map(|p| {
                let resolved = self.resolve_path(&p);
                if resolved.is_dir() {
                    (false, resolved)
                } else {
                    (false, self.parent_or_root(&resolved))
                }
            }),
            "Landscape" => raw("file_path").map(|p| {
                let resolved = self.resolve_path(&p);
                if resolved.is_dir() {
                    (false, resolved)
                } else {
                    (true, resolved)
                }
            }),
            _ => None,
        };

        if let Some((touch_as_file, path)) = target {
            if !path.as_os_str().is_empty() {
                let explorer = LandscapeExplorer;
                if touch_as_file {
                    self.data.touch_file(&path, &explorer, turn);
                } else {
                    self.data.touch_dir(&path, &explorer, turn);
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

    /// Return `path`'s parent, falling back to the worktree root.
    fn parent_or_root(&self, path: &Path) -> PathBuf {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| self.data.root.clone())
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

    /// Capture the current formatted landscape state as a turn transcript
    /// block, making it the sole index that carries landscape content.
    fn capture_turn_transcript(&mut self, turn_num: usize) {
        while self.turn_transcripts.len() <= turn_num {
            self.turn_transcripts.push(String::new());
        }
        // Clear all prior entries — only the tail carries landscape.
        for entry in &mut self.turn_transcripts {
            *entry = String::new();
        }
        let rendered = if self.data.file_entries.is_empty() && self.data.signal_sections.is_empty()
        {
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
        vec![None; self.turn_transcripts.len()]
    }

    fn system_message(&self) -> Option<String> {
        if self.data.file_entries.is_empty() && self.data.signal_sections.is_empty() {
            None
        } else {
            Some(crate::landscape::format_landscape_data(
                &self.data,
                self.max_tokens,
            ))
        }
    }

    fn join(&mut self, other: &dyn super::model_context::MutableModelContext) {
        if let Some(other_ctx) = other.as_any().downcast_ref::<LandscapeContext>() {
            // Merge file entries: re-stamp child entries with the parent's
            // current turn so they share the parent's decay clock rather than
            // the child's independent turn space.
            let current_turn = self.turn_transcripts.len().saturating_sub(1);
            for (path, entry) in &other_ctx.data.file_entries {
                let should_insert = match self.data.file_entries.get(path) {
                    Some(existing) => existing.last_touched_turn < entry.last_touched_turn,
                    None => true,
                };
                if should_insert {
                    let mut new_entry = entry.clone();
                    new_entry.last_touched_turn = current_turn;
                    self.data.file_entries.insert(path.clone(), new_entry);
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
