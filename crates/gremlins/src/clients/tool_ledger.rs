use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::format::truncate_ledger_value;
use super::protocol::{FileObservation, FileObservations};

/// Deduplication tracker for tool observations across turns.
///
/// The agent loop records every Read/Grep/Write/Edit observation so
/// the next turn's compact `[seen]` index can avoid re-reading unchanged
/// content and can invalidate stale observations after writes.
///
/// Directory landscape rendering moved to
/// [`super::landscape_context::LandscapeContext`]; this struct is now a
/// pure read/grep/write/edit dedup index.
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

#[derive(Clone)]
pub(crate) struct ToolLedger {
    /// Key: (file_path, offset, limit).  Offset and limit are zero when
    /// the model reads the whole file.
    reads: std::collections::HashMap<(String, u64, u64), LedgerReadEntry>,
    /// Key: (pattern, path).  Path is empty string when not specified.
    greps: std::collections::HashMap<(String, String), LedgerGrepEntry>,
    /// Key: file_path.
    writes: std::collections::HashMap<String, LedgerWriteEntry>,
    /// Monotonic counter shared by display order and snapshot order.
    seq: usize,
    /// File paths mutated by Write or Edit, for parent-ledger invalidation.
    mutations: Vec<String>,
    /// Mutations recorded during the current turn (since last finalize_turn).
    /// Processed by finalize_turn and cleared afterward.
    pending_mutations: Vec<String>,
    /// Per-turn transcript blocks, captured at finalize_turn time.
    /// Index i is turn i. Used by the MutableModelContext::transcript method.
    turn_transcripts: Vec<String>,
}

impl ToolLedger {
    pub(crate) fn new() -> Self {
        Self {
            reads: std::collections::HashMap::new(),
            greps: std::collections::HashMap::new(),
            writes: std::collections::HashMap::new(),
            seq: 0,
            mutations: Vec::new(),
            pending_mutations: Vec::new(),
            turn_transcripts: Vec::new(),
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

    /// Record a tool execution.  Write/Edit do NOT invalidate here;
    /// caller must call `invalidate_after_write` after all observations
    /// for the turn are recorded (see Comment 1 — Phase 2 concurrency).
    pub(crate) fn record(&mut self, name: &str, output: &str, args_json: &str, turn_num: usize) {
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
            }
            "Write" => {
                let file_path = Self::parse_str(&args, "file_path").to_string();
                let age = turn_num;
                self.seq += 1;
                let seq = self.seq;
                self.writes
                    .insert(file_path.clone(), LedgerWriteEntry { age, seq });
                self.mutations.push(file_path.clone());
                self.pending_mutations.push(file_path);
            }
            "Edit" => {
                let file_path = Self::parse_str(&args, "file_path").to_string();
                if !file_path.is_empty() {
                    self.mutations.push(file_path.clone());
                    self.pending_mutations.push(file_path);
                }
            }
            "Landscape" => {}
            "Glob" => {}
            _ => {}
        }
    }

    /// Apply write/edit invalidation for a file path.  Nukes Read entries
    /// for that file and clears all Greps (any write can invalidate a grep
    /// result).  The landscape cache is managed independently by
    /// [`super::landscape_context::LandscapeContext`].
    pub(crate) fn invalidate_after_write(&mut self, file_path: &str) {
        self.reads.retain(|(fp, _, _), _| fp != file_path);
        self.greps.clear();
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

    /// Capture the current formatted ledger state as a turn transcript block.
    /// Called at the end of each turn (by finalize_turn) and after seeding.
    pub(crate) fn capture_turn_transcript(&mut self, turn_num: usize) {
        // Ensure the vec has enough capacity.
        while self.turn_transcripts.len() <= turn_num {
            self.turn_transcripts.push(String::new());
        }
        self.turn_transcripts[turn_num] = self.format(turn_num);
    }
}

// ── MutableModelContext impl ───────────────────────────────────────────────

impl super::model_context::MutableModelContext for ToolLedger {
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

    fn observe(&mut self, turn: usize, tool: &str, args: &str, output: &str) {
        self.record(tool, output, args, turn);
    }

