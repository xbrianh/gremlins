use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::core::logging::send_log;
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
///
/// # Deferred reconciliation
///
/// Instead of invalidating the system prompt (and its Anthropic cache) on
/// every file touch, landscape updates are deferred into per-turn transcript
/// deltas. The system prompt carries a frozen `stable_landscape` snapshot that
/// persists across turns. Deltas accumulate in transcript blocks; when their
/// combined token count exceeds `reconcile_threshold`, the stable landscape is
/// rebuilt from the full current [`LandscapeData`] and all deltas are cleared.
/// This trades occasional cache misses (at reconcile) for a ~100% system-prompt
/// cache-hit rate between reconciles.
#[derive(Clone)]
pub(crate) struct LandscapeContext {
    /// The living landscape data — always up to date.
    data: LandscapeData,

    // ── stable layer → system_message() ──
    /// Frozen landscape snapshot rendered at construction and after each
    /// reconcile.  Served via [`system_message`] so it sits in the trusted
    /// system prompt and is cached by the model provider.
    stable_landscape: String,

    // ── delta layer → transcript() ──
    /// File paths touched during the current turn (cleared each turn).
    dirty: HashSet<PathBuf>,
    /// Per-turn delta blocks.  `None` when the turn produced no delta.
    /// Index `i` corresponds to turn `i`.
    turn_blocks: Vec<Option<String>>,
    /// Running token count of all unreconciled delta blocks.
    accumulated_tokens: usize,
    /// When `accumulated_tokens` exceeds this threshold the stable landscape
    /// is rebuilt and all deltas are cleared.  Derived from config; 0
    /// disables deferral (reconcile every turn, same as pre-deferral
    /// behaviour).
    reconcile_threshold: usize,

    /// Token budget for landscape rendering.
    max_tokens: usize,
    /// Turn-decay window: files untouched for more turns than this are pruned.
    /// When `0`, pruning is disabled.
    decay_turns: usize,
    /// Whether landscape was ever enabled for this stage.
    /// Set from `data.is_empty()` at construction — when false, all
    /// observations are no-ops and transcripts are always empty.  Remains
    /// false even after every file entry is decay-pruned, so re-touches can
    /// re-explore.
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

        // Reconcile threshold: explicit config value, or default to 30% of
        // max_tokens.  When 0, reconcile every turn (same behaviour as
        // before deferred reconciliation existed).
        let reconcile_threshold = match crate::config::landscape_reconcile_threshold_tokens() {
            Some(0) => 0,
            Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
            None => max_tokens.saturating_mul(3) / 10,
        };

        // Render the initial stable landscape from the seeded data.
        let stable_landscape = if landscape_enabled {
            crate::landscape::format_landscape_data(&data, max_tokens)
        } else {
            String::new()
        };

