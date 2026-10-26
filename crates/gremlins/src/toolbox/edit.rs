use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::tool_specs::describe_edit_provided_fields;
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

fn edit_not_found_diagnostic(
    content: &str,
    file_path: &str,
    old_str: &str,
    edit_num: usize,
) -> String {
    let first_line = old_str.lines().next().unwrap_or("").trim();
    let needle: &str = if first_line.is_empty() {
        return format!(
            "Error: old_string not found in {file_path} (edit {edit_num}) — first line is empty or whitespace-only"
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
    let hint = if let Some(pos) = content.find(needle) {
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
        format!(" — did you mean to match near line {line_no}? Found:\n{context}")
    } else {
        String::new()
    };
    format!(
        "Error: old_string not found in {file_path} (edit {edit_num}) — first line: {needle:?}{hint}"
    )
}

#[derive(Debug)]
enum EditOp {
    Match {
        old_str: String,
        new_str: String,
    },
    Range {
        start: usize,
        end: usize,
        replacement: String,
    },
}

fn parse_edits(edits_arr: &[serde_json::Value], file_path: &str) -> Result<Vec<EditOp>, String> {
    // Collect parsed edits with validation.
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
        let has_start = obj.contains_key("start_line");
        // If both modes are present, prefer range mode.
        if has_start {
            // --- Range edit (may also include ignored match-mode fields) ---
            if !obj.contains_key("end_line") {
                let provided = describe_edit_provided_fields(obj);
                return Err(format!(
                    "Error: edit {} missing 'end_line' for Edit tool (range mode){} in {file_path}",
                    i + 1,
                    provided
                ));
            }
            if !obj.contains_key("replacement") {
                let provided = describe_edit_provided_fields(obj);
                return Err(format!(
                    "Error: edit {} missing 'replacement' for Edit tool (range mode){} in {file_path}",
                    i + 1, provided
                ));
            }
            let start_line = match obj.get("start_line").and_then(|v| v.as_u64()) {
                Some(n) if n > 0 => n as usize,
                Some(_) => {
                    return Err(format!(
                        "Error: edit {} 'start_line' must be a positive integer in {file_path}",
                        i + 1
                    ))
                }
                None => {
                    return Err(format!(
                        "Error: edit {} 'start_line' must be an integer in {file_path}",
                        i + 1
                    ))
                }
            };
            let end_line = match obj.get("end_line").and_then(|v| v.as_u64()) {
                Some(n) if n > 0 => n as usize,
                Some(_) => {
                    return Err(format!(
                        "Error: edit {} 'end_line' must be a positive integer in {file_path}",
                        i + 1
                    ))
                }
                None => {
                    return Err(format!(
                        "Error: edit {} 'end_line' must be an integer in {file_path}",
                        i + 1
                    ))
                }
            };
            if start_line > end_line {
                return Err(format!(
                    "Error: edit {} 'start_line' ({start_line}) > 'end_line' ({end_line}) in {file_path}",
                    i + 1
                ));
            }
            let replacement = match obj.get("replacement").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return Err(format!(
                        "Error: edit {} missing 'replacement' in {file_path}",
                        i + 1
                    ))
                }
            };
            edits.push(EditOp::Range {
                start: start_line,
                end: end_line,
                replacement,
            });
        } else {
            // --- Match edit ---
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
            edits.push(EditOp::Match { old_str, new_str });
        }
    }
    Ok(edits)
}

