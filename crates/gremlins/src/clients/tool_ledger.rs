use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use super::format::truncate_ledger_value;
use super::protocol::{FileObservation, FileObservations};
use crate::landscape::{LandscapeExplorer, OutlineItem};

/// Deduplication tracker for tool observations across turns.
///
/// The agent loop records every Read/Grep/Write/Edit/Landscape observation so
/// the next turn's compact `[seen]` index can avoid re-reading unchanged
/// content and can invalidate stale observations after writes.
/// Directory landscapes are cached per parent-directory and rendered
/// interleaved with file/grep entries.
#[derive(Clone)]
struct LedgerReadEntry {
    last_age: usize,
    count: usize,
    content_hash: u64,
    /// First-access sequence for display ordering in the compact `[seen]` index.
    seq: usize,
    /// Last-access sequence for nested-loop snapshot ordering. Re-reads of
    /// identical content bump this without disturbing the display order above.
    last_seq: usize,
    /// Exact tool output, stored for nested-loop observations.
    output: String,
}

#[derive(Clone)]
struct LedgerGrepEntry {
    last_age: usize,
    count: usize,
    content_hash: u64,
    /// First-access sequence for display ordering in the compact `[seen]` index.
    seq: usize,
    /// Last-access sequence for nested-loop snapshot ordering.
    last_seq: usize,
    /// Exact tool output, stored for nested-loop observations.
    output: String,
}

#[derive(Clone)]
struct LedgerWriteEntry {
    age: usize,
    seq: usize,
}

/// Per-directory landscape cache entry.
///
/// Created eagerly by [`ToolLedger::seed`] with full outline items, or
/// lazily by side-effect listing (Read/Grep/Edit/Glob touching a file in a
/// directory not yet cached) with file names only and `outline_items: None`.
/// An explicit `Landscape` tool call refreshes the entry with full items.
#[derive(Clone)]
struct DirectoryLandscapeEntry {
    age: usize,
    /// First-access sequence for display ordering.
    seq: usize,
    last_seq: usize,
    /// Sorted file names in this directory (basename only).
    sorted_files: Vec<String>,
    /// Full outline items when available (from seed or Landscape tool),
    /// `None` for lazy side-effect listings.
    outline_items: Option<Vec<(String, Vec<OutlineItem>)>>,
    /// Optional prefix content rendered above the directory (signal sections).
    prefix_content: Option<String>,
}

pub(crate) struct ToolLedger {
    /// Key: (file_path, offset, limit).  Offset and limit are zero when
    /// the model reads the whole file.
    reads: std::collections::HashMap<(String, u64, u64), LedgerReadEntry>,
    /// Key: (pattern, path).  Path is empty string when not specified.
    greps: std::collections::HashMap<(String, String), LedgerGrepEntry>,
    /// Key: file_path.
    writes: std::collections::HashMap<String, LedgerWriteEntry>,
    /// Per-directory landscape cache, keyed by parent-directory path.
    directory_landscapes: std::collections::HashMap<PathBuf, DirectoryLandscapeEntry>,
    /// Monotonic counter shared by display order and snapshot order.
    seq: usize,
    /// File paths mutated by Write or Edit, for parent-ledger invalidation.
    mutations: Vec<String>,
    /// Worker observations that could not be fully injected into this loop's
    /// message history, but must still propagate up when this loop itself is a
    /// nested Scout. Kept separate from the seen maps so a later Scout is not
    /// deduplicated against content the parent never fully received.
    omitted: Vec<(usize, FileObservation)>,
    /// Saved root-directory prefix content (signal sections) preserved across
    /// evictions so a write/edit to the root does not permanently lose the
    /// seed signal sections. The `PathBuf` is the root directory that was
    /// evicted.
    saved_root_prefix: Option<(PathBuf, String)>,
    /// Maximum number of files to list per directory in the rendered output.
    max_files_per_dir: usize,
    /// Maximum token budget for landscape rendering (from config).
    max_landscape_tokens: usize,
}

impl ToolLedger {
    pub(crate) fn new() -> Self {
        Self {
            reads: std::collections::HashMap::new(),
            greps: std::collections::HashMap::new(),
            writes: std::collections::HashMap::new(),
            directory_landscapes: std::collections::HashMap::new(),
            seq: 0,
            mutations: Vec::new(),
            omitted: Vec::new(),
            saved_root_prefix: None,
            max_files_per_dir: 40,
            max_landscape_tokens: usize::try_from(crate::config::max_landscape_tokens())
                .unwrap_or(usize::MAX),
        }
    }

    /// Seed the directory landscape cache from a stage-start landscape seed.
    ///
    /// Groups `outlines` by parent directory, populates each with file names
    /// and outline items. Signal sections are rendered as prefix content on
    /// the root directory's entry.
    ///
    /// When `outlines` is empty and no signal sections exist (landscape-disabled
    /// stage), this is a no-op.
    ///
    /// Called by [`super::agent_loop::LoopState::new`] from the stage-start
    /// seed in [`super::backend::RunParams::landscape_seed`] before the first
    /// turn.
    pub(crate) fn seed(&mut self, seed: &crate::landscape::LandscapeSeed) {
        let root = &seed.root;
        let outlines = &seed.outlines;
        let signal_sections = &seed.signal_sections;
        if outlines.is_empty() && signal_sections.is_empty() {
            return;
        }

        let prefix = if signal_sections.is_empty() {
            None
        } else {
            Some(
                signal_sections
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )
        };

        // Group outlines by parent directory.
        let mut dir_files: std::collections::HashMap<PathBuf, Vec<(String, Vec<OutlineItem>)>> =
            std::collections::HashMap::new();
        for outline in outlines {
            if let Some(parent) = outline.path.parent() {
                let parent = parent.to_path_buf();
                let name = outline
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                dir_files
                    .entry(parent)
                    .or_default()
                    .push((name, outline.items.clone()));
            }
        }

        // Create entries for directories that have files.
        for (dir, mut files) in dir_files {
            files.sort_by(|a, b| a.0.cmp(&b.0));
            let sorted_files: Vec<String> = files.iter().map(|f| f.0.clone()).collect();
            self.seq += 1;
            let seq = self.seq;
            // Root dir gets prefix content.
            let prefix_content = if dir.as_path() == root {
                prefix.clone()
            } else {
                None
            };
            self.directory_landscapes.insert(
                dir.clone(),
                DirectoryLandscapeEntry {
                    age: 0,
                    seq,
                    last_seq: seq,
                    sorted_files,
                    outline_items: Some(files),
                    prefix_content,
                },
            );
        }

        // If signal sections exist but no outlines (or root dir not covered),
        // ensure root has a prefix-only entry.
        if prefix.is_some() && !self.directory_landscapes.contains_key(root) {
            self.seq += 1;
            let seq = self.seq;
            self.directory_landscapes.insert(
                root.to_path_buf(),
                DirectoryLandscapeEntry {
                    age: 0,
                    seq,
                    last_seq: seq,
                    sorted_files: Vec::new(),
                    outline_items: Some(Vec::new()),
                    prefix_content: prefix,
                },
            );
        }
    }

    /// Hash a string with DefaultHasher (fast, non-cryptographic).
    fn hash(s: &str) -> u64 {
        let mut h = DefaultHasher::new();
        s.hash(&mut h);
        h.finish()
    }

    /// Parse a u64 from a JSON value, defaulting to 0.
    fn parse_u64(args: &serde_json::Value, key: &str) -> u64 {
        args.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
    }

