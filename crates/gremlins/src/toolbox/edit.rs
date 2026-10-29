use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::tools::{
    blocking_string, io_enforce, parse_args, req_str, resolve, AgentLoopRuntime, ToolHandler,
};

pub(crate) struct EditTool;

#[async_trait]
impl ToolHandler for EditTool {
    async fn invoke(&self, _id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        edit_invoke(ctx, args_json).await
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

    let hint = if let Some(rel) = content[window_start..window_end].find(needle) {
        let pos = window_start + rel;
        let line_no = content[..pos].lines().count() + 1;
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
        format!(" — did you mean to match near line {line_no}? Found:\n{context}\n{divergence}")
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

fn apply_edits(content: &str, edits: &[EditOp], file_path: &str) -> Result<String, String> {
    let mut buffer = content.to_string();

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

        // Collect ALL match positions using find + offset-1 advancement
        // so overlapping occurrences (e.g. `old_str: "foo\nfoo"` in
        // `"foo\nfoo\nfoo\n"`) are detected.
        let mut match_positions: Vec<usize> = Vec::new();
        let mut search_from = 0;
        while let Some(pos) = search_window[search_from..].find(old_str) {
            let abs_pos = search_from + pos + window_offset;
            match_positions.push(abs_pos);
            search_from += pos + 1; // advance by 1 to detect overlapping matches
        }

        if match_positions.is_empty() {
            return Err(edit_not_found_diagnostic(
                &buffer,
                file_path,
                old_str,
                i + 1,
                edit.scope_start,
                edit.scope_end,
            ));
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
                let end = line_offsets
                    .get(line_no)
                    .copied()
                    .unwrap_or(buffer.len());
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
        let end = start + old_str.len();
        buffer.replace_range(start..end, &edit.new_str);
    }

    Ok(buffer)
}

pub(crate) fn edit_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
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
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };

    let edits = match parse_edits(edits_arr, file_path) {
        Ok(e) => e,
        Err(e) => return e,
    };

    let updated = match apply_edits(&content, &edits, file_path) {
        Ok(u) => u,
        Err(e) => return e,
    };

    if let Err(e) = std::fs::write(&path, updated) {
        return format!("Error: {e}");
    }
    "OK".into()
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
        assert_eq!(out, "foo baz");
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
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
        let out = apply_edits(content, &edits, "f.txt").unwrap();
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
}
