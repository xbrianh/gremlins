use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::landscape::{
    format_landscape_block_tool, format_outline_items, LandscapeData, LandscapeExplorer,
};

/// Number of past turns whose `[seen]` paths are remembered so the same path is
/// never landscaped again for at least this many turns after its first appearance.
const SEEN_WINDOW_TURNS: usize = 11;

/// Mutable model context that carries a frozen seed landscape of the worktree.
///
/// The landscape is rendered into the system prompt via [`system_message`] once
/// at construction and never updated — it is a frozen seed that may become stale.
/// Cross-turn dedup is performed so a path that received a `[seen]` block is
/// skipped for the next `SEEN_WINDOW_TURNS` turns.  Outlines are always
/// recomputed from disk — no content caching.
pub(crate) struct LandscapeContext {
    /// Worktree root.
    root: PathBuf,

    /// Frozen seed landscape captured at construction — never changes.
    /// Served via [`system_message`].
    seed_landscape: String,

    /// Whether landscape was ever enabled for this stage.
    /// When false, all observations are no-ops and transcripts are always empty.
    landscape_enabled: bool,

    /// Accumulating paths for the current turn plus ring-buffer of completed
    /// turns.  `tool_result_extra` checks *all* sets before producing output.
    seen: Mutex<SeenState>,
}

/// Per-turn tracking of landscaped paths.
#[derive(Clone)]
struct SeenState {
    /// Paths already given a `[seen]` block in the current turn.
    current: HashSet<PathBuf>,

    /// Completed turn sets; newest at the back, oldest at the front.
    /// Capacity is `SEEN_WINDOW_TURNS - 1` (the current set counts too).
    past: VecDeque<HashSet<PathBuf>>,
}

impl SeenState {
    fn new() -> Self {
        Self {
            current: HashSet::new(),
            past: VecDeque::with_capacity(SEEN_WINDOW_TURNS - 1),
        }
    }

    fn contains(&self, path: &Path) -> bool {
        self.current.contains(path) || self.past.iter().any(|s| s.contains(path))
    }

    fn insert(&mut self, path: PathBuf) {
        self.current.insert(path);
    }

    /// Save the current turn's set into the ring and start a fresh current set.
    fn rotate(&mut self) {
        let prev = std::mem::take(&mut self.current);
        if !prev.is_empty() {
            self.past.push_back(prev);
            // Keep at most SEEN_WINDOW_TURNS - 1 past sets (plus current = N).
            while self.past.len() >= SEEN_WINDOW_TURNS {
                self.past.pop_front();
            }
        }
    }

    fn clear(&mut self) {
        self.current.clear();
        self.past.clear();
    }
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
            seen: Mutex::new(SeenState::new()),
        }
    }

    /// No-op: dedup is handled entirely in `tool_result_extra` using
    /// interior mutability so the read-only signature works.
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

/// Manual Clone: Mutex<SeenState> is not Clone, so we start fresh.
impl Clone for LandscapeContext {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            seed_landscape: self.seed_landscape.clone(),
            landscape_enabled: self.landscape_enabled,
            seen: Mutex::new(SeenState::new()),
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

        // Skip if this path was landscaped in any of the last SEEN_WINDOW_TURNS
        // turns (including the current one).
        {
            let seen = self.seen.lock().unwrap();
            if seen.contains(&path) {
                return None;
            }
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
                                line.push_str(&crate::landscape::format_outline_items(&items));
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
            let dir_label = path.to_string_lossy().into_owned();
            let listing = lines.join("\n  ");
            self.seen.lock().unwrap().insert(path);
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
        self.seen.lock().unwrap().insert(path);
        Some(format!("[seen]\n{name} {items_str}"))
    }

    fn finalize_turn(&mut self, _turn: usize) {
        self.seen.lock().unwrap().rotate();
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

    fn clear_transcript(&mut self) {
        self.seen.lock().unwrap().clear();
    }
}