    fn parse_str<'a>(args: &'a serde_json::Value, key: &str) -> &'a str {
        args.get(key).and_then(|v| v.as_str()).unwrap_or("")
    }

    /// Map an outline-item kind string back to the crate's `&'static str`
    /// constant. [`OutlineItem::kind`] is `&'static str`. Kinds that match a
    /// known constant return that constant; unrecognized kinds are interned in
    /// a global set and returned as `&'static str`.
    fn static_kind(kind: &str) -> &'static str {
        use crate::landscape::{CLASS, ENUM, FN, HEADING, IMPL, KEY, METHOD, MOD, STRUCT, TRAIT};
        match kind {
            "fn" => FN,
            "struct" => STRUCT,
            "impl" => IMPL,
            "trait" => TRAIT,
            "enum" => ENUM,
            "mod" => MOD,
            "class" => CLASS,
            "method" => METHOD,
            "heading" => HEADING,
            "key" => KEY,
            other => {
                use std::collections::HashSet;
                use std::sync::Mutex;
                use std::sync::OnceLock;
                static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
                let set = INTERNED.get_or_init(|| Mutex::new(HashSet::new()));
                let mut guard = set.lock().unwrap();
                if let Some(&existing) = guard.get(other) {
                    existing
                } else {
                    let leaked: &'static str = Box::leak(other.to_string().into_boxed_str());
                    guard.insert(leaked);
                    leaked
                }
            }
        }
    }

    /// Build a depth-0 directory listing (file names only, no outline items)
    /// for `dir`. Uses [`LandscapeExplorer::build`] with depth 0 to collect
    /// file paths, then extracts basenames. Runs synchronously on the caller's
    /// thread.
    fn build_directory_listing(dir: &Path) -> Vec<String> {
        match LandscapeExplorer.build(dir, 0) {
            Ok(outlines) => {
                let mut names: Vec<String> = outlines
                    .iter()
                    .map(|o| {
                        o.path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default()
                    })
                    .collect();
                names.sort();
                names
            }
            Err(_) => Vec::new(),
        }
    }

    /// Ensure a directory is present in the cache. If absent, populate with a
    /// depth-0 file-name-only listing (outline_items: None). Returns the seq
    /// assigned to a newly inserted entry, or None if already present.
    fn ensure_directory(&mut self, dir: &Path) -> Option<usize> {
        if self.directory_landscapes.contains_key(dir) {
            return None;
        }
        let sorted_files = Self::build_directory_listing(dir);
        self.seq += 1;
        let seq = self.seq;
        // Re-attach saved root prefix if this directory matches the one
        // whose prefix was saved during eviction.
        let prefix_content = self
            .saved_root_prefix
            .as_ref()
            .and_then(|(root_path, prefix)| {
                if dir == root_path.as_path() {
                    Some(prefix.clone())
                } else {
                    None
                }
            });
        self.directory_landscapes.insert(
            dir.to_path_buf(),
            DirectoryLandscapeEntry {
                age: 0,
                seq,
                last_seq: seq,
                sorted_files,
                outline_items: None,
                prefix_content,
            },
        );
        Some(seq)
    }

    /// Record a tool execution.  Write/Edit do NOT invalidate here;
    /// caller must call `invalidate_after_write` after all observations
    /// for the turn are recorded (see Comment 1 — Phase 2 concurrency).
    pub(crate) fn record(
        &mut self,
        name: &str,
        _key: &str,
        output: &str,
        args_json: &str,
        turn_num: usize,
    ) {
        let args: serde_json::Value =
            serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);

        match name {
            "Read" => {
                let file_path = Self::parse_str(&args, "file_path").to_string();
                let offset = Self::parse_u64(&args, "offset");
                let limit = Self::parse_u64(&args, "limit");
                let content_hash = Self::hash(output);
                let age = turn_num;
                self.seq += 1;
                let seq = self.seq;

                let entry_key = (file_path, offset, limit);
                if let Some(existing) = self.reads.get_mut(&entry_key) {
                    if existing.content_hash == content_hash {
                        existing.count += 1;
                        existing.last_age = age;
                        existing.last_seq = seq;
                    } else {
                        existing.count = 1;
                        existing.last_age = age;
                        existing.content_hash = content_hash;
                        existing.seq = seq;
                        existing.last_seq = seq;
                        existing.output = output.to_string();
                    }
                } else {
                    self.reads.insert(
                        entry_key,
                        LedgerReadEntry {
                            last_age: age,
                            count: 1,
                            content_hash,
                            seq,
                            last_seq: seq,
                            output: output.to_string(),
                        },
                    );
                }
                // Populate parent directory if not yet cached.
                let path = Path::new(Self::parse_str(&args, "file_path"));
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        self.ensure_directory(parent);
                    }
                }
            }
            "Grep" => {
                let pattern = Self::parse_str(&args, "pattern").to_string();
                let path = Self::parse_str(&args, "path").to_string();
                let content_hash = Self::hash(output);
                let age = turn_num;
                self.seq += 1;
                let seq = self.seq;

                let entry_key = (pattern, path);
                if let Some(existing) = self.greps.get_mut(&entry_key) {
                    if existing.content_hash == content_hash {
                        existing.count += 1;
                        existing.last_age = age;
                        existing.last_seq = seq;
                    } else {
                        existing.count = 1;
                        existing.last_age = age;
                        existing.content_hash = content_hash;
                        existing.seq = seq;
                        existing.last_seq = seq;
                        existing.output = output.to_string();
                    }
                } else {
                    self.greps.insert(
                        entry_key,
                        LedgerGrepEntry {
                            last_age: age,
                            count: 1,
                            content_hash,
                            seq,
                            last_seq: seq,
                            output: output.to_string(),
                        },
                    );
                }
                // Grep path is a search directory — populate it too.
                let search_path = Path::new(Self::parse_str(&args, "path"));
                if !search_path.as_os_str().is_empty() && search_path.is_dir() {
                    self.ensure_directory(search_path);
                }
            }
            "Write" => {
                let file_path = Self::parse_str(&args, "file_path").to_string();
                let age = turn_num;
                self.seq += 1;
                let seq = self.seq;
                self.writes
                    .insert(file_path.clone(), LedgerWriteEntry { age, seq });
                self.mutations.push(file_path);
            }
            "Edit" => {
                let file_path = Self::parse_str(&args, "file_path").to_string();
                if !file_path.is_empty() {
                    self.mutations.push(file_path);
                }
            }
            "Landscape" => {
                let file_path = Self::parse_str(&args, "file_path").to_string();
                let age = turn_num;
                self.seq += 1;
                let seq = self.seq;
                let dir = PathBuf::from(&file_path);
                // Rebuild the directory listing with full outline items from the
                // Landscape tool output. The output is the landscape block text,
                // but we extract file-level outlines by re-outlining the
                // directory directly via LandscapeExplorer.
                let mut sorted_files: Vec<String> = Vec::new();
                let mut outline_items: Vec<(String, Vec<OutlineItem>)> = Vec::new();
                if let Ok(build) = LandscapeExplorer.build_with_failures(&dir, 0) {
                    for outline in &build.outlines {
                        let name = outline
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        sorted_files.push(name.clone());
                        outline_items.push((name, outline.items.clone()));
                    }
                    sorted_files.sort();
                    outline_items.sort_by(|a, b| a.0.cmp(&b.0));
                }
                self.directory_landscapes.insert(
                    dir,
                    DirectoryLandscapeEntry {
                        age,
                        seq,
                        last_seq: seq,
                        sorted_files,
                        outline_items: Some(outline_items),
                        prefix_content: None,
                    },
                );
            }
            "Glob" => {
                // Glob discovers files — similar to Grep, populate the
                // search directory if a path was specified.
                let path = Self::parse_str(&args, "path");
                if !path.is_empty() {
                    let dir = Path::new(path);
                    let target = if dir.is_dir() {
                        dir
                    } else {
                        dir.parent().unwrap_or(Path::new("."))
                    };
                    if !target.as_os_str().is_empty() {
                        self.ensure_directory(target);
                    }
                }
            }
            _ => {}
        }
    }

    /// Apply write/edit invalidation for a file path.  Nukes Read entries
    /// for that file, all Greps, and removes the written file's parent
    /// directory from the landscape cache so the next interaction rebuilds
    /// a fresh listing.  Preserves the root directory's `prefix_content`
    /// (signal sections) so they survive any write/edit to a root file.
    pub(crate) fn invalidate_after_write(&mut self, file_path: &str) {
        self.reads.retain(|(fp, _, _), _| fp != file_path);
        self.greps.clear();
        // Evict the written file's parent directory.
        let path = Path::new(file_path);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                // Preserve prefix_content before eviction.
                if let Some(entry) = self.directory_landscapes.get(parent) {
                    if let Some(ref prefix) = entry.prefix_content {
                        if self.saved_root_prefix.is_none() {
                            self.saved_root_prefix = Some((parent.to_path_buf(), prefix.clone()));
                        }
                    }
                }
                self.directory_landscapes.remove(parent);
            }
        }
    }

    /// Format the accumulated ledger as a compact index, including directory
    /// landscape entries interleaved with file and grep entries by first-access
    /// sequence.
    pub(crate) fn format(&self, turn_num: usize) -> String {
        let age_of = |stored: usize| -> usize { turn_num.saturating_sub(stored) };

        let mut items: Vec<(usize, String)> = Vec::new();

        // Group reads and writes by file path with first-access seq.
        let mut file_seqs: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut file_entries: std::collections::HashMap<String, Vec<(&LedgerReadEntry, u64, u64)>> =
            std::collections::HashMap::new();

        for ((fp, offset, limit), entry) in &self.reads {
            file_seqs
                .entry(fp.clone())
                .and_modify(|s| *s = (*s).min(entry.seq))
                .or_insert(entry.seq);
            file_entries
                .entry(fp.clone())
                .or_default()
                .push((entry, *offset, *limit));
        }
        for (fp, entry) in &self.writes {
            file_seqs
                .entry(fp.clone())
                .and_modify(|s| *s = (*s).min(entry.seq))
                .or_insert(entry.seq);
        }

        let mut keys: Vec<&String> = file_seqs.keys().collect();
        keys.sort_by_key(|fp| file_seqs[*fp]);

        for fp in &keys {
            let has_write = self.writes.contains_key(*fp);
            let write_age = self.writes.get(*fp).map(|e| age_of(e.age));
            let display_fp = truncate_ledger_value(fp);

            let mut full_count: usize = 0;
            let mut full_age: usize = usize::MAX;
            let mut partials: Vec<(u64, u64, usize, usize)> = Vec::new();

            if let Some(entries) = file_entries.get(*fp) {
                let mut sorted: Vec<_> = entries.iter().collect();
                sorted.sort_by_key(|(e, _, _)| e.seq);
                for (entry, offset, limit) in sorted {
                    let age = age_of(entry.last_age);
                    if *offset == 0 && *limit == 0 {
                        full_count += entry.count;
                        full_age = full_age.min(age);
                    } else {
                        partials.push((*offset, *limit, age, entry.count));
                    }
                }
            }

            let mut line = String::new();
            if full_count > 0 {
                line.push_str(&format!("{} R({}t", display_fp, full_age));
                if full_count > 1 {
                    line.push_str(&format!(",{}×", full_count));
                }
                for (offset, limit, age, count) in &partials {
                    line.push_str(&format!(".{}-{}:{}t", offset, limit, age));
                    if *count > 1 {
                        line.push_str(&format!(",{}×", count));
                    }
                }
                line.push(')');
            } else if !partials.is_empty() {
                let min_age = partials.iter().map(|(_, _, a, _)| *a).min().unwrap();
                line.push_str(&format!("{} R({}t", display_fp, min_age));
                for (offset, limit, age, count) in &partials {
                    line.push_str(&format!(".{}-{}:{}t", offset, limit, age));
                    if *count > 1 {
                        line.push_str(&format!(",{}×", count));
                    }
                }
                line.push(')');
            }

            if has_write {
                if line.is_empty() {
                    line.push_str(&display_fp);
                    line.push(' ');
                } else {
                    line.push(' ');
                }
                line.push_str(&format!("W({}t)", write_age.unwrap()));
            }

            if !line.is_empty() {
                items.push((file_seqs[*fp], line));
            }
        }

        // Grep entries.
        for ((pattern, _path), entry) in &self.greps {
            let age = age_of(entry.last_age);
            let display_pattern = truncate_ledger_value(pattern);
            let mut line = format!("\"{}\" G({}t", display_pattern, age);
            if entry.count > 1 {
                line.push_str(&format!(",{}×", entry.count));
            }
            line.push(')');
            items.push((entry.seq, line));
        }

        // Directory landscape entries — interleaved by seq.
        // Precompute read path index and mutations set for O(1) lookups.
        let read_paths: std::collections::HashSet<&str> =
            self.reads.keys().map(|(fp, _, _)| fp.as_str()).collect();
        let read_ages: std::collections::HashMap<&str, usize> = self
            .reads
            .iter()
            .map(|((fp, _, _), e)| (fp.as_str(), age_of(e.last_age)))
            .collect();
        let write_ages: std::collections::HashMap<&str, usize> = self
            .writes
            .iter()
            .map(|(fp, e)| (fp.as_str(), age_of(e.age)))
            .collect();
        let mutation_set: std::collections::HashSet<&str> =
            self.mutations.iter().map(|s| s.as_str()).collect();

        // Track remaining token budget for landscape rendering.
        let mut landscape_budget = self.max_landscape_tokens;

        for (dir, entry) in &self.directory_landscapes {
            let display_dir = truncate_ledger_value(&dir.to_string_lossy());

            // Add prefix content if present and within budget.
            if let Some(ref prefix) = entry.prefix_content {
                let tok = crate::landscape::count_tokens(prefix).unwrap_or(prefix.len());
                if tok <= landscape_budget {
                    landscape_budget = landscape_budget.saturating_sub(tok);
                    items.push((entry.seq, prefix.clone()));
                }
            }

            // Build item-lookup map for this directory.
            let item_map: std::collections::HashMap<&str, &[OutlineItem]> = entry
                .outline_items
                .as_ref()
                .map_or_else(std::collections::HashMap::new, |items| {
                    items
                        .iter()
                        .map(|(n, o)| (n.as_str(), o.as_slice()))
                        .collect()
                });

            // Collect file entries under this directory.
            let mut file_lines: Vec<String> = Vec::new();
            let file_cap = self.max_files_per_dir;
            let total_files = entry.sorted_files.len();

            for name in entry.sorted_files.iter().take(file_cap) {
                let full_path_str = {
                    let p = dir.join(name);
                    p.to_string_lossy().to_string()
                };
                let fp_key: &str = &full_path_str;

                // Check annotations: R, G, W, E.
                let has_read = read_paths.contains(fp_key);
                let has_write = write_ages.contains_key(fp_key);
                let has_edit = mutation_set.contains(fp_key) && !has_write;
                let mut flags = String::new();
                let mut file_age: usize = usize::MAX;
                if has_read {
                    flags.push('R');
                    if let Some(a) = read_ages.get(fp_key) {
                        file_age = file_age.min(*a);
                    }
                }
                if has_write {
                    flags.push('W');
                    if let Some(a) = write_ages.get(fp_key) {
                        file_age = file_age.min(*a);
                    }
                }
                if has_edit {
                    flags.push('E');
                }

                // Gather outline items for this file.
                let mut file_line = if flags.is_empty() {
                    name.clone()
                } else {
                    let age_display = if file_age != usize::MAX {
                        file_age
                    } else {
                        age_of(entry.age)
                    };
                    format!("{name} {flags}({age_display}t)")
                };

                if let Some(outline) = item_map.get(name.as_str()) {
                    if !outline.is_empty() {
                        let item_str = crate::landscape::format_outline_items(outline);
                        file_line.push(' ');
                        file_line.push_str(&item_str);
                    }
                }
                file_lines.push(file_line);
            }

            if total_files > file_cap {
                file_lines.push(format!("... and {} more", total_files - file_cap));
            }

            if !file_lines.is_empty() {
                let header = format!("{display_dir}/");
                // Check token budget: count header + all file lines.
                let header_tok = crate::landscape::count_tokens(&header).unwrap_or(header.len());
                let file_tokens: usize = file_lines
                    .iter()
                    .map(|l| crate::landscape::count_tokens(l).unwrap_or(l.len()))
                    .sum();
                let total_tok = header_tok + file_tokens;
                if total_tok <= landscape_budget {
                    landscape_budget = landscape_budget.saturating_sub(total_tok);
                    items.push((entry.seq, header));
                    for fl in &file_lines {
                        items.push((entry.seq, format!("  {fl}")));
                    }
                } else if header_tok <= landscape_budget {
                    // Emit header with truncated file list.
                    landscape_budget = landscape_budget.saturating_sub(header_tok);
                    items.push((entry.seq, header));
                    let mut emitted = 0usize;
                    for fl in &file_lines {
                        let fl_tok = crate::landscape::count_tokens(fl).unwrap_or(fl.len());
                        if fl_tok <= landscape_budget {
                            landscape_budget = landscape_budget.saturating_sub(fl_tok);
                            items.push((entry.seq, format!("  {fl}")));
                            emitted += 1;
                        } else {
                            break;
                        }
                    }
                    if emitted < file_lines.len() {
                        items.push((entry.seq, "  ...".to_string()));
                    }
                }
            }
        }

        items.sort_by_key(|(se, _)| *se);
        let lines: Vec<String> = items.into_iter().map(|(_, l)| l).collect();

        if lines.is_empty() {
            "[seen]\n(none)".to_string()
        } else {
            format!("[seen]\n{}", lines.join("\n"))
        }
    }

    /// Build a serializable snapshot of the worker's file observations
    /// (Read/Grep/DirLs/Landscape) plus the mutation paths, for reporting
    /// up to the parent loop of a Scout task.
    pub(crate) fn file_observations(&self) -> FileObservations {
        let mut items: Vec<(usize, FileObservation)> = Vec::new();

        for ((path, offset, limit), entry) in &self.reads {
            items.push((
                entry.last_seq,
                FileObservation {
                    tool: "Read".to_string(),
                    path: path.clone(),
                    offset: *offset,
                    limit: *limit,
                    pattern: None,
                    output: entry.output.clone(),
                },
            ));
        }
        for ((pattern, path), entry) in &self.greps {
            items.push((
                entry.last_seq,
                FileObservation {
                    tool: "Grep".to_string(),
                    path: path.clone(),
                    offset: 0,
                    limit: 0,
                    pattern: Some(pattern.clone()),
                    output: entry.output.clone(),
                },
            ));
        }
        // Emit directory landscape entries as DirLs observations.
        for (dir, entry) in &self.directory_landscapes {
            let json = serde_json::json!({
                "files": entry.sorted_files,
                "outline_items": entry.outline_items.as_ref().map(|items| {
                    items.iter().map(|(name, outline)| {
                        serde_json::json!({
                            "name": name,
                            "items": outline.iter().map(|oi| {
                                serde_json::json!({
                                    "kind": oi.kind,
                                    "name": oi.name,
                                    "line": oi.line,
                                })
                            }).collect::<Vec<_>>(),
                        })
                    }).collect::<Vec<_>>()
                }),
                "prefix_content": entry.prefix_content,
            });
            items.push((
                entry.last_seq,
                FileObservation {
                    tool: "DirLs".to_string(),
                    path: dir.to_string_lossy().to_string(),
                    offset: 0,
                    limit: 0,
                    pattern: None,
                    output: json.to_string(),
                },
            ));
        }
        for (seq, obs) in &self.omitted {
            items.push((*seq, obs.clone()));
        }

        items.sort_by_key(|a| std::cmp::Reverse(a.0));
        let items = items.into_iter().map(|(_, item)| item).collect();

        let mut written_paths: Vec<String> = Vec::new();
        for p in &self.mutations {
            if !written_paths.iter().any(|w| w == p) {
                written_paths.push(p.clone());
            }
        }

        FileObservations {
            items,
            truncated: false,
            written_paths,
        }
    }

    /// Whether the ledger already records an observation matching `obs`.
    pub(crate) fn has_observation(&self, obs: &FileObservation) -> bool {
        match obs.tool.as_str() {
            "Read" => self
                .reads
                .contains_key(&(obs.path.clone(), obs.offset, obs.limit)),
            "Grep" => {
                let key = (obs.pattern.clone().unwrap_or_default(), obs.path.clone());
                self.greps
                    .get(&key)
                    .is_some_and(|e| e.content_hash == Self::hash(&obs.output))
            }
            "Landscape" => {
                let dir = PathBuf::from(&obs.path);
                match self.directory_landscapes.get(&dir) {
                    // Only deduplicate when we already have outline items (the
                    // richer form). A shallow listing stays open for upgrade.
                    Some(e) if e.outline_items.is_some() => true,
                    _ => false,
                }
            }
            "DirLs" => {
                let dir = PathBuf::from(&obs.path);
                match self.directory_landscapes.get(&dir) {
                    // Same directory → compare content hash for dedup.
                    Some(existing) => {
                        let existing_json = serde_json::json!({
                            "files": existing.sorted_files,
                            "outline_items": existing.outline_items.as_ref().map(|items| {
                                items.iter().map(|(name, outline)| {
                                    serde_json::json!({
                                        "name": name,
                                        "items": outline.iter().map(|oi| {
                                            serde_json::json!({
                                                "kind": oi.kind,
                                                "name": oi.name,
                                                "line": oi.line,
                                            })
                                        }).collect::<Vec<_>>(),
                                    })
                                }).collect::<Vec<_>>()
                            }),
                            "prefix_content": existing.prefix_content,
                        });
                        Self::hash(&existing_json.to_string()) == Self::hash(&obs.output)
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// Record a worker's file observation as if this loop had read it.
    pub(crate) fn absorb_observation(&mut self, obs: &FileObservation, turn_num: usize) {
        self.seq += 1;
        let seq = self.seq;
        let content_hash = Self::hash(&obs.output);
        match obs.tool.as_str() {
            "Read" => {
                let key = (obs.path.clone(), obs.offset, obs.limit);
                self.reads.insert(
                    key,
                    LedgerReadEntry {
                        last_age: turn_num,
                        count: 1,
                        content_hash,
                        seq,
                        last_seq: seq,
                        output: obs.output.clone(),
                    },
                );
            }
            "Grep" => {
                let key = (obs.pattern.clone().unwrap_or_default(), obs.path.clone());
                self.greps.insert(
                    key,
                    LedgerGrepEntry {
                        last_age: turn_num,
                        count: 1,
                        content_hash,
                        seq,
                        last_seq: seq,
                        output: obs.output.clone(),
                    },
                );
            }
            "Landscape" => {
                let dir = PathBuf::from(&obs.path);
                // Only rebuild/replace when we don't already have outline
                // items — the seed + explicit Landscape calls already
                // provide richer data than a nested scout's Landscape.
                let needs_insert = match self.directory_landscapes.get(&dir) {
                    Some(e) => e.outline_items.is_none(),
                    None => true,
                };
                if !needs_insert {
                    // Bump the access seq and age so format() still reflects
                    // that this directory was observed.
                    if let Some(e) = self.directory_landscapes.get_mut(&dir) {
                        e.last_seq = seq;
                        e.age = turn_num;
                    }
                    return;
                }
                // Rebuild via LandscapeExplorer.
                let mut sorted_files: Vec<String> = Vec::new();
                let mut outline_items: Vec<(String, Vec<OutlineItem>)> = Vec::new();
                if let Ok(build) = LandscapeExplorer.build_with_failures(&dir, 0) {
                    for outline in &build.outlines {
                        let name = outline
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        sorted_files.push(name.clone());
                        outline_items.push((name, outline.items.clone()));
                    }
                    sorted_files.sort();
                    outline_items.sort_by(|a, b| a.0.cmp(&b.0));
                }
                // Preserve existing prefix_content.
                let existing_prefix = self
                    .directory_landscapes
                    .get(&dir)
                    .and_then(|e| e.prefix_content.clone());
                self.directory_landscapes.insert(
                    dir,
                    DirectoryLandscapeEntry {
                        age: turn_num,
                        seq,
                        last_seq: seq,
                        sorted_files,
                        outline_items: Some(outline_items),
                        prefix_content: existing_prefix,
                    },
                );
            }
            "DirLs" => {
                // Parse the JSON output to recover the directory listing.
                let dir = PathBuf::from(&obs.path);
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&obs.output) {
                    let sorted_files: Vec<String> = json
                        .get("files")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let outline_items =
                        json.get("outline_items")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|entry| {
                                        let name = entry.get("name")?.as_str()?.to_string();
                                        let items = entry
                                            .get("items")
                                            .and_then(|v| v.as_array())
                                            .map(|arr| {
                                                arr.iter()
                                                    .filter_map(|oi| {
                                                        Some(OutlineItem::new(
                                                            Self::static_kind(
                                                                oi.get("kind")?.as_str()?,
                                                            ),
                                                            oi.get("name")?.as_str()?,
                                                            oi.get("line")?.as_u64()? as usize,
                                                        ))
                                                    })
                                                    .collect()
                                            })
                                            .unwrap_or_default();
                                        Some((name, items))
                                    })
                                    .collect()
                            });
                    let incoming_prefix_content = json
                        .get("prefix_content")
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    // Merge: only insert when the incoming entry is richer
                    // (has outline_items) or the local entry is absent. A
                    // shallow scout entry never overwrites deeper seeded data.
                    let should_insert = match self.directory_landscapes.get(&dir) {
                        None => true,
                        Some(existing) => {
                            // Upgrade when incoming has outline items and we
                            // don't, or when incoming has prefix content we
                            // don't.
                            outline_items.is_some() && existing.outline_items.is_none()
                                || incoming_prefix_content.is_some()
                                    && existing.prefix_content.is_none()
                        }
                    };
                    if should_insert {
                        // When upgrading, preserve existing prefix_content
                        // unless the incoming entry provides one.
                        let prefix_content = incoming_prefix_content.or_else(|| {
                            self.directory_landscapes
                                .get(&dir)
                                .and_then(|e| e.prefix_content.clone())
                        });
                        // When upgrading and the local entry already has
                        // outline items, keep those (they're richer/higher
                        // quality from the seed).
                        let final_outline_items = match self.directory_landscapes.get(&dir) {
                            Some(e) if e.outline_items.is_some() && outline_items.is_none() => {
                                e.outline_items.clone()
                            }
                            _ => outline_items,
                        };
                        self.directory_landscapes.insert(
                            dir,
                            DirectoryLandscapeEntry {
                                age: turn_num,
                                seq,
                                last_seq: seq,
                                sorted_files,
                                outline_items: final_outline_items,
                                prefix_content,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// Remember an observation that was too large to deliver fully.
    pub(crate) fn remember_omitted(&mut self, obs: &FileObservation) {
        self.seq += 1;
        self.omitted.push((self.seq, obs.clone()));
    }
}

/// Fold a Scout Task result's embedded file observations into a readable
/// form, deduplicated against the parent's own [`ToolLedger`].
///
/// Files the worker wrote invalidate the parent's stale ledger entries; files
/// the parent has already read are dropped so they are not re-injected into
/// the message history. A truncation marker is preserved when the worker read
/// more than the output budget permitted.
pub(crate) fn fold_scout_output(output: &str, ledger: &mut ToolLedger, turn_num: usize) -> String {
    let Some((mut obs, prefix, suffix)) = FileObservations::parse_block(output) else {
        return output.to_string();
    };

    // A worker's Write/Edit must invalidate the parent's stale copy of any
    // file it touched, so the parent never serves an outdated read. Record
    // those paths too, so a Scout nested inside a Scout propagates the
    // invalidation all the way up the chain.
    for fp in &obs.written_paths {
        ledger.invalidate_after_write(fp);
        if !ledger.mutations.iter().any(|m| m == fp) {
            ledger.mutations.push(fp.clone());
        }
    }

    // Drop observations the parent has already read this run.
    obs.items.retain(|o| !ledger.has_observation(o));

    if obs.items.is_empty() && !obs.truncated {
        return format!("{prefix}{suffix}");
    }

    let max_bytes = crate::config::max_tool_output_bytes() as usize;
    let budget = if max_bytes == 0 {
        usize::MAX
    } else {
        max_bytes
    };

    // Reserve room for the fixed wrapper (heading + truncation marker) before
    // allocating bytes to rendered observations. Without this, a section that
    // fills the raw budget would push the wrapped block over the configured
    // tool-output cap. The marker is always reserved because truncation may
    // only become known while rendering below.
    let heading = "\n\n## Scout observations\n";
    let marker =
        "> Some observations were omitted: the worker read more than the output budget.\n\n";
    let section_budget = budget.saturating_sub(heading.len() + marker.len());

    let mut truncated = obs.truncated;
    let mut section = String::new();
    let mut used = 0usize;
    let mut delivered: Vec<FileObservation> = Vec::new();
    let mut omitted: Vec<FileObservation> = Vec::new();

    // Items are most-recent-first; keep the newest observations and a prefix
    // of any single oversized observation rather than dropping it entirely.
    for o in &obs.items {
        let rendered = render_observation(o);
        if used.saturating_add(rendered.len()) <= section_budget {
            section.push_str(&rendered);
            used += rendered.len();
            delivered.push(o.clone());
        } else if used < section_budget {
            let remaining = section_budget - used;
            section.push_str(&truncate_to_bytes(&rendered, remaining));
            used = section_budget;
            truncated = true;
            // The parent only receives a prefix, so the full observation must
            // still propagate upward without entering the seen ledger.
            omitted.push(o.clone());
        } else {
            truncated = true;
            omitted.push(o.clone());
        }
    }

    // Only fully delivered observations become "seen" for future dedup.
    // Omitted ones are remembered separately for recursive propagation.
    let mut delivered_oldest_first = delivered;
    delivered_oldest_first.reverse();
    for o in &delivered_oldest_first {
        ledger.absorb_observation(o, turn_num);
    }
    let mut omitted_oldest_first = omitted;
    omitted_oldest_first.reverse();
    for o in &omitted_oldest_first {
        ledger.remember_omitted(o);
    }

    let mut out = prefix.to_string();
    let mut block = String::new();
    block.push_str(heading);
    if truncated {
        block.push_str(marker);
    }
    block.push_str(&section);
    // The fixed wrapper itself can exceed a very small budget; clamp the
    // complete block so the configured cap is still honored.
    out.push_str(&truncate_to_bytes(&block, budget));
    out.push_str(suffix);
    out
}

/// Extract the file paths a Scout Task wrote, for the parent's deferred
/// invalidation pass. A Scout that edits a file is a mutation of the parent's
/// view just like a direct Write/Edit, so these paths must participate in the
/// same end-of-batch invalidation that guards against concurrent tool ordering.
pub(crate) fn scout_written_paths(output: &str) -> Vec<String> {
    FileObservations::parse_block(output)
        .map(|(obs, _, _)| obs.written_paths)
        .unwrap_or_default()
}

/// Render one worker observation as the markdown section injected into the
/// parent's message history.
fn render_observation(o: &FileObservation) -> String {
    match o.tool.as_str() {
        "Read" => format!(
            "### Read: {} (offset {}..{})\n{}\n\n",
            o.path,
            o.offset,
            o.offset.saturating_add(o.limit),
            o.output
        ),
        "Grep" => format!(
            "### Grep: {} in {}\n{}\n\n",
            o.pattern.as_deref().unwrap_or(""),
            o.path,
            o.output
        ),
        "Landscape" if o.path.is_empty() => format!("### Landscape\n{}\n\n", o.output),
        "Landscape" => format!("### Landscape: {}\n{}\n\n", o.path, o.output),
        "DirLs" => {
            // Render the directory listing as a human-readable `dir/` header
            // with indented file lines, mirroring the format() shape.
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&o.output) {
                let files: Vec<&str> = json
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let items: Vec<(&str, Vec<String>)> = json
                    .get("outline_items")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|entry| {
                                let name = entry.get("name")?.as_str()?;
                                let oi = entry
                                    .get("items")
                                    .and_then(|v| v.as_array())
                                    .map(|arr| {
                                        arr.iter()
                                            .map(|oi| {
                                                format!(
                                                    "{}|{}|{}",
                                                    oi.get("line")
                                                        .and_then(|v| v.as_u64())
                                                        .unwrap_or(0),
                                                    oi.get("kind")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("?"),
                                                    oi.get("name")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("?")
                                                )
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                Some((name, oi))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let prefix = json
                    .get("prefix_content")
                    .and_then(|v| v.as_str())
                    .map(|s| format!("{s}\n"))
                    .unwrap_or_default();
                let mut body = String::new();
                for name in &files {
                    let item_str = items
                        .iter()
                        .find(|(n, _)| *n == *name)
                        .map(|(_, o)| o.join(" "))
                        .unwrap_or_default();
                    if item_str.is_empty() {
                        body.push_str(&format!("  {name}\n"));
                    } else {
                        body.push_str(&format!("  {name} {item_str}\n"));
                    }
                }
                format!(
                    "### Landscape: {}\n{prefix}{}/{}\n{body}\n",
                    o.path, o.path, ""
                )
            } else {
                format!("### Landscape: {}\n{}\n\n", o.path, o.output)
            }
        }
        _ => String::new(),
    }
}

/// Return a prefix of `s` that fits within `max_bytes` at a UTF-8 character
/// boundary.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(max_bytes);
    let mut used = 0usize;
    for c in s.chars() {
        let cl = c.len_utf8();
        if used + cl > max_bytes {
            break;
        }
        out.push(c);
        used += cl;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::format::{key_arg, ledger_key_arg};
    use crate::test_support::EnvGuard;

    fn make_read_obs(path: &str, output: &str, offset: u64, limit: u64) -> FileObservation {
        FileObservation {
            tool: "Read".into(),
            path: path.into(),
            offset,
            limit,
            pattern: None,
            output: output.into(),
        }
    }

    // ── ToolLedger unit tests ──────────────────────────────────────

    #[test]
    fn tool_ledger_accumulates_reads() {
        let mut tl = ToolLedger::new();
        let args = serde_json::json!({"file_path": "/tmp/a.rs"}).to_string();
        let key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/a.rs"}));
        tl.record("Read", &key, "content1", &args, 0);
        tl.record("Read", &key, "content1", &args, 0);
        let args2 = serde_json::json!({"file_path": "/tmp/b.rs"}).to_string();
        let key2 = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/b.rs"}));
        tl.record("Read", &key2, "content2", &args2, 0);
        // Two files, each read; turn 0 for all.
        let out = tl.format(0);
        assert!(out.contains("/tmp/a.rs"), "{out}");
        assert!(out.contains("/tmp/b.rs"), "{out}");
        assert!(out.contains(",2×"), "count should be 2 for a.rs: {out}");
    }

    #[test]
    fn tool_ledger_dedup_by_content_hash() {
        let mut tl = ToolLedger::new();
        let args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        let key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x.rs"}));
        tl.record("Read", &key, "same", &args, 0);
        tl.record("Read", &key, "same", &args, 0);
        tl.record("Read", &key, "same", &args, 0);
        let out = tl.format(0);
        assert!(
            out.contains(",3×"),
            "same-hash re-reads should bump count: {out}"
        );

        // Different content replaces.
        let mut tl2 = ToolLedger::new();
        tl2.record("Read", &key, "first", &args, 0);
        tl2.record("Read", &key, "second", &args, 0);
        let out2 = tl2.format(0);
        assert!(!out2.contains("×"), "different hash still counts: {out2}");
    }

    #[test]
    fn tool_ledger_write_invalidates_read() {
        let mut tl = ToolLedger::new();
        let args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        let key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x.rs"}));
        tl.record("Read", &key, "content", &args, 0);
        tl.record("Write", &key, "written", &args, 0);
        // Invalidation is deferred — caller must apply after the batch.
        tl.invalidate_after_write("/tmp/x.rs");
        let out = tl.format(0);
        assert!(out.contains("W("), "should have write: {out}");
        assert!(!out.contains("R("), "read should be invalidated: {out}");
    }

    #[test]
    fn tool_ledger_write_invalidates_greps() {
        let mut tl = ToolLedger::new();
        let grep_args = serde_json::json!({"pattern": "fn main", "path": "/tmp"}).to_string();
        let grep_key = ledger_key_arg(&serde_json::json!({"pattern": "fn main"}));
        tl.record("Grep", &grep_key, "results", &grep_args, 0);
        let write_args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        let write_key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x.rs"}));
        tl.record("Write", &write_key, "written", &write_args, 0);
        tl.invalidate_after_write("/tmp/x.rs");
        let out = tl.format(0);
        assert!(!out.contains("G("), "greps should be invalidated: {out}");
        assert!(
            !out.contains("fn main"),
            "grep pattern should be gone: {out}"
        );
    }

    #[test]
    fn tool_ledger_write_invalidates_landscape() {
        let mut tl = ToolLedger::new();
        let ls_args = serde_json::json!({"file_path": "/tmp"}).to_string();
        let ls_key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp"}));
        tl.record("Landscape", &ls_key, "tree", &ls_args, 0);
        let write_args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        let write_key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x.rs"}));
        tl.record("Write", &write_key, "written", &write_args, 0);
        tl.invalidate_after_write("/tmp/x.rs");
        let out = tl.format(0);
        assert!(
            !out.contains("L("),
            "landscape should be invalidated: {out}"
        );
        // The cache no longer holds the invalidated parent directory.
        assert!(
            !tl.directory_landscapes.contains_key(Path::new("/tmp")),
            "parent directory must be evicted from the landscape cache"
        );
    }

    #[test]
    fn tool_ledger_partial_reads() {
        let mut tl = ToolLedger::new();
        let args1 =
            serde_json::json!({"file_path": "/tmp/x.rs", "offset": 0, "limit": 100}).to_string();
        let key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x.rs"}));
        tl.record("Read", &key, "chunk1", &args1, 0);
        let args2 =
            serde_json::json!({"file_path": "/tmp/x.rs", "offset": 200, "limit": 300}).to_string();
        tl.record("Read", &key, "chunk2", &args2, 0);
        let out = tl.format(0);
        assert!(
            out.contains("0-100"),
            "partial reads should show offset-limit: {out}"
        );
        assert!(out.contains("200-300"), "second partial: {out}");
    }

    #[test]
    fn tool_ledger_format_empty() {
        let tl = ToolLedger::new();
        let out = tl.format(0);
        assert_eq!(out, "[seen]\n(none)");
    }

    #[test]
    fn tool_ledger_format_ages() {
        let mut tl = ToolLedger::new();
        let args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        let key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x.rs"}));
        tl.record("Read", &key, "content", &args, 0);
        let out = tl.format(5);
        assert!(out.contains("R(5t"), "age should be 5: {out}");
        let out2 = tl.format(1);
        assert!(out2.contains("R(1t"), "age should be 1: {out2}");
    }

    #[test]
    fn tool_ledger_deferred_invalidation_guards_concurrent_ordering() {
        // Simulate the Phase 2 concurrency issue: a Write and a Read
        // of the same file appear in the same batch.  The Read completes
        // first but appears second in request order.  Without deferred
        // invalidation, recording Write then Read would let the stale
        // Read survive.  With deferred invalidation, the Read is nuked
        // after the whole batch.
        let mut tl = ToolLedger::new();
        let w_args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        let w_key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x.rs"}));
        let r_args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        let r_key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/x.rs"}));
        // Record Write, then Read (simulating request order).
        tl.record("Write", &w_key, "written", &w_args, 0);
        tl.record("Read", &r_key, "stale-content", &r_args, 0);
        // No invalidation yet — Read is still present.
        let pre = tl.format(0);
        assert!(
            pre.contains("R("),
            "Read present before invalidation: {pre}"
        );
        // Apply deferred invalidation.
        tl.invalidate_after_write("/tmp/x.rs");
        let post = tl.format(0);
        assert!(
            !post.contains("R("),
            "Read nuked after deferred invalidation: {post}"
        );
        assert!(post.contains("W("), "Write entry survives: {post}");
    }

    #[test]
    fn tool_ledger_partial_only_omits_aggregate_count() {
        // When only partial reads exist (no full-file read), the
        // headline omits the aggregate count to avoid misleading
        // summing across different windows.
        let mut tl = ToolLedger::new();
        let a1 =
            serde_json::json!({"file_path": "/tmp/z.rs", "offset": 0, "limit": 100}).to_string();
        let a2 =
            serde_json::json!({"file_path": "/tmp/z.rs", "offset": 200, "limit": 100}).to_string();
        let key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/z.rs"}));
        tl.record("Read", &key, "chunk1", &a1, 0);
        tl.record("Read", &key, "chunk2", &a2, 0);
        let out = tl.format(0);
        assert!(out.contains("/tmp/z.rs R(0t"), "headline present: {out}");
        assert!(
            !out.contains("/tmp/z.rs R(0t,"),
            "no aggregate count: {out}"
        );
        assert!(out.contains("0-100:0t"), "first partial: {out}");
        assert!(out.contains("200-100:0t"), "second partial: {out}");
    }

    #[test]
    fn tool_ledger_storage_keys_are_untruncated() {
        // Distinct file paths that would collide under truncation
        // must remain separate entries in storage.
        let mut tl = ToolLedger::new();
        // truncate_ledger_value keeps head-40 + "\u{2026}" + tail-39.
        // Build two paths that share the same head and tail but
        // differ in the middle, so display labels collide.
        let head = "/tmp/HHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH"; // 40 chars
        let tail = "TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT.rs"; // 39 chars
        let path_a = format!("{head}_aaaa_{tail}");
        let path_b = format!("{head}_bbbb_{tail}");
        let args_a = serde_json::json!({"file_path": &path_a}).to_string();
        let args_b = serde_json::json!({"file_path": &path_b}).to_string();
        let key_a = ledger_key_arg(&serde_json::json!({"file_path": &path_a}));
        let key_b = ledger_key_arg(&serde_json::json!({"file_path": &path_b}));
        // Both display keys are identical after truncation.
        assert_eq!(key_a, key_b, "display keys collide by design");
        tl.record("Read", &key_a, "ca", &args_a, 0);
        tl.record("Read", &key_b, "cb", &args_b, 0);
        // Two distinct entries in storage.
        assert_eq!(tl.reads.len(), 2, "storage keys must not collide");
    }

    #[test]
    fn tool_ledger_edit_invalidates_read_without_w_entry() {
        // Edit nukes reads for the target file but does not add a W.
        let mut tl = ToolLedger::new();
        let r_args = serde_json::json!({"file_path": "/tmp/e.rs"}).to_string();
        let r_key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/e.rs"}));
        tl.record("Read", &r_key, "before", &r_args, 0);
        let e_args = serde_json::json!({"file_path": "/tmp/e.rs"}).to_string();
        let e_key = ledger_key_arg(&serde_json::json!({"file_path": "/tmp/e.rs"}));
        tl.record("Edit", &e_key, "", &e_args, 0);
        tl.invalidate_after_write("/tmp/e.rs");
        let out = tl.format(0);
        assert!(!out.contains("R("), "read nuked by Edit: {out}");
        assert!(!out.contains("W("), "no W entry for Edit: {out}");
    }

    // ── Scout observation tests ────────────────────────────────────

    #[test]
    fn fold_scout_dedups_against_parent_reads() {
        let _env = EnvGuard::lock();
        std::env::remove_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES");

        let mut ledger = ToolLedger::new();
        let args_json =
            serde_json::json!({"file_path": "/tmp/foo.rs", "offset": 0, "limit": 0}).to_string();
        // Simulate parent already having read /tmp/foo.rs
        ledger.record(
            "Read",
            &key_arg(&serde_json::json!({"file_path": "/tmp/foo.rs"})),
            "content A",
            &args_json,
            1,
        );

        let obs = FileObservations {
            items: vec![make_read_obs("/tmp/foo.rs", "content A", 0, 0)],
            truncated: false,
            written_paths: vec![],
        };
        let scout_output = format!("Done.{}", obs.render_block());

        // fold_scout_output should dedup the already-read file and return
        // only the "Done." prose.
        let folded = fold_scout_output(&scout_output, &mut ledger, 2);
        assert!(
            !folded.contains("content A"),
            "already-read file must be deduplicated, got: {folded:?}"
        );
        assert!(folded.starts_with("Done."), "prose prefix preserved");
        assert!(
            !folded.contains("__SCOUT_OBSERVATIONS__"),
            "no block markers in output"
        );
    }

    #[test]
    fn fold_scout_passes_through_unread_file() {
        let _env = EnvGuard::lock();
        std::env::remove_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES");

        let mut ledger = ToolLedger::new();
        let obs = FileObservations {
            items: vec![make_read_obs("/tmp/bar.rs", "bar content", 5, 20)],
            truncated: false,
            written_paths: vec![],
        };
        let scout_output = format!("Ran scout.{}", obs.render_block());
        let folded = fold_scout_output(&scout_output, &mut ledger, 1);
        assert!(
            folded.contains("bar content"),
            "unread file must appear in output"
        );
        assert!(folded.starts_with("Ran scout."), "prose prefix preserved");
    }

    #[test]
    fn fold_scout_invalidates_writes_and_absorbs_into_ledger() {
        let _env = EnvGuard::lock();
        std::env::remove_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES");

        let mut ledger = ToolLedger::new();
        // Parent previously read the file; worker then wrote it.
        let args_json =
            serde_json::json!({"file_path": "/tmp/shared.rs", "offset": 0, "limit": 0}).to_string();
        ledger.record(
            "Read",
            &key_arg(&serde_json::json!({"file_path": "/tmp/shared.rs"})),
            "stale content",
            &args_json,
            1,
        );

        let obs = FileObservations {
            items: vec![make_read_obs("/tmp/other.rs", "other", 0, 0)],
            truncated: false,
            written_paths: vec!["/tmp/shared.rs".into()],
        };
        let scout_output = format!("Done.{}", obs.render_block());
        let folded = fold_scout_output(&scout_output, &mut ledger, 2);

        // The stale read of /tmp/shared.rs should be invalidated.
        assert!(
            !ledger.has_observation(&make_read_obs("/tmp/shared.rs", "", 0, 0)),
            "written file must be invalidated in parent ledger"
        );
        // /tmp/other.rs should be absorbed.
        assert!(
            ledger.has_observation(&make_read_obs("/tmp/other.rs", "other", 0, 0)),
            "unread file must be absorbed into parent ledger for propagation"
        );
        assert!(
            folded.contains("other"),
            "unread file appears in rendered output"
        );
    }

    #[test]
    fn fold_scout_no_block_returns_unchanged() {
        let _env = EnvGuard::lock();
        std::env::remove_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES");

        let mut ledger = ToolLedger::new();
        let output = "plain prose without markers";
        let folded = fold_scout_output(output, &mut ledger, 1);
        assert_eq!(folded, output);
    }

    #[test]
    fn fold_scout_truncated_preserves_marker() {
        let _env = EnvGuard::lock();
        std::env::remove_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES");

        let mut ledger = ToolLedger::new();
        let obs = FileObservations {
            items: vec![],
            truncated: true,
            written_paths: vec![],
        };
        let scout_output = format!("OK.{}", obs.render_block());
        let folded = fold_scout_output(&scout_output, &mut ledger, 1);
        assert!(
            folded.contains("Some observations were omitted"),
            "truncation marker must appear"
        );
    }

    #[test]
    fn fold_scout_caps_after_dedup_and_keeps_newest() {
        let _env = EnvGuard::lock();
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "154");

        let mut ledger = ToolLedger::new();
        // Parent already read old.rs; the Scout reports it again plus new.rs.
        let old_args =
            serde_json::json!({"file_path": "/tmp/old.rs", "offset": 0, "limit": 0}).to_string();
        ledger.record(
            "Read",
            &key_arg(&serde_json::json!({"file_path": "/tmp/old.rs"})),
            "old",
            &old_args,
            1,
        );

        let obs = FileObservations {
            items: vec![
                make_read_obs("/tmp/new.rs", "new contents", 0, 0),
                make_read_obs("/tmp/old.rs", "old", 0, 0),
            ],
            truncated: false,
            written_paths: vec![],
        };
        let scout_output = format!("Done.{}", obs.render_block());
        let folded = fold_scout_output(&scout_output, &mut ledger, 2);

        assert!(
            folded.contains("new contents"),
            "newest unseen read must survive the cap, got: {folded:?}"
        );
        assert!(
            !folded.contains("### Read: /tmp/old.rs"),
            "already-seen read must be deduped before the cap is applied, got: {folded:?}"
        );
    }

    #[test]
    fn fold_scout_keeps_prefix_of_oversized_observation() {
        let _env = EnvGuard::lock();
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "126");

        let mut ledger = ToolLedger::new();
        let obs = FileObservations {
            items: vec![make_read_obs("/tmp/huge.rs", "0123456789", 0, 0)],
            truncated: false,
            written_paths: vec![],
        };
        let scout_output = format!("Done.{}", obs.render_block());
        let folded = fold_scout_output(&scout_output, &mut ledger, 1);

        assert!(
            folded.contains("### Read: /tmp/huge.rs"),
            "oversized observation must keep a prefix rather than vanishing, got: {folded:?}"
        );
        assert!(
            folded.contains("Some observations were omitted"),
            "oversized observation must carry the truncation marker"
        );
    }

    #[test]
    fn fold_scout_absorbs_landscape_and_propagates_writes() {
        let _env = EnvGuard::lock();
        std::env::remove_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES");

        let mut ledger = ToolLedger::new();
        let landscape = FileObservation {
            tool: "Landscape".into(),
            path: "/tmp/dir".into(),
            offset: 0,
            limit: 0,
            pattern: None,
            output: "tree".into(),
        };
        let obs = FileObservations {
            items: vec![
                make_read_obs("/tmp/child.rs", "child content", 0, 0),
                landscape,
            ],
            truncated: false,
            written_paths: vec!["/tmp/wrote.rs".into()],
        };
        let scout_output = format!("Child.{}", obs.render_block());
        let folded = fold_scout_output(&scout_output, &mut ledger, 2);

        assert!(folded.contains("child content"));
        assert!(folded.contains("### Landscape: /tmp/dir"));
        assert!(ledger.has_observation(&make_read_obs("/tmp/child.rs", "child content", 0, 0)));
        assert!(ledger.has_observation(&FileObservation {
            tool: "Landscape".into(),
            path: "/tmp/dir".into(),
            offset: 0,
            limit: 0,
            pattern: None,
            output: String::new(),
        }));

        // The outer ledger must report the nested write upward too.
        let reported = ledger.file_observations();
        assert!(reported.written_paths.iter().any(|p| p == "/tmp/wrote.rs"));
        // Directory landscapes now serialize as `DirLs` observations.
        assert!(reported
            .items
            .iter()
            .any(|o| { o.tool == "DirLs" && o.path == "/tmp/dir" }));
    }

    #[test]
    fn tool_ledger_snapshot_orders_by_last_access() {
        let _env = EnvGuard::lock();
        let mut ledger = ToolLedger::new();

        let a_args =
            serde_json::json!({"file_path": "/tmp/a.rs", "offset": 0, "limit": 0}).to_string();
        let b_args =
            serde_json::json!({"file_path": "/tmp/b.rs", "offset": 0, "limit": 0}).to_string();

        // Read(A), Read(B), Read(A) — display order is A → B, but snapshot
        // order must reflect last-access, so A comes after B (most recent).
        ledger.record("Read", "/tmp/a.rs", "content-a", &a_args, 0);
        ledger.record("Read", "/tmp/b.rs", "content-b", &b_args, 0);
        ledger.record("Read", "/tmp/a.rs", "content-a", &a_args, 0);

        let snapshot = ledger.file_observations();
        let a_pos = snapshot
            .items
            .iter()
            .position(|o| o.path == "/tmp/a.rs")
            .unwrap();
        let b_pos = snapshot
            .items
            .iter()
            .position(|o| o.path == "/tmp/b.rs")
            .unwrap();
        assert!(
            a_pos < b_pos,
            "A re-read last, so it must appear before B in the snapshot"
        );

        // display `[seen]` output (format) must still sort by first access.
        let formatted = ledger.format(0);
        let a_idx = formatted.find("/tmp/a.rs").unwrap();
        let b_idx = formatted.find("/tmp/b.rs").unwrap();
        assert!(
            a_idx < b_idx,
            "display order unchanged by re-read: A first, then B"
        );
    }

    #[test]
    fn tool_ledger_has_observation_grep_compares_content_hash() {
        let _env = EnvGuard::lock();
        let mut ledger = ToolLedger::new();

        let args_json = serde_json::json!({"pattern": "fn", "path": "/tmp"}).to_string();
        ledger.record("Grep", "fn", "hits: A", &args_json, 0);

        // Same pattern/path but different output — must NOT be a duplicate.
        let fresh = FileObservation {
            tool: "Grep".into(),
            path: "/tmp".into(),
            offset: 0,
            limit: 0,
            pattern: Some("fn".into()),
            output: "hits: B".into(),
        };
        assert!(
            !ledger.has_observation(&fresh),
            "different Grep output must not be deduplicated"
        );

        // Same output — IS a duplicate.
        let dup = FileObservation {
            tool: "Grep".into(),
            path: "/tmp".into(),
            offset: 0,
            limit: 0,
            pattern: Some("fn".into()),
            output: "hits: A".into(),
        };
        assert!(
            ledger.has_observation(&dup),
            "identical Grep output must be deduplicated"
        );

        // Different glob — key is identical, output hash decides.
        let different_glob = FileObservation {
            tool: "Grep".into(),
            path: "/tmp".into(),
            offset: 0,
            limit: 0,
            pattern: Some("fn".into()),
            output: "hits: C".into(),
        };
        assert!(
            !ledger.has_observation(&different_glob),
            "Grep with different glob/output must not deduplicate"
        );
    }

    #[test]
    fn fold_scout_marks_only_delivered_as_seen_and_propagates_omitted() {
        let _env = EnvGuard::lock();
        // Budget such that only the newest observation (55 bytes rendered) fits;
        // the older one (45 bytes) is pushed to omitted for propagation.
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "159");

        let mut ledger = ToolLedger::new();
        let newest = make_read_obs("/tmp/newest.rs", "newest content", 0, 0);
        let older = make_read_obs("/tmp/older.rs", "older", 0, 0);
        let obs = FileObservations {
            items: vec![newest.clone(), older.clone()],
            truncated: false,
            written_paths: vec![],
        };
        let scout_output = format!("Done.{}", obs.render_block());
        let folded = fold_scout_output(&scout_output, &mut ledger, 1);

        // newest delivered and visible in history.
        assert!(folded.contains("newest content"));
        assert!(folded.contains("### Read: /tmp/newest.rs"));

        // older NOT in history, but still in snapshot for propagation.
        assert!(!folded.contains("older"));
        let snapshot = ledger.file_observations();
        assert!(
            snapshot
                .items
                .iter()
                .any(|o| o.path == "/tmp/older.rs" && o.output == "older"),
            "omitted observations must still propagate upward"
        );

        // Only newest is "seen" for dedup.
        assert!(ledger.has_observation(&newest));
        assert!(!ledger.has_observation(&older));

        // truncation marker present because render budget was exceeded.
        assert!(folded.contains("Some observations were omitted"));
    }

    #[test]
    fn fold_scout_reserves_wrapper_budget() {
        let _env = EnvGuard::lock();
        // Budget exactly covers heading + marker only (24 + 80 = 104 bytes).
        // No observation content can fit, so the section is empty.
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "104");

        let mut ledger = ToolLedger::new();
        let obs = FileObservations {
            items: vec![make_read_obs("/tmp/x.rs", "secret", 0, 0)],
            truncated: false,
            written_paths: vec![],
        };
        let scout_output = format!("Prose.{}", obs.render_block());
        let folded = fold_scout_output(&scout_output, &mut ledger, 1);

        // The heading is emitted.
        assert!(folded.contains("## Scout observations"));
        // The marker is emitted (truncation forced by budget).
        assert!(folded.contains("Some observations were omitted"));
        // The observation content itself must NOT leak past the reserved wrapper.
        assert!(!folded.contains("secret"));

        // Observation propagated upward (via omitted list).
        let snapshot = ledger.file_observations();
        assert!(
            snapshot
                .items
                .iter()
                .any(|o| o.path == "/tmp/x.rs" && o.output == "secret"),
            "omitted observations must still propagate upward"
        );

        // But not "seen" — a later Scout returning the same file won't be
        // deduplicated away against an observation the parent never fully received.
        assert!(!ledger.has_observation(&make_read_obs("/tmp/x.rs", "secret", 0, 0)));
    }

    #[test]
    fn scout_written_paths_extracts_mutation_paths() {
        let _env = EnvGuard::lock();
        let obs = FileObservations {
            items: vec![],
            truncated: false,
            written_paths: vec!["/tmp/a.rs".into(), "/tmp/b.rs".into()],
        };
        let output = format!("Scout done.{}", obs.render_block());

        let paths = scout_written_paths(&output);
        assert_eq!(paths, vec!["/tmp/a.rs", "/tmp/b.rs"]);

        // No block → empty.
        assert!(scout_written_paths("plain text").is_empty());
    }
}
