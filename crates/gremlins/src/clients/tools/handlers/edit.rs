use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;

use crate::clients::tools::registry::{str_detail, ToolHandler};
use crate::clients::tools::runtime::{AgentLoopRuntime, ToolOutcome};
use crate::clients::tools::util::{blocking_string, parse_args, req_str};
use crate::core::paths::{canonicalize_or_ancestor, enforce, io_enforce, resolve};

/// Per-file mutex map: serializes `edit_sync` calls targeting the same
/// canonical path so concurrent `Edit` tool invocations never race on
/// read-edit-write. Each file gets an `Arc<Mutex<()>>`; entries are
/// created on first access and never evicted (the map is tiny in practice).
static EDIT_FILE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Acquire the per-file lock for `path` and run `f` under it.
fn with_edit_file_lock(path: &Path, f: impl FnOnce() -> String) -> String {
    let lock = {
        let mut map = EDIT_FILE_LOCKS.lock().unwrap();
        map.entry(canonicalize_or_ancestor(path))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().unwrap();
    f()
}

pub(crate) struct EditTool;

#[async_trait]
impl ToolHandler for EditTool {
    fn title_detail(&self, args: &serde_json::Value) -> Option<String> {
        str_detail(args, "file_path")
    }

    fn sandbox_check(&self, ctx: &AgentLoopRuntime, args: &serde_json::Value) -> Option<String> {
        let roots = ctx.sandbox.roots()?;
        enforce(
            roots,
            args.get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("."),
            ctx.cwd.as_deref(),
        )
    }

    fn repair_args(&self, args: serde_json::Value) -> Option<serde_json::Value> {
        let repaired = repair_edit_args(args);
        Some(repaired)
    }

    async fn invoke(
        &self,
        _id: &str,
        ctx: &AgentLoopRuntime,
        _conv: &dyn crate::clients::conversation::Conversation,
        args_json: &str,
    ) -> ToolOutcome {
        ToolOutcome::text(edit_invoke(ctx, args_json).await)
    }
}

pub(crate) async fn edit_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || edit_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

/// Build the byte offset of the start of every line in `content`.
///
/// The returned table always begins with `0` (the start of line 1) and gains an
/// entry for each newline byte: entry `k` is the byte offset where line `k + 1`
/// begins. Consequently `offs[n - 1]` is the start of line `n`, and `offs[m]`
/// is the exclusive end of line `m`.
fn compute_line_offsets(content: &str) -> Vec<usize> {
    let mut offs = vec![0usize];
    for (idx, _) in content.char_indices().filter(|(_, c)| *c == '\n') {
        offs.push(idx + 1);
    }
    offs
}

/// Map a byte offset in `content` to its 1-based line number.
///
/// An offset that falls exactly on a line start reports that line; an offset
/// in the middle of a line reports the line containing it. Offsets past the
/// end of the file clamp to the final line.
fn line_number_at(line_offsets: &[usize], byte_offset: usize) -> usize {
    match line_offsets.binary_search(&byte_offset) {
        Ok(idx) => idx + 1,
        Err(idx) => idx,
    }
}

/// Translate an inclusive line scope (`start_line` ..= `end_line`) into a
/// half-open byte range `[start, end)`.
///
/// `scope_start` is a 1-based line number. When `scope_end` is `None` the scope
/// runs to the end of the file. Line numbers past the end of the file clamp to
/// EOF, yielding a possibly-empty range.
fn scope_byte_range(
    line_offsets: &[usize],
    content_len: usize,
    scope_start: usize,
    scope_end: Option<usize>,
) -> (usize, usize) {
    let start = line_offsets
        .get(scope_start.saturating_sub(1))
        .copied()
        .unwrap_or(content_len)
        .min(content_len);
    let end = match scope_end {
        Some(e) => line_offsets
            .get(e)
            .copied()
            .unwrap_or(content_len)
            .min(content_len),
        None => content_len,
    };
    (start, end.max(start))
}

const DIVERGENCE_SNIPPET_RADIUS: usize = 120;

/// Extract a bounded, human-readable byte window around `byte_index`.
///
/// The slice is capped to `radius` bytes on either side so diagnostics stay
/// concise even for very large `old_string` values. UTF-8 boundaries are not
/// guaranteed for the byte range, so the slice is rendered lossily rather than
/// risking a panic.
fn snippet_around(s: &str, byte_index: usize, radius: usize) -> String {
    let bytes = s.as_bytes();
    let start = byte_index.saturating_sub(radius);
    let end = (byte_index + radius).min(bytes.len());
    String::from_utf8_lossy(&bytes[start..end]).into_owned()
}

/// Trim a line to `max_chars` characters for compact diagnostics, appending
/// an ellipsis when truncated.
fn clip_line(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// Walk `old_str` and the file content byte-by-byte from `content_start` and
/// report the first point where they diverge.
///
/// `content_end` is the exclusive end of the search window, so scoped searches
/// never read past the scope. When one side is shorter than the other the
/// report uses the byte offset where that side ends, and the snippets show
/// what each side has at that point.
fn divergence_report(
    content: &str,
    content_start: usize,
    content_end: usize,
    old_str: &str,
) -> String {
    let actual = &content[content_start..content_end];
    let actual_bytes = actual.as_bytes();
    let expected_bytes = old_str.as_bytes();
    let common = actual_bytes.len().min(expected_bytes.len());

    let mut rel = 0;
    while rel < common && actual_bytes[rel] == expected_bytes[rel] {
        rel += 1;
    }

    let divergence_offset = content_start + rel;
    let expected_snippet = snippet_around(old_str, rel, DIVERGENCE_SNIPPET_RADIUS);
    let actual_snippet = snippet_around(actual, rel, DIVERGENCE_SNIPPET_RADIUS);

    format!(
        " — diverges at byte offset {divergence_offset}:\n  expected: {expected_snippet:?}\n  actual:   {actual_snippet:?}"
    )
}

fn edit_not_found_diagnostic(
    content: &str,
    file_path: &str,
    old_str: &str,
    edit_num: usize,
    scope_start: Option<usize>,
    scope_end: Option<usize>,
) -> String {
    let scope_label = match scope_start {
        Some(s) => match scope_end {
            Some(e) => format!(" between lines {s}-{e}"),
            None => format!(" from line {s} to end of file"),
        },
        None => String::new(),
    };

    let first_line_raw = old_str.lines().next().unwrap_or("");
    // `needle` is the trimmed first line, but `old_str` still carries any
    // leading whitespace. Re-anchor the divergence walk to where that
    // whitespace begins so the byte-by-byte comparison measures the real
    // mismatch instead of tripping over the stripped indent.
    let leading_ws = first_line_raw.len() - first_line_raw.trim_start().len();
    let first_line = first_line_raw.trim();
    let needle: &str = if first_line.is_empty() {
        return format!(
            "Error: old_string not found{scope_label} in {file_path} (edit {edit_num}) — first line is empty or whitespace-only"
        );
    } else if first_line.chars().count() > 80 {
        let byte_idx = first_line
            .char_indices()
            .nth(80)
            .map(|(i, _)| i)
            .unwrap_or(first_line.len());
        &first_line[..byte_idx]
    } else {
        first_line
    };

    // Restrict the "did you mean" hint to the scope window when a scope is set,
    // so the suggestion reflects what the model actually constrained.
    let line_offsets = compute_line_offsets(content);
    let (window_start, window_end) = match scope_start {
        Some(s) => scope_byte_range(&line_offsets, content.len(), s, scope_end),
        None => (0, content.len()),
    };

    // When a scope is set and the scope-window search finds nothing, fall
    // back to a full-file search before giving up. If the needle exists
    // elsewhere in the file the scope is stale (prior edits shifted line
    // numbers); if it's nowhere, the content is genuinely wrong.
    let scope_is_proper_subset = (scope_start.is_some() || scope_end.is_some())
        && (window_start > 0 || window_end < content.len());
    let scoped_hint = if let Some(rel) = content[window_start..window_end].find(needle) {
        let pos = window_start + rel;
        let line_no = line_number_at(&line_offsets, pos);
        let context_start = content[..pos].rfind('\n').map_or(0, |n| n + 1);
        let context_end = content[pos..].find('\n').map_or(content.len(), |n| pos + n);
        let cap_byte = content[context_start..]
            .char_indices()
            .nth(200)
            .map(|(i, _)| context_start + i)
            .unwrap_or(content.len());
        let context_end = context_end.min(cap_byte);
        let context = &content[context_start..context_end];
        // Account for the leading whitespace trimmed off `needle` without
        // stepping back across the scope boundary.
        let content_start = pos.saturating_sub(leading_ws).max(window_start);
        let divergence = divergence_report(content, content_start, window_end, old_str);
        Some(format!(
            " — did you mean to match near line {line_no}? Found:\n{context}\n{divergence}"
        ))
    } else {
        None
    };

    // Full-file fallback: only when a scope was set and the scoped search
    // came up empty. If the needle exists anywhere in the full file, the
    // scope is stale rather than the content being wrong.
    let hint = if let Some(scoped) = scoped_hint {
        scoped
    } else if scope_is_proper_subset {
        if let Some(rel) = content.find(needle) {
            // The trimmed needle matched, but verify the complete old_str
            // also matches here before declaring this as an out-of-scope
            // occurrence. A partial first-line match (e.g. "alpha" from
            // "alpha\ngamma" matching "alpha\nbeta") is a genuine
            // content mismatch, not a stale scope.
            if content[rel..].starts_with(old_str) {
                let line_no = line_number_at(&line_offsets, rel);
                format!(
                    " — first occurrence at line {line_no} (outside the requested scope; \
                     prior edits may have shifted line numbers — try without \
                     start_line/end_line or re-read the file)"
                )
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    format!(
        "Error: old_string not found{scope_label} in {file_path} (edit {edit_num}) — first line: {needle:?}{hint}"
    )
}

/// A single replacement: the literal `old_str` is located by exact match and
/// swapped for `new_str`. An optional inclusive line scope (`scope_start`
/// ..= `scope_end`) constrains both which occurrences are eligible for the
/// uniqueness check and which occurrence is replaced.
#[derive(Debug)]
struct EditOp {
    old_str: String,
    new_str: String,
    scope_start: Option<usize>,
    scope_end: Option<usize>,
}

fn parse_edits(edits_arr: &[serde_json::Value], file_path: &str) -> Result<Vec<EditOp>, String> {
    let mut edits: Vec<EditOp> = Vec::with_capacity(edits_arr.len());
    for (i, e) in edits_arr.iter().enumerate() {
        let obj = match e.as_object() {
            Some(o) => o,
            None => {
                return Err(format!(
                    "Error: edit {} is not an object in {file_path}",
                    i + 1
                ))
            }
        };

        // `old_string` and `new_string` are always required.
        let old_str = match obj.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return Err(format!(
                    "Error: edit {} missing 'old_string' in {file_path}",
                    i + 1
                ))
            }
        };
        let new_str = match obj.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return Err(format!(
                    "Error: edit {} missing 'new_string' in {file_path}",
                    i + 1
                ))
            }
        };

        // Optional scope: `start_line` may stand alone (open-ended downward);
        // `end_line` without `start_line` defaults scope_start to 1.
        // Zero values are clamped to 1; start_line > end_line is accepted
        // and handled by scope_byte_range at execution time.
        let mut scope_start = match obj.get("start_line") {
            Some(v) => match v.as_u64() {
                Some(0) => Some(1usize),
                Some(n) => Some(n as usize),
                None => {
                    return Err(format!(
                        "Error: edit {} 'start_line' must be an integer in {file_path}",
                        i + 1
                    ))
                }
            },
            None => None,
        };
        let scope_end = match obj.get("end_line") {
            Some(v) => match v.as_u64() {
                Some(0) => Some(1usize),
                Some(n) => Some(n as usize),
                None => {
                    return Err(format!(
                        "Error: edit {} 'end_line' must be an integer in {file_path}",
                        i + 1
                    ))
                }
            },
            None => None,
        };
        // When end_line is given without start_line, default start to line 1.
        if scope_start.is_none() && scope_end.is_some() {
            scope_start = Some(1);
        }

        edits.push(EditOp {
            old_str,
            new_str,
            scope_start,
            scope_end,
        });
    }
    Ok(edits)
}

struct AppliedEdit {
    start: usize,
    old_len: usize,
    new_len: usize,
}

/// Track a byte position from the accumulated buffer state immediately after
/// `from_edit` was applied through every subsequent edit, returning its final
/// position in the result buffer.
///
/// A later edit shifts positions after its replaced region by its length
/// delta; positions at or before the match start are untouched, and a position
/// that falls inside the replaced region collapses to the start of that
/// replacement (the original byte no longer exists).
fn map_position_forward(applied: &[AppliedEdit], from_edit: usize, mut position: usize) -> usize {
    for later in &applied[from_edit + 1..] {
        if position <= later.start {
            continue;
        }
        let region_end = later.start + later.old_len;
        if position >= region_end {
            position =
                (position as isize + later.new_len as isize - later.old_len as isize) as usize;
        } else {
            // inside the replaced region — collapse to its start
            position = later.start;
        }
    }
    position
}

/// Normalize common Unicode confusables to ASCII equivalents for fuzzy matching.
/// Only the match location is found via normalized text; the replacement still
/// operates on the original bytes so no data is lost.
fn normalize_for_fuzzy_match(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            // Smart quotes → straight quotes
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => out.push('"'),
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => out.push('\''),
            // Unicode dashes → ASCII hyphen
            '\u{2013}' | '\u{2014}' | '\u{2015}' => out.push('-'),
            // Special spaces → regular space
            '\u{00A0}' | '\u{2009}' | '\u{200A}' | '\u{202F}' => out.push(' '),
            // Fullwidth Latin uppercase A-Z
            c if ('\u{FF21}'..='\u{FF3A}').contains(&c) => {
                let ascii = (c as u32 - 0xFF21 + 'A' as u32) as u8 as char;
                out.push(ascii);
            }
            // Fullwidth Latin lowercase a-z
            c if ('\u{FF41}'..='\u{FF5A}').contains(&c) => {
                let ascii = (c as u32 - 0xFF41 + 'a' as u32) as u8 as char;
                out.push(ascii);
            }
            // Fullwidth digits 0-9
            c if ('\u{FF10}'..='\u{FF19}').contains(&c) => {
                let ascii = (c as u32 - 0xFF10 + '0' as u32) as u8 as char;
                out.push(ascii);
            }
            // Ellipsis → three dots
            '\u{2026}' => out.push_str("..."),
            // Bullet → asterisk
            '\u{2022}' => out.push('*'),
            // Keep everything else as-is
            c => out.push(c),
        }
    }
    out
}

