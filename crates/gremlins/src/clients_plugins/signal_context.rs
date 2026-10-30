use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::landscape::signals::{self, SignalSection, SymbolEntry};

/// Signal sections rendered only into the prompt tail, with a living symbol
/// map that re-scans on Write/Edit.
///
/// Static sections (changed-files, diff-stat) are pre-rendered at construction
/// and never change.  The symbol map is re-rendered on each turn from a live
/// symbol index that is updated when files are written or edited.
#[derive(Clone)]
pub(crate) struct SignalMapContext {
    /// Pre-rendered static sections (changed-files, diff-stat) joined with
    /// double newlines.  Empty when there are no static sections.
    static_sections_text: String,
    /// Stage prompt text used to derive symbol-map identifier candidates.
    /// `None` disables the symbol map entirely.
    prompt: Option<String>,
    /// Worktree root for path canonicalization and display.
    cwd: PathBuf,
    /// Live symbol index: identifier name → first `file:line` occurrence.
    /// Mutated on Write/Edit by re-scanning the touched file.
    symbol_index: HashMap<String, SymbolEntry>,
    /// Per-turn transcript blocks.  Only the most recent turn carries
    /// `Some(rendered)`; all prior turns are `None`.
    turn_transcripts: Vec<Option<String>>,
}

impl SignalMapContext {
    pub(crate) fn new(
        signal_sections: Vec<SignalSection>,
        prompt: Option<String>,
        cwd: PathBuf,
        symbol_index: HashMap<String, SymbolEntry>,
    ) -> Self {
        let static_sections_text = signal_sections
            .iter()
            .filter(|s| s.key != "symbol-map")
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        Self {
            static_sections_text,
            prompt,
            cwd,
            symbol_index,
            turn_transcripts: vec![None],
        }
    }

    /// Render the symbol map from the current live index and the stage prompt.
    fn render_symbol_map(&self) -> String {
        let ctx = signals::SignalContext {
            cwd: self.cwd.clone(),
            pruned_files: None,
            profile: signals::SignalProfile::Full,
            prompt: self.prompt.clone(),
            diff_stat: false,
        };
        signals::build_symbol_map(&ctx, &self.symbol_index)
    }

    /// Build the full rendered text: static sections + live symbol map.
    fn render(&self) -> String {
        let symbol_map = self.render_symbol_map();
        match (self.static_sections_text.is_empty(), symbol_map.is_empty()) {
            (true, true) => String::new(),
            (true, false) => symbol_map,
            (false, true) => self.static_sections_text.clone(),
            (false, false) => format!("{}\n\n{}", self.static_sections_text, symbol_map),
        }
    }

    /// Resolve a tool-reported path against the worktree root.
    fn resolve_path(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// Re-scan a single file after a Write or Edit: drop all symbol-index
    /// entries for this file, then re-extract identifiers from its current
    /// content.  First-occurrence-wins semantics are preserved because
    /// removed entries are re-inserted before any other file could claim
    /// them.
    fn re_scan_file(&mut self, path: &Path) {
        // Remove old entries for this file.
        self.symbol_index.retain(|_, entry| entry.file != path);

        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for (idx, line) in content.lines().enumerate() {
            let line_number = idx + 1;
            for ident in signals::heuristic::extract_identifiers(line, line_number, path) {
                self.symbol_index
                    .entry(ident.name.clone())
                    .or_insert(SymbolEntry {
                        name: ident.name,
                        file: ident.source_file.clone(),
                        line: ident.line,
                    });
            }
        }
    }

    /// Make `turn` the sole index that carries signal content.
    fn set_tail_turn(&mut self, turn: usize) {
        while self.turn_transcripts.len() <= turn {
            self.turn_transcripts.push(None);
        }
        self.turn_transcripts.fill(None);
        let rendered = self.render();
        if !rendered.is_empty() {
            self.turn_transcripts[turn] = Some(rendered);
        }
    }
}

impl crate::clients::model_context::MutableModelContext for SignalMapContext {
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

    fn observe_tool(&mut self, _turn: usize, tool: &str, args: &str, _output: &str) {
        match tool {
            "Write" | "Edit" => {
                let args_val: serde_json::Value =
                    serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
                if let Some(file_path) = args_val
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    self.re_scan_file(&self.resolve_path(file_path));
                }
            }
            _ => {}
        }
    }

    fn finalize_turn(&mut self, turn: usize) {
        self.set_tail_turn(turn);
    }

    fn transcript(&self) -> Vec<Option<String>> {
        vec![None; self.turn_transcripts.len()]
    }

    fn system_message(&self) -> Option<String> {
        let rendered = self.render();
        if rendered.is_empty() {
            None
        } else {
            Some(rendered)
        }
    }

    fn join(&mut self, _other: &dyn crate::clients::model_context::MutableModelContext) {
        // No-op: signal sections come from the parent stage.
    }

    fn clear_transcript(&mut self) {
        self.turn_transcripts.clear();
        self.turn_transcripts.push(None);
    }
}