    fn finalize_turn(&mut self, turn: usize) {
        // Apply deferred invalidations for paths written/edited this turn.
        let pending: Vec<String> = std::mem::take(&mut self.pending_mutations);
        for fp in &pending {
            self.invalidate_after_write(fp);
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

    fn system_message(&self) -> Option<String> {
        // [seen] now travels via transcript() as per-turn user-message
        // blocks, keeping the system prompt (cached prefix) stable.
        None
    }

    fn join(&mut self, other: &dyn super::model_context::MutableModelContext) {
        if let Some(other_ledger) = other.as_any().downcast_ref::<ToolLedger>() {
            // Merge reads: take the newer entry (by seq).
            for (key, entry) in &other_ledger.reads {
                match self.reads.get(key) {
                    Some(existing) if existing.seq >= entry.seq => {}
                    _ => {
                        self.reads.insert(key.clone(), entry.clone());
                    }
                }
            }
            // Merge greps.
            for (key, entry) in &other_ledger.greps {
                match self.greps.get(key) {
                    Some(existing) if existing.seq >= entry.seq => {}
                    _ => {
                        self.greps.insert(key.clone(), entry.clone());
                    }
                }
            }
            // Merge writes.
            for (key, entry) in &other_ledger.writes {
                match self.writes.get(key) {
                    Some(existing) if existing.seq >= entry.seq => {}
                    _ => {
                        self.writes.insert(key.clone(), entry.clone());
                    }
                }
            }
            // Merge mutations.
            for p in &other_ledger.mutations {
                if !self.mutations.contains(p) {
                    self.mutations.push(p.clone());
                }
            }
            // Queue child write paths for deferred invalidation in the
            // parent's `finalize_turn`, so a child's Write/Edit invalidates
            // the parent's stale reads within the same tool batch.  `join`
            // replaces the old `queue_scout_writes` path.
            for fp in other_ledger.writes.keys() {
                if !self.pending_mutations.contains(fp) {
                    self.pending_mutations.push(fp.clone());
                }
            }
            // Take the higher sequence number.
            self.seq = self.seq.max(other_ledger.seq);
            // Merge turn transcripts: pad ours to match, then overwrite empties.
            while self.turn_transcripts.len() < other_ledger.turn_transcripts.len() {
                self.turn_transcripts.push(String::new());
            }
            for (i, theirs) in other_ledger.turn_transcripts.iter().enumerate() {
                if !theirs.is_empty() && self.turn_transcripts[i].is_empty() {
                    self.turn_transcripts[i] = theirs.clone();
                }
            }
        }
    }

    fn clear_transcript(&mut self) {
        self.turn_transcripts.clear();
        self.turn_transcripts.push(String::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::format::ledger_key_arg;
    use crate::test_support::EnvGuard;

    // ── ToolLedger unit tests ──────────────────────────────────────

    #[test]
    fn tool_ledger_accumulates_reads() {
        let mut tl = ToolLedger::new();
        let args = serde_json::json!({"file_path": "/tmp/a.rs"}).to_string();
        tl.record("Read", "content1", &args, 0);
        tl.record("Read", "content1", &args, 0);
        let args2 = serde_json::json!({"file_path": "/tmp/b.rs"}).to_string();
        tl.record("Read", "content2", &args2, 0);
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
        tl.record("Read", "same", &args, 0);
        tl.record("Read", "same", &args, 0);
        tl.record("Read", "same", &args, 0);
        let out = tl.format(0);
        assert!(
            out.contains(",3×"),
            "same-hash re-reads should bump count: {out}"
        );

        // Different content replaces.
        let mut tl2 = ToolLedger::new();
        tl2.record("Read", "first", &args, 0);
        tl2.record("Read", "second", &args, 0);
        let out2 = tl2.format(0);
        assert!(!out2.contains("×"), "different hash still counts: {out2}");
    }

    #[test]
    fn tool_ledger_write_invalidates_read() {
        let mut tl = ToolLedger::new();
        let args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        tl.record("Read", "content", &args, 0);
        tl.record("Write", "written", &args, 0);
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
        tl.record("Grep", "results", &grep_args, 0);
        let write_args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        tl.record("Write", "written", &write_args, 0);
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
        tl.record("Landscape", "tree", &ls_args, 0);
        let write_args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        tl.record("Write", "written", &write_args, 0);
        tl.invalidate_after_write("/tmp/x.rs");
        let out = tl.format(0);
        assert!(
            !out.contains("L("),
            "landscape should be invalidated: {out}"
        );
    }

    #[test]
    fn tool_ledger_partial_reads() {
        let mut tl = ToolLedger::new();
        let args1 =
            serde_json::json!({"file_path": "/tmp/x.rs", "offset": 0, "limit": 100}).to_string();
        tl.record("Read", "chunk1", &args1, 0);
        let args2 =
            serde_json::json!({"file_path": "/tmp/x.rs", "offset": 200, "limit": 300}).to_string();
        tl.record("Read", "chunk2", &args2, 0);
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
        tl.record("Read", "content", &args, 0);
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
        let r_args = serde_json::json!({"file_path": "/tmp/x.rs"}).to_string();
        // Record Write, then Read (simulating request order).
        tl.record("Write", "written", &w_args, 0);
        tl.record("Read", "stale-content", &r_args, 0);
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
        tl.record("Read", "chunk1", &a1, 0);
        tl.record("Read", "chunk2", &a2, 0);
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
        let key_a = ledger_key_arg(&serde_json::json!({"file_path": &path_a}));
        let key_b = ledger_key_arg(&serde_json::json!({"file_path": &path_b}));
        let args_a = serde_json::json!({"file_path": &path_a}).to_string();
        let args_b = serde_json::json!({"file_path": &path_b}).to_string();
        // Both display keys are identical after truncation.
        assert_eq!(key_a, key_b, "display keys collide by design");
        tl.record("Read", "ca", &args_a, 0);
        tl.record("Read", "cb", &args_b, 0);
        // Two distinct entries in storage.
        assert_eq!(tl.reads.len(), 2, "storage keys must not collide");
    }

    #[test]
    fn tool_ledger_edit_invalidates_read_without_w_entry() {
        // Edit nukes reads for the target file but does not add a W.
        let mut tl = ToolLedger::new();
        let r_args = serde_json::json!({"file_path": "/tmp/e.rs"}).to_string();
        tl.record("Read", "before", &r_args, 0);
        let e_args = serde_json::json!({"file_path": "/tmp/e.rs"}).to_string();
        tl.record("Edit", "", &e_args, 0);
        tl.invalidate_after_write("/tmp/e.rs");
        let out = tl.format(0);
        assert!(!out.contains("R("), "read nuked by Edit: {out}");
        assert!(!out.contains("W("), "no W entry for Edit: {out}");
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
        ledger.record("Read", "content-a", &a_args, 0);
        ledger.record("Read", "content-b", &b_args, 0);
        ledger.record("Read", "content-a", &a_args, 0);

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
}