/// Collect all byte-positions where `needle` occurs in `haystack`,
/// offset by `window_offset` so results are absolute positions in the
/// original buffer. Advances by the first character's UTF-8 length so
/// the next search never starts inside a multi-byte character, while
/// still detecting overlapping occurrences.
fn find_all_matches(haystack: &str, needle: &str, window_offset: usize) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut search_from = 0;
    let advance_by = needle.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
    while let Some(pos) = haystack[search_from..].find(needle) {
        positions.push(search_from + pos + window_offset);
        search_from += pos + advance_by;
    }
    positions
}

/// Returned by [`apply_edits`]: the final buffer, applied-edit line ranges, and
/// whether any `\n` unescape was performed.
type ApplyEditsResult = Result<(String, Vec<(usize, usize)>, bool), String>;

fn apply_edits(content: &str, edits: &[EditOp], file_path: &str) -> ApplyEditsResult {
    let mut buffer = content.to_string();
    // Record each applied edit's replacement byte span at application time.
    // Line ranges are computed afterwards against the final buffer, so a later
    // edit that inserts or deletes lines before an earlier edit's position
    // still reports the earlier edit at its true final location.
    let mut applied: Vec<AppliedEdit> = Vec::with_capacity(edits.len());
    let mut any_unescape = false;

    // Validate and apply edits sequentially. Each edit sees the accumulated
    // state of all prior edits in the same batch, enabling chained edits
    // (edit 1 renames a symbol, edit 2 uses the new name) to work in a
    // single tool call. The buffer lives only in memory until all edits
    // succeed — we never leave a partially-edited file on disk.
    for (i, edit) in edits.iter().enumerate() {
        let old_str = &edit.old_str;
        if old_str.is_empty() {
            return Err(format!(
                "Error: old_string is empty in {file_path} (edit {})",
                i + 1
            ));
        }

        let content_len = buffer.len();

        // Compute line offsets lazily — only when an edit actually uses a
        // line scope. Unscoped edits never pay the full-file scan, and a
        // non-unique error recomputes them below when it needs line numbers.
        let line_offsets = edit.scope_start.map(|_| compute_line_offsets(&buffer));

        // When a scope is set, search only the scope byte window so that
        // multiline needles overlapping earlier out-of-scope occurrences
        // are not shadowed by a global walk.
        let scope = edit.scope_start.map(|s| {
            scope_byte_range(
                line_offsets.as_deref().unwrap(),
                content_len,
                s,
                edit.scope_end,
            )
        });
        let search_window = match scope {
            Some((s, e)) => &buffer[s..e],
            None => &buffer,
        };
        let window_offset = scope.map_or(0, |(s, _)| s);

        // Collect all match positions with find + advance-by-char.
        let mut match_positions = find_all_matches(search_window, old_str, window_offset);

        if match_positions.is_empty() {
            // Newline unescape retry: the LLM may have seen a literal `\n`
            // in Read output and interpreted it as a JSON escape, producing
            // a real newline byte in old_string. Convert real newlines back
            // to the literal two-byte sequence and retry so the needle
            // matches what the file actually contains. Only `\n` is
            // converted: tabs and carriage returns in old_string may be
            // genuine indentation or line endings, not misinterpreted
            // escapes, so rewriting them could break otherwise-valid input.
            let mut newline_unescaped = String::with_capacity(old_str.len());
            let mut had_escape = false;
            for ch in old_str.chars() {
                match ch {
                    '\n' => {
                        newline_unescaped.push_str("\\n");
                        had_escape = true;
                    }
                    _ => newline_unescaped.push(ch),
                }
            }
            if had_escape {
                match_positions =
                    find_all_matches(search_window, &newline_unescaped, window_offset);

                if !match_positions.is_empty() {
                    if match_positions.len() > 1 {
                        return Err(format!(
                            "Error: old_string is not unique in {file_path} (edit {}) — {} matches found after newline unescaping. Try scoping with start_line/end_line.",
                            i + 1,
                            match_positions.len()
                        ));
                    }
                    let start = match_positions[0];
                    let old_len = newline_unescaped.len();
                    buffer.replace_range(start..start + old_len, &edit.new_str);
                    applied.push(AppliedEdit {
                        start,
                        old_len,
                        new_len: edit.new_str.len(),
                    });
                    any_unescape = true;
                    continue;
                }
            }
        }

        if match_positions.is_empty() {
            // Fuzzy fallback: normalize Unicode confusables and retry.
            // Always attempt when exact match fails — both the query may
            // contain confusables and the buffer may contain confusables.
            let normalized_old = normalize_for_fuzzy_match(old_str);
            let normalized_window = normalize_for_fuzzy_match(search_window);
            let mut fuzzy_search_from = 0;
            let fuzzy_advance_by = normalized_old
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            while let Some(fuzzy_pos) =
                normalized_window[fuzzy_search_from..].find(normalized_old.as_str())
            {
                let norm_match_start = fuzzy_search_from + fuzzy_pos;
                fuzzy_search_from = norm_match_start + fuzzy_advance_by;

                // Map the normalized byte position back to the original buffer.
                // Walk original chars, accumulating normalized *byte* lengths.
                // After the loop, orig_pos points just past the consumed chars
                // (i.e. the start of the next character).
                let mut orig_pos = 0usize;
                let mut norm_bytes_consumed = 0usize;
                for (byte_idx, oc) in search_window.char_indices() {
                    if norm_bytes_consumed >= norm_match_start {
                        break;
                    }
                    let norm_char = normalize_for_fuzzy_match(&oc.to_string());
                    norm_bytes_consumed += norm_char.len();
                    orig_pos = byte_idx + oc.len_utf8();
                }
                // Require exact normalized boundary — reject matches that
                // start mid-character.
                if norm_bytes_consumed != norm_match_start {
                    continue;
                }
                let abs_pos = orig_pos + window_offset;

                // Compute the span length in the original buffer: walk
                // forward until the accumulated normalized output reaches
                // `normalized_old.len()` bytes.
                let mut span_len = 0usize;
                let mut norm_span_bytes = 0usize;
                for (_, oc) in search_window[orig_pos..].char_indices() {
                    if norm_span_bytes >= normalized_old.len() {
                        break;
                    }
                    let norm_char = normalize_for_fuzzy_match(&oc.to_string());
                    norm_span_bytes += norm_char.len();
                    span_len += oc.len_utf8();
                }
                // Verify the collected span normalizes to the same string.
                let matched_bytes =
                    &buffer.as_bytes()[abs_pos..(abs_pos + span_len).min(buffer.len())];
                let matched_str = String::from_utf8_lossy(matched_bytes);
                if normalize_for_fuzzy_match(&matched_str) == normalized_old {
                    match_positions.push(abs_pos);
                }
            }

            if !match_positions.is_empty() {
                // Fuzzy match(es) found. Compute the actual span length
                // for the first match — it may differ from old_str.len().
                let fuzzy_start = match_positions[0];
                let mut fuzzy_span_len = 0usize;
                let mut norm_span_bytes = 0usize;
                for (_, oc) in buffer[fuzzy_start..].char_indices() {
                    if norm_span_bytes >= normalized_old.len() {
                        break;
                    }
                    let norm_char = normalize_for_fuzzy_match(&oc.to_string());
                    norm_span_bytes += norm_char.len();
                    fuzzy_span_len += oc.len_utf8();
                }
                // Apply the replacement with the computed span.
                buffer.replace_range(fuzzy_start..fuzzy_start + fuzzy_span_len, &edit.new_str);
                applied.push(AppliedEdit {
                    start: fuzzy_start,
                    old_len: fuzzy_span_len,
                    new_len: edit.new_str.len(),
                });
                // Check uniqueness; bail if > 1 fuzzy match found.
                if match_positions.len() > 1 {
                    return Err(format!(
                        "Error: old_string is not unique after fuzzy matching in {file_path} (edit {}) — {} ambiguous matches found. Try using the exact characters from the file.",
                        i + 1,
                        match_positions.len()
                    ));
                }
                continue; // proceed to next edit
            }

            if match_positions.is_empty() {
                // When a batched edit (not the first) fails to find its
                // old_string in the accumulated buffer, check whether it
                // exists in the *original* file content. If so, the problem
                // is a batch-order issue: prior edits modified the region
                // and this edit should run first or in a separate call.
                // Only fire the batch-order message when the old_string is
                // genuinely absent from the accumulated buffer. If it still
                // exists in the buffer (just outside a stale scope window),
                // let edit_not_found_diagnostic produce the more precise
                // scope-stale diagnostic from Change 2.
                if i > 0 && !buffer.contains(old_str) && content.contains(old_str) {
                    return Err(format!(
                        "Error: old_string not found in file after prior edits modified \
                     lines surrounding the target (edit {}). The old_string \
                     matches the original file content — try splitting this \
                     into a separate Edit call, reorder edits so this one \
                     runs first, or verify your edits don't target overlapping \
                     regions.",
                        i + 1
                    ));
                }
                return Err(edit_not_found_diagnostic(
                    &buffer,
                    file_path,
                    old_str,
                    i + 1,
                    edit.scope_start,
                    edit.scope_end,
                ));
            }
        }

        if match_positions.len() > 1 {
            // Convert byte positions to 1-based line numbers so the model
            // can immediately supply start_line/end_line to disambiguate.
            // Deduplicate so a self-overlapping needle (e.g. "aa" in "aaa")
            // does not list the same line twice. Each reported line also
            // carries a trimmed snippet so the model can pick the right
            // occurrence without a follow-up Read — a bare `}` or a block
            // ending in `}` is otherwise ambiguous.
            let line_offsets = line_offsets.unwrap_or_else(|| compute_line_offsets(&buffer));
            let mut entries: Vec<(String, String)> = Vec::new();
            for &pos in &match_positions {
                let line_no = match line_offsets.binary_search(&pos) {
                    Ok(idx) => idx + 1,
                    Err(idx) => idx,
                };
                let line_no_str = line_no.to_string();
                if entries
                    .last()
                    .map(|(n, _)| n == &line_no_str)
                    .unwrap_or(false)
                {
                    continue;
                }
                let start = line_offsets.get(line_no - 1).copied().unwrap_or(0);
                let end = line_offsets.get(line_no).copied().unwrap_or(buffer.len());
                let line = buffer[start..end].trim_end_matches(&['\r', '\n'][..]);
                entries.push((line_no_str, clip_line(line, 120)));
            }

            let scope_label = if edit.scope_start.is_some() {
                "matches within scope at lines "
            } else {
                "matches at lines "
            };

            let total = entries.len();
            let shown_count = total.min(10);
            let lines_shown = entries[..shown_count]
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if total > 10 {
                format!(" and {} more", total - 10)
            } else {
                String::new()
            };
            let context = entries[..shown_count]
                .iter()
                .map(|(n, snippet)| format!("  line {n}: {snippet:?}"))
                .collect::<Vec<_>>()
                .join("\n");

            return Err(format!(
                "Error: old_string is not unique in {file_path} (edit {}) — {}{}{}{}",
                i + 1,
                scope_label,
                lines_shown,
                suffix,
                context
            ));
        }

        // Single match — apply to the accumulated buffer.
        let start = match_positions[0];
        let old_len = old_str.len();
        buffer.replace_range(start..start + old_len, &edit.new_str);
        applied.push(AppliedEdit {
            start,
            old_len,
            new_len: edit.new_str.len(),
        });
    }

    // Translate each replacement's byte span into the final buffer (after all
    // edits are applied), then map each to a 1-based line extent. Line offsets
    // are computed once here rather than rescanning the whole buffer per edit.
    let offsets = compute_line_offsets(&buffer);
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(applied.len());
    for (i, edit) in applied.iter().enumerate() {
        let final_start = map_position_forward(&applied, i, edit.start);
        let start_line = line_number_at(&offsets, final_start);
        let end_line = if edit.new_len == 0 {
            start_line
        } else {
            let final_end = map_position_forward(&applied, i, edit.start + edit.new_len);
            if final_end > final_start {
                line_number_at(&offsets, final_end - 1)
            } else {
                start_line
            }
        };
        ranges.push((start_line, end_line));
    }

    Ok((buffer, ranges, any_unescape))
}