        let ctx = Self {
            data,
            stable_landscape,
            dirty: HashSet::new(),
            turn_blocks: vec![None], // turn 0: landscape in system prompt, no delta
            accumulated_tokens: 0,
            reconcile_threshold,
            max_tokens,
            decay_turns,
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
                    self.data.touch_dir(&path, &explorer, turn);
                    // Mark all files currently in this directory as dirty
                    // so the delta captures the full updated listing.
                    for file_path in self.data.file_entries.keys() {
                        if file_path.parent() == Some(path.as_path()) {
                            self.dirty.insert(file_path.clone());
                        }
                    }
                } else {
                    self.data.touch_file(&path, &explorer, turn);
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

    /// Render a landscape delta block from the current turn's dirty paths.
    ///
    /// Returns the formatted delta text, or an empty string when no dirty
    /// paths exist or none have entries in `file_entries`.
    fn render_delta(&self) -> String {
        if self.dirty.is_empty() {
            return String::new();
        }

        // Build a throwaway LandscapeData containing only the dirty entries.
        // Signal sections never appear in deltas.
        let mut delta_data = crate::landscape::LandscapeData {
            root: self.data.root.clone(),
            outlines: Vec::new(),
            signal_sections: Vec::new(),
            symbol_index: std::collections::HashMap::new(),
            file_entries: std::collections::HashMap::new(),
        };

        for path in &self.dirty {
            if let Some(entry) = self.data.file_entries.get(path) {
                delta_data.file_entries.insert(path.clone(), entry.clone());
            }
        }

        if delta_data.file_entries.is_empty() {
            return String::new();
        }

        let rendered = crate::landscape::format_landscape_data(&delta_data, self.max_tokens);
        if rendered.is_empty() {
            return String::new();
        }

        format!(
            "<!-- Landscape update — apply to the directory structure in the system prompt -->\n\n{rendered}"
        )
    }

    /// Rebuild the stable landscape from the full current `data` and clear
    /// all pending deltas.  Called when accumulated delta tokens cross the
    /// reconcile threshold.
    fn reconcile(&mut self) {
        self.stable_landscape = if self.landscape_enabled {
            crate::landscape::format_landscape_data(&self.data, self.max_tokens)
        } else {
            String::new()
        };
        self.turn_blocks.fill(None);
        self.accumulated_tokens = 0;

        send_log(
            &self.log_tx,
            log::Level::Debug,
            "landscape context: reconciled — stable landscape rebuilt, deltas cleared",
        );
    }

    /// Compute an approximate token count for a string using the o200k_base
    /// tokenizer.
    fn approx_tokens(text: &str) -> usize {
        crate::landscape::count_tokens(text).unwrap_or(text.len())
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

    fn finalize_turn(&mut self, turn: usize) {
        // Prune decayed directories. When decay_turns is 0 the guard
        // disables pruning entirely so every entry persists regardless
        // of the time since its last touch.
        if self.decay_turns > 0 {
            self.data.prune_decayed(turn, self.decay_turns);
        }

        // Ensure turn_blocks has capacity for this turn index.
        while self.turn_blocks.len() <= turn {
            self.turn_blocks.push(None);
        }

        // Turn 0: landscape lives in the system prompt; no delta needed.
        // Subsequent turns: render a delta from dirty paths if any.
        let delta = if turn == 0 {
            String::new()
        } else {
            self.render_delta()
        };

        if delta.is_empty() {
            self.turn_blocks[turn] = None;
        } else {
            let tok = Self::approx_tokens(&delta);
            self.turn_blocks[turn] = Some(delta);
            self.accumulated_tokens += tok;

            send_log(
                &self.log_tx,
                log::Level::Debug,
                format!(
                    "landscape context: turn {turn} delta {tok} tokens, accumulated {} / {} threshold",
                    self.accumulated_tokens, self.reconcile_threshold,
                ),
            );
        }

        // Check reconcile threshold.  When 0, always reconcile (pre-deferral
        // behaviour — every turn updates the system prompt).
        if self.reconcile_threshold == 0 || self.accumulated_tokens >= self.reconcile_threshold {
            self.reconcile();
        }

        // Clear per-turn dirty set for the next turn.
        self.dirty.clear();
    }

    fn transcript(&self) -> Vec<Option<String>> {
        self.turn_blocks.clone()
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
            // Merge file entries: re-stamp child entries with the parent's
            // current turn so they share the parent's decay clock rather than
            // the child's independent turn space.
            let current_turn = self.turn_blocks.len().saturating_sub(1);
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

            // Merge child delta blocks at the parent's current turn index
            // rather than interleaving by the child's (unrelated) indices.
            if self.turn_blocks.is_empty() {
                self.turn_blocks.push(None);
            }
            let parent_idx = self.turn_blocks.len() - 1;
            // … rest of join logic continues below
            for theirs in &other_ctx.turn_blocks {
                if theirs.is_some() && self.turn_blocks[parent_idx].is_none() {
                    self.turn_blocks[parent_idx] = theirs.clone();
                }
            }

            // If the child has already reconciled (or has more recent data),
            // rebuild our stable landscape to match.  This avoids serving a
            // stale system message after a join.  Clear pending deltas too
            // so the model doesn't see the same updates twice (once folded
            // into the stable system prompt, once as a pending delta).
            if !other_ctx.stable_landscape.is_empty() {
                self.reconcile();
            }

            self.log_stats();
        }
    }

    fn clear_transcript(&mut self) {
        self.turn_blocks.clear();
    }
}