fn apply_edits(
    content: &str,
    edits: &[EditOp],
    file_path: &str,
    line_count: usize,
    line_offsets: &[usize],
) -> Result<String, String> {
    // Validate match edits against original content.
    for (i, edit) in edits.iter().enumerate() {
        if let EditOp::Match { old_str, .. } = edit {
            if old_str.is_empty() {
                return Err(format!(
                    "Error: old_string is empty in {file_path} (edit {})",
                    i + 1
                ));
            }
            if !content.contains(old_str.as_str()) {
                return Err(edit_not_found_diagnostic(
                    content,
                    file_path,
                    old_str,
                    i + 1,
                ));
            }
            if content.matches(old_str.as_str()).count() > 1 {
                return Err(format!(
                    "Error: old_string is not unique in {file_path} (edit {})",
                    i + 1
                ));
            }
        }
    }

    let eof = content.len();

    // Validate range edits against file line count.
    for (i, edit) in edits.iter().enumerate() {
        if let EditOp::Range { start, .. } = edit {
            if *start > line_count {
                return Err(format!(
                    "Error: edit {} 'start_line' ({}) exceeds file line count ({}) in {file_path}",
                    i + 1,
                    start,
                    line_count
                ));
            }
        }
    }

    // Compute byte ranges.
    let ranges: Vec<(usize, usize)> = edits
        .iter()
        .map(|edit| match edit {
            EditOp::Match { old_str, .. } => {
                let pos = content.find(old_str.as_str()).unwrap();
                (pos, pos + old_str.len())
            }
            EditOp::Range { start, end, .. } => {
                let start_byte = line_offsets[*start - 1];
                let end_byte = if *end < line_offsets.len() {
                    line_offsets[*end]
                } else {
                    // end_line beyond last line — clamp to EOF (replace through end of file)
                    eof
                };
                (start_byte, end_byte)
            }
        })
        .collect();

    // Validate non-overlapping.
    for i in 0..ranges.len() {
        for j in (i + 1)..ranges.len() {
            if ranges[i].0 < ranges[j].1 && ranges[j].0 < ranges[i].1 {
                return Err(format!(
                    "Error: edits {} and {} overlap in {file_path}",
                    i + 1,
                    j + 1
                ));
            }
        }
    }

    // Apply in reverse byte-order.
    let mut indexed: Vec<(usize, usize, &str)> = edits
        .iter()
        .zip(ranges.iter())
        .map(|(edit, &(start, end))| {
            let replacement: &str = match edit {
                EditOp::Match { new_str, .. } => new_str,
                EditOp::Range { replacement, .. } => replacement,
            };
            (start, end, replacement)
        })
        .collect();
    indexed.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut updated = content.to_string();
    for (start, end, replacement) in &indexed {
        updated.replace_range(*start..*end, replacement);
    }

    Ok(updated)
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

    // Precompute line offsets.
    let line_offsets: Vec<usize> = {
        let mut offs = vec![0usize];
        for (idx, _) in content.char_indices().filter(|(_, c)| *c == '\n') {
            offs.push(idx + 1);
        }
        offs
    };
    let line_count = line_offsets
        .len()
        .saturating_sub(if content.ends_with('\n') { 1 } else { 0 });

    let updated = match apply_edits(&content, &edits, file_path, line_count, &line_offsets) {
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

    /// Replicates the line-offset table `edit_sync` builds before applying edits.
    fn line_offsets(content: &str) -> Vec<usize> {
        let mut offs = vec![0usize];
        for (idx, _) in content.char_indices().filter(|(_, c)| *c == '\n') {
            offs.push(idx + 1);
        }
        offs
    }

    /// Replicates the line-count computation `edit_sync` passes to `apply_edits`.
    fn line_count(content: &str, offs: &[usize]) -> usize {
        offs.len()
            .saturating_sub(if content.ends_with('\n') { 1 } else { 0 })
    }

    fn offsets_and_count(content: &str) -> (Vec<usize>, usize) {
        let offs = line_offsets(content);
        let count = line_count(content, &offs);
        (offs, count)
    }

    #[test]
    fn parse_edits_match_mode() {
        let edits = parse_edits(
            &[serde_json::json!({"old_string": "a", "new_string": "b"})],
            "f.txt",
        )
        .unwrap();
        assert!(matches!(
            &edits[0],
            EditOp::Match { old_str, new_str } if old_str == "a" && new_str == "b"
        ));
    }

    #[test]
    fn parse_edits_prefers_range_mode_when_start_line_present() {
        let edits = parse_edits(
            &[serde_json::json!({
                "old_string": "a", "new_string": "z",
                "start_line": 2, "end_line": 3, "replacement": "x"
            })],
            "f.txt",
        )
        .unwrap();
        assert!(matches!(
            &edits[0],
            EditOp::Range { start: 2, end: 3, replacement } if replacement == "x"
        ));
    }

    #[test]
    fn parse_edits_rejects_non_object() {
        let err = parse_edits(&[serde_json::json!("nope")], "f.txt").unwrap_err();
        assert!(err.contains("edit 1 is not an object"), "got: {err}");
    }

    #[test]
    fn parse_edits_range_requires_end_line() {
        let err = parse_edits(
            &[serde_json::json!({"start_line": 1, "replacement": "x"})],
            "f.txt",
        )
        .unwrap_err();
        assert!(err.contains("missing 'end_line'"), "got: {err}");
    }

    #[test]
    fn parse_edits_range_requires_replacement() {
        let err = parse_edits(
            &[serde_json::json!({"start_line": 1, "end_line": 2})],
            "f.txt",
        )
        .unwrap_err();
        assert!(err.contains("missing 'replacement'"), "got: {err}");
    }

    #[test]
    fn parse_edits_range_rejects_zero_start_line() {
        let err = parse_edits(
            &[serde_json::json!({"start_line": 0, "end_line": 2, "replacement": "x"})],
            "f.txt",
        )
        .unwrap_err();
        assert!(err.contains("positive integer"), "got: {err}");
    }

    #[test]
    fn parse_edits_range_rejects_start_after_end() {
        let err = parse_edits(
            &[serde_json::json!({"start_line": 3, "end_line": 2, "replacement": "x"})],
            "f.txt",
        )
        .unwrap_err();
        assert!(err.contains("> 'end_line'"), "got: {err}");
    }

    #[test]
    fn parse_edits_match_requires_old_string() {
        let err = parse_edits(&[serde_json::json!({"new_string": "b"})], "f.txt").unwrap_err();
        assert!(err.contains("missing 'old_string'"), "got: {err}");
    }

    #[test]
    fn parse_edits_match_requires_new_string() {
        let err = parse_edits(&[serde_json::json!({"old_string": "a"})], "f.txt").unwrap_err();
        assert!(err.contains("missing 'new_string'"), "got: {err}");
    }

    #[test]
    fn apply_match_replaces_unique() {
        let content = "foo bar";
        let edits = vec![EditOp::Match {
            old_str: "bar".into(),
            new_str: "baz".into(),
        }];
        let (offs, count) = offsets_and_count(content);
        let out = apply_edits(content, &edits, "f.txt", count, &offs).unwrap();
        assert_eq!(out, "foo baz");
    }

    #[test]
    fn apply_match_rejects_empty_old_string() {
        let content = "foo";
        let edits = vec![EditOp::Match {
            old_str: String::new(),
            new_str: "x".into(),
        }];
        let (offs, count) = offsets_and_count(content);
        let err = apply_edits(content, &edits, "f.txt", count, &offs).unwrap_err();
        assert!(err.contains("old_string is empty"), "got: {err}");
    }

    #[test]
    fn apply_match_rejects_non_unique() {
        let content = "foo foo";
        let edits = vec![EditOp::Match {
            old_str: "foo".into(),
            new_str: "x".into(),
        }];
        let (offs, count) = offsets_and_count(content);
        let err = apply_edits(content, &edits, "f.txt", count, &offs).unwrap_err();
        assert!(err.contains("not unique"), "got: {err}");
    }

    #[test]
    fn apply_match_rejects_missing() {
        let content = "hello";
        let edits = vec![EditOp::Match {
            old_str: "nope".into(),
            new_str: "x".into(),
        }];
        let (offs, count) = offsets_and_count(content);
        let err = apply_edits(content, &edits, "f.txt", count, &offs).unwrap_err();
        assert!(err.contains("old_string not found"), "got: {err}");
    }

    #[test]
    fn apply_range_replaces_inclusive_lines() {
        let content = "a\nb\nc\n";
        let edits = vec![EditOp::Range {
            start: 2,
            end: 2,
            replacement: "B".into(),
        }];
        let (offs, count) = offsets_and_count(content);
        let out = apply_edits(content, &edits, "f.txt", count, &offs).unwrap();
        // Range mode replaces the whole line including its trailing newline.
        assert_eq!(out, "a\nBc\n");
    }

    #[test]
    fn apply_range_clamps_end_beyond_eof() {
        let content = "a\nb\nc\n";
        let edits = vec![EditOp::Range {
            start: 2,
            end: 100,
            replacement: "B".into(),
        }];
        let (offs, count) = offsets_and_count(content);
        let out = apply_edits(content, &edits, "f.txt", count, &offs).unwrap();
        assert_eq!(out, "a\nB");
    }

    #[test]
    fn apply_range_rejects_start_past_eof() {
        let content = "a\nb\n";
        let edits = vec![EditOp::Range {
            start: 5,
            end: 6,
            replacement: "x".into(),
        }];
        let (offs, count) = offsets_and_count(content);
        let err = apply_edits(content, &edits, "f.txt", count, &offs).unwrap_err();
        assert!(err.contains("exceeds file line count"), "got: {err}");
    }

    #[test]
    fn apply_edits_rejects_overlap() {
        let content = "0123456789";
        let edits = vec![
            EditOp::Match {
                old_str: "234".into(),
                new_str: "x".into(),
            },
            EditOp::Match {
                old_str: "456".into(),
                new_str: "y".into(),
            },
        ];
        let (offs, count) = offsets_and_count(content);
        let err = apply_edits(content, &edits, "f.txt", count, &offs).unwrap_err();
        assert!(err.contains("overlap"), "got: {err}");
    }

    #[test]
    fn apply_edits_applies_disjoint_matches_in_reverse_order() {
        let content = "a1b2c";
        let edits = vec![
            EditOp::Match {
                old_str: "1".into(),
                new_str: "ONE".into(),
            },
            EditOp::Match {
                old_str: "2".into(),
                new_str: "TWO".into(),
            },
        ];
        let (offs, count) = offsets_and_count(content);
        let out = apply_edits(content, &edits, "f.txt", count, &offs).unwrap();
        assert_eq!(out, "aONEbTWOc");
    }
}