const SUCCESS_SNIPPET_RADIUS: usize = 3;
const SUCCESS_EXPANDED_EDIT_CAP: usize = 6;
const SUCCESS_WINDOW_LINE_CAP: usize = 12;

/// Render one line of the success report with its 1-based line number.
fn render_success_line(content: &str, offsets: &[usize], line_no: usize) -> String {
    let line_start = offsets.get(line_no - 1).copied().unwrap_or(content.len());
    let line_end = offsets.get(line_no).copied().unwrap_or(content.len());
    let line = content[line_start..line_end].trim_end_matches(&['\r', '\n'][..]);
    format!("  {line_no}: {}\n", clip_line(line, 120))
}

/// A contiguous block of lines already bounded to the per-edit cap.
struct SuccessPiece {
    /// Bounded line segments to render for this edit (head and, for an
    /// oversized window, tail).
    segments: Vec<(usize, usize)>,
    /// Interior lines omitted from an oversized window, inclusive.
    skipped: Option<(usize, usize)>,
}

/// Render the bounded post-edit confirmation report returned by a successful
/// `edit_sync` call.
///
/// Each applied edit contributes a short, numbered snippet of the final file
/// around its replacement, so the caller can confirm where the edit landed
/// without a follow-up `Read`. Per-edit snippets are bounded *before* shared
/// context is deduplicated, so an edit whose context window would otherwise be
/// swallowed by an adjacent oversized block is still shown. Overlapping or
/// touching segments are coalesced after capping, at most the first
/// `SUCCESS_EXPANDED_EDIT_CAP` edits are expanded (the remainder are summarized
/// by a trailing count), and every snippet is capped at
/// `SUCCESS_WINDOW_LINE_CAP` lines.
fn render_success(
    content: &str,
    ranges: &[(usize, usize)],
    file_path: &str,
    original_lines: usize,
    final_lines: usize,
    diff: Option<&str>,
    unescape_used: bool,
) -> String {
    let offsets = compute_line_offsets(content);
    let last_line = offsets.len();

    // Expand each replacement's line extent into a 1-based inclusive window
    // with `SUCCESS_SNIPPET_RADIUS` lines of context on either side. Sort the
    // windows by start line first — ranges arrive in edit-application order,
    // which need not be monotonic in the final file.
    let mut windows: Vec<(usize, usize)> = ranges
        .iter()
        .map(|&(start_line, end_line)| {
            let start = start_line.saturating_sub(SUCCESS_SNIPPET_RADIUS).max(1);
            let end = end_line
                .saturating_add(SUCCESS_SNIPPET_RADIUS)
                .min(last_line);
            (start, end)
        })
        .collect();
    windows.sort_unstable_by_key(|&(start, _)| start);

    let expanded = windows.len().min(SUCCESS_EXPANDED_EDIT_CAP);

    // Bound each selected edit before coalescing. An oversized window is split
    // into a head and tail around its omitted interior so the surrounding
    // context is still labeled while the replacement's endpoints stay visible.
    let mut pieces: Vec<SuccessPiece> = Vec::with_capacity(expanded);
    for &(start, end) in windows.iter().take(expanded) {
        let block_len = end - start + 1;
        if block_len > SUCCESS_WINDOW_LINE_CAP {
            let head_end = start + SUCCESS_WINDOW_LINE_CAP / 2 - 1;
            let tail_start = end - (SUCCESS_WINDOW_LINE_CAP / 2) + 1;
            pieces.push(SuccessPiece {
                segments: vec![(start, head_end), (tail_start, end)],
                skipped: Some((head_end + 1, tail_start.saturating_sub(1))),
            });
        } else {
            pieces.push(SuccessPiece {
                segments: vec![(start, end)],
                skipped: None,
            });
        }
    }

    // Deduplicate shared context by merging touching/overlapping bounded
    // segments into union intervals. Segments are sorted so a later edit that
    // overlaps an earlier edit's skipped interior still lands in file order.
    let mut segments: Vec<(usize, usize)> = pieces
        .iter()
        .flat_map(|piece| piece.segments.iter().copied())
        .collect();
    segments.sort_unstable_by_key(|&(start, _)| start);

    let mut intervals: Vec<(usize, usize)> = Vec::new();
    for (start, end) in segments {
        match intervals.last_mut() {
            Some((_, last_end)) if start <= last_end.saturating_add(1) => {
                *last_end = (*last_end).max(end);
            }
            _ => intervals.push((start, end)),
        }
    }

    let delta = final_lines as isize - original_lines as isize;
    let delta_sign = if delta >= 0 { "+" } else { "" };
    let mut out = format!(
        "Applied {} edit{} to {file_path}. ({} lines, {}{} vs original)\n",
        ranges.len(),
        if ranges.len() == 1 { "" } else { "s" },
        final_lines,
        delta_sign,
        delta
    );

    if unescape_used {
        out.push_str("(note: one or more edits matched via newline unescaping — the LLM may have misinterpreted a literal \\n in Read output as a JSON escape)\n");
    }

    if expanded < windows.len() {
        let remaining = windows.len() - expanded;
        out.push_str(&format!(
            "({remaining} more context block{} not shown)\n",
            if remaining == 1 { "" } else { "s" }
        ));
    }

    // Thread the diff block after the summary line but before context snippets.
    if let Some(d) = diff {
        out.push('\n');
        out.push_str(d);
    }

    for (interval_idx, &(start, end)) in intervals.iter().enumerate() {
        if interval_idx > 0 {
            let previous_end = intervals[interval_idx - 1].1;
            let gap_start = previous_end.saturating_add(1);
            let gap_end = start.saturating_sub(1);

            // A gap that is the omitted interior of a single oversized edit is
            // rendered as a line-count marker; any other non-contiguous gap is
            // a separate context block and gets the block separator.
            let is_skipped_interior = pieces.iter().any(|piece| {
                piece.skipped.is_some_and(|(skip_start, skip_end)| {
                    skip_start <= gap_start && gap_end <= skip_end
                })
            });
            if is_skipped_interior {
                out.push_str(&format!("  ... {} lines ...\n", gap_end - gap_start + 1));
            } else {
                out.push_str("--\n");
            }
        }

        for line_no in start..=end {
            out.push_str(&render_success_line(content, &offsets, line_no));
        }
    }

    out
}

/// Silently repair three common JSON malformations before schema validation.
/// This is a pre-validation pass — if repair fails, existing schema validation
/// still catches it.
pub(crate) fn repair_edit_args(args: serde_json::Value) -> serde_json::Value {
    let mut args = args;

    // Repair 1: `edits` is a JSON-encoded string → parse it
    if let Some(edits_val) = args.get("edits") {
        if let Some(s) = edits_val.as_str() {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("edits".to_string(), parsed);
                }
            }
        }
    }

    // Repair 2: `edits` is an object with `old_string` key (not an array) → wrap in array
    if let Some(edits_val) = args.get("edits") {
        if edits_val.is_object() && edits_val.get("old_string").is_some() {
            if let Some(obj) = args.as_object_mut() {
                let single = obj.remove("edits").unwrap();
                obj.insert("edits".to_string(), serde_json::json!([single]));
            }
        }
    }

    // Repair 3: Top-level `old_string` + `new_string` keys exist without `edits` → merge
    if !args.get("edits").is_some()
        && args.get("old_string").is_some()
        && args.get("new_string").is_some()
    {
        if let Some(obj) = args.as_object_mut() {
            let old = obj.remove("old_string").unwrap();
            let new = obj.remove("new_string").unwrap();
            let mut edit_obj = serde_json::json!({
                "old_string": old,
                "new_string": new,
            });
            // Carry over optional scope keys
            for key in &["start_line", "end_line"] {
                if let Some(v) = obj.remove(*key) {
                    edit_obj[key] = v;
                }
            }
            obj.insert("edits".to_string(), serde_json::json!([edit_obj]));
        }
    }

    args
}

const DIFF_LINE_CAP: usize = 60;

/// Compute a simple line-based unified diff between `original` and `updated`.
/// Walk forward for a common prefix, backward for a common suffix, and emit
/// the changed lines between them. Falls back to a note when `lines()` obscures
/// changes (e.g. a trailing newline was added/removed). Output is bounded to
/// `DIFF_LINE_CAP` lines to avoid blowing up model history on large files.
fn compute_diff(original: &str, updated: &str) -> Option<String> {
    if original == updated {
        return None;
    }
    let orig_lines: Vec<&str> = original.lines().collect();
    let upd_lines: Vec<&str> = updated.lines().collect();

    // Common prefix
    let prefix = orig_lines
        .iter()
        .zip(upd_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();

    // Common suffix (from the end, but not overlapping prefix)
    let suffix = orig_lines[prefix..]
        .iter()
        .rev()
        .zip(upd_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let orig_start = prefix;
    let orig_end = orig_lines.len().saturating_sub(suffix);
    let upd_start = prefix;
    let upd_end = upd_lines.len().saturating_sub(suffix);

    // If lines() obscured the change (e.g. trailing newline), produce a note.
    if orig_start >= orig_end && upd_start >= upd_end {
        return Some("```diff\n(whitespace-only change)\n```\n".to_string());
    }

    let orig_count = orig_end.saturating_sub(orig_start);
    let upd_count = upd_end.saturating_sub(upd_start);
    let total_diff_lines = orig_count + upd_count;
    let omitted = total_diff_lines.saturating_sub(DIFF_LINE_CAP);

    let half = DIFF_LINE_CAP / 2;
    let mut diff = String::from("```diff\n");
    if omitted > 0 {
        let orig_take = orig_count.min(half);
        let upd_take = upd_count.min(half);
        for line in orig_lines[orig_start..][..orig_take].iter() {
            diff.push_str(&format!("-{}\n", line));
        }
        for line in upd_lines[upd_start..][..upd_take].iter() {
            diff.push_str(&format!("+{}\n", line));
        }
        diff.push_str(&format!("... {} diff lines omitted ...\n", omitted));
    } else {
        if orig_start < orig_end {
            for line in &orig_lines[orig_start..orig_end] {
                diff.push_str(&format!("-{}\n", line));
            }
        }
        if upd_start < upd_end {
            for line in &upd_lines[upd_start..upd_end] {
                diff.push_str(&format!("+{}\n", line));
            }
        }
    }
    diff.push_str("```\n");
    Some(diff)
}

/// Detect the line ending style from the file content.
/// Returns `"\r\n"` if CRLF is found, `None` for LF (the default).
fn detect_line_ending(content: &str) -> Option<&'static str> {
    let bytes = content.as_bytes();
    if bytes.windows(2).any(|w| w == b"\r\n") {
        Some("\r\n")
    } else {
        None
    }
}

/// Strip the UTF-8 BOM if present.
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{FEFF}').unwrap_or(content)
}

pub(crate) fn edit_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let args = repair_edit_args(args);
    let file_path = match req_str(&args, "file_path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let edits_arr = match args.get("edits").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return "Error: missing 'edits'".into(),
    };
    if edits_arr.is_empty() {
        return "Error: no edits provided".into();
    }

    let path = resolve(file_path, cwd);
    if let Some(roots) = roots {
        if let Some(err) = io_enforce(&path, roots) {
            return err;
        }
    }

    // Serialize read-edit-write per file: two concurrent `Edit` calls
    // targeting the same path would otherwise race — both read the
    // original content, and the later write silently discards the first.
    with_edit_file_lock(&path, || {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error: {e}"),
        };
        let had_bom = content.starts_with('\u{FEFF}');
        let line_ending = detect_line_ending(&content);
        // Normalize to LF before editing so CRLF files don't double-convert.
        let content = strip_bom(&content).replace("\r\n", "\n");
        let original_lines = content.lines().count();

        let mut edits = match parse_edits(edits_arr, file_path) {
            Ok(e) => e,
            Err(e) => return e,
        };
        // Normalize edit strings to LF so CRLF needles/new_strings work
        // against the LF-normalized content and restoration produces correct
        // line endings (CRLF→LF normalization, then LF→CRLF restoration).
        for edit in &mut edits {
            edit.old_str = edit.old_str.replace("\r\n", "\n");
            edit.new_str = edit.new_str.replace("\r\n", "\n");
        }

        let (mut updated, ranges, any_unescape) = match apply_edits(&content, &edits, file_path) {
            Ok(u) => u,
            Err(e) => return e,
        };
        // Restore line endings (safe now: the buffer is pure LF so no double-convert).
        if line_ending == Some("\r\n") {
            updated = updated.replace('\n', "\r\n");
        }
        let final_lines = updated.lines().count();

        // Build result and diff against the BOM-stripped, LF-normalized content
        // before the BOM is re-prepended, so the diff reflects real changes.
        let diff = compute_diff(&content, &updated);
        let result = render_success(
            &updated,
            &ranges,
            file_path,
            original_lines,
            final_lines,
            diff.as_deref(),
            any_unescape,
        );

        // Restore BOM after diff/render so it doesn't leak into context snippets.
        if had_bom {
            updated.insert(0, '\u{FEFF}');
        }

        if let Err(e) = std::fs::write(&path, &updated) {
            return format!("Error: {e}");
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_edits_match_mode() {
        let edits = parse_edits(
            &[serde_json::json!({"old_string": "a", "new_string": "b"})],
            "f.txt",
        )
        .unwrap();
        let e = &edits[0];
        assert_eq!(e.old_str, "a");
        assert_eq!(e.new_str, "b");
        assert_eq!(e.scope_start, None);
        assert_eq!(e.scope_end, None);
    }

    #[test]
    fn parse_edits_rejects_non_object() {
        let err = parse_edits(&[serde_json::json!("nope")], "f.txt").unwrap_err();
        assert!(err.contains("edit 1 is not an object"), "got: {err}");
    }

    #[test]
    fn parse_edits_requires_old_string() {
        // old_string is required even when a scope is provided.
        let err = parse_edits(
            &[serde_json::json!({"new_string": "b", "start_line": 2, "end_line": 3})],
            "f.txt",
        )
        .unwrap_err();
        assert!(err.contains("missing 'old_string'"), "got: {err}");
    }

    #[test]
    fn parse_edits_requires_new_string() {
        let err = parse_edits(&[serde_json::json!({"old_string": "a"})], "f.txt").unwrap_err();
        assert!(err.contains("missing 'new_string'"), "got: {err}");
    }

    #[test]
    fn parse_edits_scope_start_and_end() {
        let edits = parse_edits(
            &[serde_json::json!({
                "old_string": "a", "new_string": "b",
                "start_line": 2, "end_line": 4
            })],
            "f.txt",
        )
        .unwrap();
        assert_eq!(edits[0].scope_start, Some(2));
        assert_eq!(edits[0].scope_end, Some(4));
    }

    #[test]
    fn parse_edits_start_line_alone_is_valid() {
        let edits = parse_edits(
            &[serde_json::json!({
                "old_string": "a", "new_string": "b", "start_line": 2
            })],
            "f.txt",
        )
        .unwrap();
        assert_eq!(edits[0].scope_start, Some(2));
        assert_eq!(edits[0].scope_end, None);
    }

    #[test]
    fn parse_edits_end_line_without_start_line_defaults_to_one() {
        let edits = parse_edits(
            &[serde_json::json!({
                "old_string": "a", "new_string": "b", "end_line": 3
            })],
            "f.txt",
        )
        .unwrap();
        assert_eq!(edits[0].scope_start, Some(1));
        assert_eq!(edits[0].scope_end, Some(3));
    }

    #[test]
    fn parse_edits_start_after_end_accepted() {
        let edits = parse_edits(
            &[serde_json::json!({
                "old_string": "a", "new_string": "b",
                "start_line": 3, "end_line": 2
            })],
            "f.txt",
        )
        .unwrap();
        assert_eq!(edits[0].scope_start, Some(3));
        assert_eq!(edits[0].scope_end, Some(2));
    }

    #[test]
    fn parse_edits_zero_start_line_clamped() {
        let edits = parse_edits(
            &[serde_json::json!({
                "old_string": "a", "new_string": "b",
                "start_line": 0, "end_line": 2
            })],
            "f.txt",
        )
        .unwrap();
        assert_eq!(edits[0].scope_start, Some(1));
        assert_eq!(edits[0].scope_end, Some(2));
    }

    #[test]
    fn parse_edits_zero_end_line_clamped() {
        let edits = parse_edits(
            &[serde_json::json!({
                "old_string": "a", "new_string": "b",
                "start_line": 1, "end_line": 0
            })],
            "f.txt",
        )
        .unwrap();
        assert_eq!(edits[0].scope_start, Some(1));
        assert_eq!(edits[0].scope_end, Some(1));
    }

    #[test]
    fn apply_match_replaces_unique() {
        let content = "foo bar";
        let edits = vec![EditOp {
            old_str: "bar".into(),
            new_str: "baz".into(),
            scope_start: None,
            scope_end: None,
        }];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "foo baz");
    }

    #[test]
    fn apply_match_unescapes_json_newline_in_old_string() {
        // Read shows raw file bytes, so a JSON5 string like
        // `'Line one\nLine two'` is displayed with a literal backslash-n.
        // A JSON parser turns the LLM's `\n` into a real newline byte,
        // which would not match the file. The unescape retry must convert
        // real newlines back to the literal two-byte sequence before matching.
        let content = "'Line one\\nLine two'\n";
        let edits = vec![EditOp {
            old_str: "'Line one\nLine two'".into(),
            new_str: "'Replaced'".into(),
            scope_start: None,
            scope_end: None,
        }];
        let (out, _, unescape_used) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "'Replaced'\n");
        assert!(unescape_used, "should report unescape was used");
    }

    #[test]
    fn apply_match_unescape_retry_still_reports_not_unique() {
        // The unescape retry must preserve the uniqueness check so a
        // converted needle matching two literal `\n` strings fails loudly.
        let content = "'a\\nb'\n'a\\nb'\n";
        let edits = vec![EditOp {
            old_str: "'a\nb'".into(),
            new_str: "'x'".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(
            err.contains("not unique") && err.contains("after newline unescaping"),
            "got: {err}"
        );
    }

    #[test]
    fn apply_match_keeps_genuine_tab_indentation_with_newline_unescape() {
        // old_string preserves a real tab for indentation while an embedded
        // literal `\n` in the file was misread as a newline. The newline-only
        // retry must leave the indentation alone so the whole needle matches.
        let content = "\t'Line one\\nLine two'\n";
        let edits = vec![EditOp {
            old_str: "\t'Line one\nLine two'".into(),
            new_str: "\t'Replaced'".into(),
            scope_start: None,
            scope_end: None,
        }];
        let (out, _, unescape_used) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "\t'Replaced'\n");
        assert!(unescape_used);
    }

    #[test]
    fn apply_match_does_not_unescape_tabs_or_carriage_returns() {
        // Tab and CR bytes in old_string are preserved as-is: they may be
        // genuine whitespace or line endings, not misinterpreted escapes.
        // The literal-match path (not the retry) finds these needles.
        let tab_content = "\tfoo\n";
        let tab_edits = vec![EditOp {
            old_str: "\tfoo".into(),
            new_str: "bar".into(),
            scope_start: None,
            scope_end: None,
        }];
        let (out, _, unescape_used) = apply_edits(tab_content, &tab_edits, "f.txt").unwrap();
        assert_eq!(out, "bar\n");
        assert!(!unescape_used);

        let cr_content = "'before\rafter'\n";
        let cr_edits = vec![EditOp {
            old_str: "'before\rafter'".into(),
            new_str: "'replaced'".into(),
            scope_start: None,
            scope_end: None,
        }];
        let (out, _, unescape_used) = apply_edits(cr_content, &cr_edits, "f.txt").unwrap();
        assert_eq!(out, "'replaced'\n");
        assert!(!unescape_used);
    }

    #[test]
    fn apply_match_unescape_retry_falls_through_to_fuzzy() {
        // unescape retry must fail (no literal \n in file) so fuzzy must
        // kick in and match via confusable normalization.
        // Use a smart-quote character that the fuzzy path normalizes.
        let content = "\u{201C}hello\u{201D}\n";
        let edits = vec![EditOp {
            old_str: "\"hello\"".into(),
            new_str: "'replaced'".into(),
            scope_start: None,
            scope_end: None,
        }];
        let (out, _, unescape_used) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "'replaced'\n");
        assert!(!unescape_used);
    }

    #[test]
    fn apply_match_unescape_retry_respects_line_scope() {
        // Two lines with literal \n, scoped to line 2 only.
        let content = "'a\\nb'\n'a\\nb'\n";
        let edits = vec![EditOp {
            old_str: "'a\nb'".into(),
            new_str: "'replaced'".into(),
            scope_start: Some(2),
            scope_end: Some(2),
        }];
        let (out, _, unescape_used) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "'a\\nb'\n'replaced'\n");
        assert!(unescape_used);
    }

    #[test]
    fn apply_match_rejects_empty_old_string() {
        let content = "foo";
        let edits = vec![EditOp {
            old_str: String::new(),
            new_str: "x".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("old_string is empty"), "got: {err}");
    }

    #[test]
    fn apply_match_rejects_non_unique_globally() {
        let content = "foo foo";
        let edits = vec![EditOp {
            old_str: "foo".into(),
            new_str: "x".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
    }

    #[test]
    fn apply_match_reports_match_locations_on_not_unique() {
        // "foo" appears on lines 1, 2, 3, 4, 5; the error should list them.
        let content = "foo\nfoo\nfoo\nfoo\nfoo\n";
        let edits = vec![EditOp {
            old_str: "foo".into(),
            new_str: "x".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
        assert!(err.contains("matches at lines 1, 2, 3, 4, 5"), "got: {err}");
    }

    #[test]
    fn apply_match_reports_snippets_on_not_unique() {
        // A bare `}` style needle matches multiple lines; the error should
        // include a trimmed snippet per match so the model can disambiguate
        // without a follow-up Read.
        let content = "fn a() {\n  {}\n}\nfn b() {\n  {}\n}\n";
        let edits = vec![EditOp {
            old_str: "}".into(),
            new_str: "}".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
        assert!(err.contains("line 3:"), "got: {err}");
        assert!(err.contains("line 6:"), "got: {err}");
    }

    #[test]
    fn apply_match_reports_scope_location_on_non_unique() {
        let content = "alpha\nalpha\nalpha\n";
        let edits = vec![EditOp {
            old_str: "alpha".into(),
            new_str: "x".into(),
            scope_start: Some(1),
            scope_end: Some(2),
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
        assert!(
            err.contains("matches within scope at lines 1, 2"),
            "got: {err}"
        );
    }

    #[test]
    fn apply_match_reports_mid_line_match() {
        // "lph" starts mid-line (byte 1), exercising the Err(idx) => idx
        // branch of the binary_search line mapping rather than only exact
        // line-boundary starts.
        let content = "alpha\nalpha\n";
        let edits = vec![EditOp {
            old_str: "lph".into(),
            new_str: "x".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
        assert!(err.contains("matches at lines 1, 2"), "got: {err}");
    }

    #[test]
    fn apply_match_dedups_self_overlapping_line_numbers() {
        // "aa" in "aaa" matches at bytes 0 and 1, both on line 1. The
        // deduplicated diagnostic must list line 1 only once.
        let content = "aaa\n";
        let edits = vec![EditOp {
            old_str: "aa".into(),
            new_str: "x".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
        assert!(err.contains("matches at lines 1"), "got: {err}");
        assert!(!err.contains("1, 1"), "got: {err}");
    }

    #[test]
    fn apply_match_reports_many_matches_with_suffix() {
        // 12 identical lines; only first 10 line numbers listed plus suffix.
        let content = "x\n".repeat(12);
        let edits = vec![EditOp {
            old_str: "x".into(),
            new_str: "y".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(&content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
        assert!(err.contains("and 2 more"), "got: {err}");
    }

    #[test]
    fn apply_match_rejects_missing() {
        let content = "hello";
        let edits = vec![EditOp {
            old_str: "nope".into(),
            new_str: "x".into(),
            scope_start: None,
            scope_end: None,
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("old_string not found"), "got: {err}");
    }

    #[test]
    fn apply_scope_match_succeeds_on_in_scope_line() {
        let content = "line1\nline2\ntarget\nline4\nline5\n";
        let edits = vec![EditOp {
            old_str: "target".into(),
            new_str: "TARGET".into(),
            scope_start: Some(2),
            scope_end: Some(4),
        }];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "line1\nline2\nTARGET\nline4\nline5\n");
    }

    #[test]
    fn apply_scope_match_fails_out_of_scope() {
        // "line5" is on line 5, outside the 2-4 scope.
        let content = "line1\nline2\ntarget\nline4\nline5\n";
        let edits = vec![EditOp {
            old_str: "line5".into(),
            new_str: "LINE5".into(),
            scope_start: Some(2),
            scope_end: Some(4),
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("old_string not found"), "got: {err}");
        assert!(err.contains("between lines 2-4"), "got: {err}");
    }

    #[test]
    fn apply_scope_open_ended_runs_to_eof() {
        // start_line without end_line scopes from that line to EOF.
        let content = "line1\nline2\ntarget\nline4\n";
        let edits = vec![EditOp {
            old_str: "target".into(),
            new_str: "TARGET".into(),
            scope_start: Some(3),
            scope_end: None,
        }];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "line1\nline2\nTARGET\nline4\n");
    }

    #[test]
    fn apply_scope_end_beyond_eof_clamps() {
        let content = "line1\nline2\ntarget\nline4\n";
        let edits = vec![EditOp {
            old_str: "target".into(),
            new_str: "TARGET".into(),
            scope_start: Some(2),
            scope_end: Some(100),
        }];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "line1\nline2\nTARGET\nline4\n");
    }

    #[test]
    fn apply_scope_disambiguates_non_unique_match() {
        // "foo" appears on lines 2 and 3; the scope selects only line 2.
        let content = "x\nfoo\nfoo\n";
        let edits = vec![EditOp {
            old_str: "foo".into(),
            new_str: "BAR".into(),
            scope_start: Some(2),
            scope_end: Some(2),
        }];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "x\nBAR\nfoo\n");
    }

    #[test]
    fn apply_scope_multiline_needle_scoped_to_later_occurrence() {
        // Regression: a multiline needle whose first (out-of-scope) occurrence
        // shadows a later in-scope occurrence when using global match_indices.
        // Scoped byte-slice search finds the correct one.
        let content = "foo\nfoo\nfoo\n";
        let edits = vec![EditOp {
            old_str: "foo\nfoo".into(),
            new_str: "BAR\nBAZ".into(),
            scope_start: Some(2),
            scope_end: Some(3),
        }];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "foo\nBAR\nBAZ\n");
    }

    #[test]
    fn apply_scope_non_unique_within_scope_still_fails() {
        let content = "foo\nfoo\n";
        let edits = vec![EditOp {
            old_str: "foo".into(),
            new_str: "BAR".into(),
            scope_start: Some(1),
            scope_end: Some(2),
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
    }

    #[test]
    fn apply_chained_edits_succeed() {
        // Edit 1 renames "foo" to "baz", edit 2 modifies a call site
        // using the new name. Sequential validation lets edit 2 find
        // "baz" even though it didn't exist in the original content.
        let content = "fn foo() {}\nfn main() { foo(); }\n";
        let edits = vec![
            EditOp {
                old_str: "fn foo()".into(),
                new_str: "fn baz()".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "foo()".into(),
                new_str: "baz()".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "fn baz() {}\nfn main() { baz(); }\n");
    }

    #[test]
    fn apply_chained_edit_not_found_shows_accumulated_buffer() {
        // Edit 1 renames the declaration from "foo" to "baz". Edit 2's
        // needle starts with "fn baz()" — present only in the accumulated
        // buffer, not the original — so the "did you mean" hint fires and the
        // divergence report is built from the post-edit-1 buffer.
        let content = "fn foo() {}\nfn caller() { foo(); }\n";
        let edits = vec![
            EditOp {
                old_str: "fn foo()".into(),
                new_str: "fn baz()".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "fn baz()\n  body".into(),
                new_str: "fn qux()\n  body".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();

        // Should say "not found" because the full multi-line needle does not
        // exist in the accumulated buffer.
        assert!(err.contains("old_string not found"), "got: {err}");

        // The hint fires only against the accumulated buffer ("fn baz"), so
        // its absence in the original content proves the diagnostic is built
        // from the post-prior-edit state — the original "fn foo" would not
        // match. Assert positively on the buffer's distinct content.
        assert!(err.contains("did you mean"), "got: {err}");
        assert!(
            err.contains("fn baz() {}"),
            "hint/divergence should reflect post-edit-1 buffer; got: {err}"
        );
    }

    #[test]
    fn apply_chained_disjoint_edits_still_work() {
        // Disjoint edits on the same file (edit 1 modifies "hello",
        // edit 2 modifies "goodbye" elsewhere in the file).
        let content = "let a = \"hello\";\nlet b = \"goodbye\";\n";
        let edits = vec![
            EditOp {
                old_str: "\"hello\"".into(),
                new_str: "\"hi\"".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "\"goodbye\"".into(),
                new_str: "\"bye\"".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "let a = \"hi\";\nlet b = \"bye\";\n");
    }

    #[test]
    fn apply_edits_with_overlap_fails_not_found() {
        // With sequential validation, edit 1 replaces "234" with "x",
        // changing the content to "01x56789". Then edit 2 looks for "456"
        // which no longer exists in the accumulated buffer.
        let content = "0123456789";
        let edits = vec![
            EditOp {
                old_str: "234".into(),
                new_str: "x".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "456".into(),
                new_str: "y".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("old_string not found"), "got: {err}");
    }

    #[test]
    fn apply_edits_applies_disjoint_matches() {
        let content = "a1b2c";
        let edits = vec![
            EditOp {
                old_str: "1".into(),
                new_str: "ONE".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "2".into(),
                new_str: "TWO".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let (out, _, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "aONEbTWOc");
    }

    #[test]
    fn apply_scope_overlapping_matches_detected() {
        // Regression: match_indices skips by match length, so it can miss
        // overlapping occurrences.  `old_str: "foo\nfoo"` appears at both
        // byte 0 and byte 4 in "foo\nfoo\nfoo\n".  Within scope lines
        // 1-3 both occurrences are present; the uniqueness check must find
        // the second one even though it overlaps the first.
        let content = "foo\nfoo\nfoo\n";
        let edits = vec![EditOp {
            old_str: "foo\nfoo".into(),
            new_str: "BAR\nBAZ".into(),
            scope_start: Some(1),
            scope_end: Some(3),
        }];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
    }

    #[test]
    fn diagnostic_reports_divergence_when_second_line_differs() {
        let content = "alpha\nbeta\n";
        let old_str = "alpha\ngamma";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, None, None);
        assert!(msg.contains("diverges at byte offset 6"), "got: {msg}");
        assert!(msg.contains("expected:"), "got: {msg}");
        assert!(msg.contains("actual:"), "got: {msg}");
        assert!(msg.contains(r#""alpha\ngamma""#), "got: {msg}");
        assert!(msg.contains(r#""alpha\nbeta\n""#), "got: {msg}");
    }

    #[test]
    fn diagnostic_accounts_for_leading_whitespace_on_first_line() {
        // The trimmed probe "alpha" matches at byte 2, but `old_str` carries a
        // two-space indent. The divergence must be reported at the true
        // mismatch on the second line (byte 8), not at the indent.
        let content = "  alpha\nbeta\n";
        let old_str = "  alpha\ngamma";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, None, None);
        assert!(msg.contains("diverges at byte offset 8"), "got: {msg}");
        assert!(msg.contains(r#""  alpha\ngamma""#), "got: {msg}");
        assert!(msg.contains(r#""  alpha\nbeta\n""#), "got: {msg}");
    }

    #[test]
    fn diagnostic_reports_truncation_when_old_is_shorter() {
        let content = "hello world";
        let old_str = "hello";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, None, None);
        assert!(msg.contains("diverges at byte offset 5"), "got: {msg}");
        assert!(msg.contains(r#"expected: "hello""#), "got: {msg}");
        assert!(msg.contains(r#"actual:   "hello world""#), "got: {msg}");
    }

    #[test]
    fn diagnostic_reports_truncation_when_old_is_longer() {
        let content = "hello";
        let old_str = "hello\nworld";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, None, None);
        assert!(msg.contains("diverges at byte offset 5"), "got: {msg}");
        assert!(msg.contains(r#"expected: "hello\nworld""#), "got: {msg}");
        assert!(msg.contains(r#"actual:   "hello""#), "got: {msg}");
    }

    #[test]
    fn diagnostic_divergence_respects_scope_window() {
        // The first line matches inside the scope, but the rest of `old_str`
        // extends past the scope window. The divergence report must stop at the
        // window boundary rather than reading the rest of the file.
        let content = "line1\nline2\nline3\n";
        let old_str = "line2\nline3";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, Some(2), Some(2));
        assert!(msg.contains("between lines 2-2"), "got: {msg}");
        assert!(msg.contains("diverges at byte offset 12"), "got: {msg}");
        assert!(msg.contains(r#"expected: "line2\nline3""#), "got: {msg}");
        assert!(msg.contains(r#"actual:   "line2\n""#), "got: {msg}");
    }

    // --- Scope-stale diagnostic tests ---

    #[test]
    fn diagnostic_scoped_miss_needle_exists_outside_scope() {
        // Needle "target" is at line 5. Scope is lines 1-3. The scoped
        // window search finds nothing, but the full-file fallback should
        // locate it and report a scope-stale hint.
        let content = "alpha\nbeta\ngamma\ndelta\ntarget\nzeta\n";
        let old_str = "target";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, Some(1), Some(3));
        assert!(msg.contains("between lines 1-3"), "got: {msg}");
        assert!(msg.contains("first occurrence at line 5"), "got: {msg}");
        assert!(msg.contains("outside the requested scope"), "got: {msg}");
    }

    #[test]
    fn diagnostic_scoped_miss_needle_nowhere_in_file() {
        // Needle is genuinely absent from the whole file. Neither the
        // scoped window nor the full-file fallback finds it. No
        // divergence report fires (that only triggers when the needle
        // matches but the full old_str diverges). Result is a clean
        // "not found" error with the scope label but no extra hints.
        let content = "alpha\nbeta\ngamma\n";
        let old_str = "nowhere";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, Some(1), Some(3));
        assert!(msg.contains("between lines 1-3"), "got: {msg}");
        assert!(!msg.contains("outside the requested scope"), "got: {msg}");
        assert!(!msg.contains("first occurrence"), "got: {msg}");
        assert!(!msg.contains("did you mean"), "got: {msg}");
    }

    #[test]
    fn diagnostic_unscoped_still_works() {
        // No scope at all — the scoped search and full-file fallback are
        // both skipped, and the existing "did you mean" hint fires.
        let content = "alpha\nbeta\ngamma\n";
        let old_str = "beta";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, None, None);
        assert!(!msg.contains("between lines"), "got: {msg}");
        assert!(!msg.contains("outside the requested scope"), "got: {msg}");
        assert!(msg.contains("did you mean"), "got: {msg}");
    }

    #[test]
    fn diagnostic_scoped_miss_needle_match_but_full_old_str_diverges() {
        // Regression: the trimmed first-line needle "alpha" matches at
        // line 2, but the complete old_str "alpha\ngamma" does NOT match
        // at that position (the content has "alpha\nbeta"). The scope-stale
        // hint must NOT fire because retrying without the scope would still
        // fail — this is a genuine content mismatch, not a stale scope.
        let content = "header\nalpha\nbeta\n";
        let old_str = "alpha\ngamma";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, Some(1), Some(1));
        assert!(msg.contains("between lines 1-1"), "got: {msg}");
        assert!(!msg.contains("outside the requested scope"), "got: {msg}");
        assert!(!msg.contains("first occurrence"), "got: {msg}");
    }

    #[test]
    fn diagnostic_scoped_miss_mid_line_match_reports_containing_line() {
        // Regression: when the trimmed needle ("target") starts mid-line,
        // the full-file fallback must report the containing line via
        // line_number_at, not the naive .lines().count() + 1 which counts
        // a partial prefix as a full line.
        // Content: "alpha\n  target\nzeta\n"
        // "target" at byte 8 starts on line 2. The old bug counted
        // content[..8].lines() = ["alpha", "  "] → 2, then +1 = 3 (wrong).
        let content = "alpha\n  target\nzeta\n";
        let old_str = "target";
        let msg = edit_not_found_diagnostic(content, "f.txt", old_str, 1, Some(1), Some(1));
        assert!(msg.contains("between lines 1-1"), "got: {msg}");
        assert!(msg.contains("first occurrence at line 2"), "got: {msg}");
        assert!(msg.contains("outside the requested scope"), "got: {msg}");
    }

    // --- Batch-order diagnostic tests ---

    #[test]
    fn apply_edits_batch_order_error_when_prior_edit_destroys_text() {
        // Edit 1 replaces "234" with "x". Edit 2 looks for "456" which
        // no longer exists in the accumulated buffer but does exist in
        // the *original* content — this is a batch-order problem.
        let content = "0123456789";
        let edits = vec![
            EditOp {
                old_str: "234".into(),
                new_str: "x".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "456".into(),
                new_str: "y".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(
            err.contains("old_string not found in file after prior edits"),
            "got: {err}"
        );
        assert!(
            err.contains("matches the original file content"),
            "got: {err}"
        );
        assert!(
            err.contains("try splitting this into a separate Edit call"),
            "got: {err}"
        );
        assert!(
            err.contains("verify your edits don't target overlapping regions"),
            "got: {err}"
        );
    }

    #[test]
    fn apply_edits_batch_edit_not_found_in_either_buffer() {
        // Edit 1 succeeds. Edit 2's old_str is absent from both the
        // accumulated buffer AND the original — a genuine wrong-text
        // error, not a batch-order problem.
        let content = "hello world\nfoo bar\n";
        let edits = vec![
            EditOp {
                old_str: "hello".into(),
                new_str: "hi".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "nonexistent".into(),
                new_str: "replacement".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(err.contains("old_string not found"), "got: {err}");
        assert!(
            !err.contains("matches the original file content"),
            "batch-order message should not fire when old_str is absent from original too; got: {err}"
        );
    }

    #[test]
    fn apply_edits_scoped_batch_needle_in_buffer_outside_scope() {
        // Edit 1 adds a line above, shifting content down. Edit 2 has a
        // scope that was correct for the *original* file but is now stale
        // because edit 1 shifted lines. The needle still exists in the
        // buffer — just outside the stale scope window. The batch-order
        // message must NOT fire; instead the scope-stale diagnostic from
        // edit_not_found_diagnostic should produce the "outside the
        // requested scope" hint.
        let content = "line1\nline2\ntarget\nline4\n";
        let edits = vec![
            EditOp {
                old_str: "line1".into(),
                new_str: "line0\nline1".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "target".into(),
                new_str: "TARGET".into(),
                scope_start: Some(3),
                scope_end: Some(3),
            },
        ];
        let err = apply_edits(content, &edits, "f.txt").unwrap_err();
        assert!(
            !err.contains("matches the original file content"),
            "batch-order message should NOT fire when needle is still in buffer; got: {err}"
        );
        assert!(
            err.contains("outside the requested scope"),
            "scope-stale diagnostic should fire; got: {err}"
        );
    }

    // --- Success-report tests ---

    #[test]
    fn apply_edits_reports_line_range_for_single_edit() {
        let content = "one\ntwo\nthree\nfour\nfive\n";
        let edits = vec![EditOp {
            old_str: "three".into(),
            new_str: "THREE".into(),
            scope_start: None,
            scope_end: None,
        }];
        let (out, ranges, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "one\ntwo\nTHREE\nfour\nfive\n");
        assert_eq!(ranges, vec![(3, 3)]);
    }

    #[test]
    fn render_success_includes_context_around_single_edit() {
        let content = "one\ntwo\nTHREE\nfour\nfive\n";
        let report = render_success(content, &[(3, 3)], "f.txt", 5, 5, None, false);
        assert!(
            report.starts_with("Applied 1 edit to f.txt. (5 lines, +0 vs original)\n"),
            "got: {report}"
        );
        assert!(report.contains("  1: one"), "got: {report}");
        assert!(report.contains("  3: THREE"), "got: {report}");
        assert!(report.contains("  5: five"), "got: {report}");
    }

    #[test]
    fn apply_edits_reports_ranges_against_accumulated_buffer() {
        // The first replacement adds a line, so the second edit's true final
        // position shifts down relative to the original content.
        let content = "a\nb\nc\nd\n";
        let edits = vec![
            EditOp {
                old_str: "b\nc".into(),
                new_str: "B\nC\nC2".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "d".into(),
                new_str: "D".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let (out, ranges, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "a\nB\nC\nC2\nD\n");
        assert_eq!(ranges, vec![(2, 4), (5, 5)]);
    }

    #[test]
    fn render_success_coalesces_adjacent_windows() {
        let content = "a\nB\nC\nd\n";
        let report = render_success(content, &[(2, 2), (3, 3)], "f.txt", 4, 4, None, false);
        assert!(
            report.starts_with("Applied 2 edits to f.txt. (4 lines, +0 vs original)\n"),
            "got: {report}"
        );
        assert_eq!(report.matches("  2: B").count(), 1, "got: {report}");
        assert_eq!(report.matches("  3: C").count(), 1, "got: {report}");
        assert!(
            !report.contains("--"),
            "adjacent edits should not repeat context; got: {report}"
        );
    }

    #[test]
    fn render_success_coalesces_out_of_order_windows() {
        let content = "one\ntwo\nthree\nfour\nfive\n";
        let report = render_success(content, &[(5, 5), (1, 1)], "f.txt", 5, 5, None, false);
        assert!(
            report.starts_with("Applied 2 edits to f.txt. (5 lines, +0 vs original)\n"),
            "got: {report}"
        );
        assert_eq!(report.matches("  1: one").count(), 1, "got: {report}");
        assert_eq!(report.matches("  5: five").count(), 1, "got: {report}");
        assert!(!report.contains("--"), "got: {report}");
    }

    #[test]
    fn render_success_preserves_trailing_empty_line() {
        let content = "one\ntwo\n";
        let report = render_success(content, &[(2, 2)], "f.txt", 2, 2, None, false);
        assert!(report.contains("  3: "), "got: {report}");
    }

    #[test]
    fn apply_edits_reports_ranges_when_later_edit_shifts_earlier_lines() {
        // Edit 1 replaces a later line; edit 2 inserts a line before it, so
        // edit 1's true final position shifts down relative to its position at
        // application time.
        let content = "a\nb\nc\n";
        let edits = vec![
            EditOp {
                old_str: "c".into(),
                new_str: "C".into(),
                scope_start: None,
                scope_end: None,
            },
            EditOp {
                old_str: "a".into(),
                new_str: "A\nA2".into(),
                scope_start: None,
                scope_end: None,
            },
        ];
        let (out, ranges, _) = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "A\nA2\nb\nC\n");
        assert_eq!(ranges, vec![(4, 4), (1, 2)]);
    }

    #[test]
    fn render_success_caps_oversized_window() {
        let content: String = (1..=40)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let report = render_success(&content, &[(1, 40)], "f.txt", 40, 40, None, false);
        assert!(
            report.starts_with("Applied 1 edit to f.txt. (40 lines, +0 vs original)\n"),
            "got: {report}"
        );
        assert!(report.contains("  1: line1"), "got: {report}");
        assert!(report.contains("  6: line6"), "got: {report}");
        assert!(!report.contains("  7: line7"), "got: {report}");
        assert!(report.contains("28 lines"), "got: {report}");
    }

    #[test]
    fn render_success_caps_expanded_blocks() {
        // Eight disjoint ranges collapse to eight context blocks; only the
        // first six are expanded, with a trailing count for the remainder.
        let content: String = (1..=70)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let ranges = [
            (1, 1),
            (10, 10),
            (20, 20),
            (30, 30),
            (40, 40),
            (50, 50),
            (60, 60),
            (70, 70),
        ];
        let report = render_success(&content, &ranges, "f.txt", 70, 70, None, false);
        assert!(
            report.starts_with("Applied 8 edits to f.txt. (70 lines, +0 vs original)\n"),
            "got: {report}"
        );
        assert!(
            report.contains("(2 more context blocks not shown)"),
            "got: {report}"
        );
        assert!(
            !report.contains("  70:"),
            "last block should be capped; got: {report}"
        );
    }

    #[test]
    fn render_success_keeps_each_edit_inside_a_coalesced_oversized_window() {
        // Three disjoint one-line edits at lines 10, 17, and 24 produce
        // context windows 7-13, 14-20, and 21-27. Those touch and therefore
        // coalesce into a single 21-line window, but capping must be done per
        // edit *before* deduplication — otherwise the oversized-window
        // truncation keeps only lines 7-12 and 22-27 and silently drops the
        // replacement at line 17.
        let content: String = (1..=30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let report = render_success(
            &content,
            &[(10, 10), (17, 17), (24, 24)],
            "f.txt",
            30,
            30,
            None,
            false,
        );
        assert!(report.contains("  10: line10"), "got: {report}");
        assert!(report.contains("  17: line17"), "got: {report}");
        assert!(report.contains("  24: line24"), "got: {report}");
        assert!(!report.contains("... skipped"), "got: {report}");
    }
}
