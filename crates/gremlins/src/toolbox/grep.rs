use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::Regex;

use crate::clients::tools::{blocking_string, parse_args, req_str, AgentLoopRuntime, ToolHandler};
use crate::core::paths::{io_enforce, resolve};

const GREP_MAX_LINES: usize = 2000;
const SKIP_DIRS: &[&str] = &["__pycache__", "node_modules", "target"];

pub(crate) struct GrepTool;

#[async_trait]
impl ToolHandler for GrepTool {
    async fn invoke(&self, _id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        grep_invoke(ctx, args_json).await
    }
}

fn is_binary_prefix(path: &Path) -> bool {
    match std::fs::File::open(path) {
        Ok(mut f) => {
            use std::io::Read;
            let mut buf = [0u8; 8192];
            match f.read(&mut buf) {
                Ok(n) => buf[..n].contains(&0),
                Err(_) => true,
            }
        }
        Err(_) => true,
    }
}

fn fnmatch_name(name: &str, pat: &str) -> bool {
    glob::Pattern::new(pat)
        .map(|p| p.matches(name))
        .unwrap_or(false)
}

#[derive(Clone, Copy)]
struct GrepOptions {
    max_matches: usize,
    context_before: usize,
    context_after: usize,
}

/// Accumulates match/context output across the files visited by one grep
/// invocation. Keeping the mutable scan state together avoids threading four
/// separate `&mut` references through the recursive walk.
struct GrepCollector {
    matches: Vec<String>,
    match_count: usize,
    blocked: bool,
    truncated: bool,
}

/// Shared per-file matcher. Both the single-file and directory-walk grep
/// paths call this so match formatting, context expansion, and truncation
/// semantics cannot drift between the two.
fn append_file_matches(
    path: &Path,
    content: &str,
    pattern: &Regex,
    options: &GrepOptions,
    out: &mut GrepCollector,
) {
    // Zero context keeps the exact legacy format: one `path:line:content`
    // entry per match and no block separators.
    if options.context_before == 0 && options.context_after == 0 {
        for (idx, line) in content.lines().enumerate() {
            if pattern.is_match(line) {
                if out.match_count >= options.max_matches {
                    out.truncated = true;
                    return;
                }
                out.matches
                    .push(format!("{}:{}:{line}", path.display(), idx + 1));
                out.match_count += 1;
            }
        }
        return;
    }

    let lines: Vec<&str> = content.lines().collect();
    let last_line = lines.len();

    let mut match_lines = Vec::new();
    let mut first_excluded: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if pattern.is_match(line) {
            if out.match_count >= options.max_matches {
                out.truncated = true;
                first_excluded = Some(idx + 1);
                break;
            }
            match_lines.push(idx + 1);
            out.match_count += 1;
        }
    }
    if match_lines.is_empty() {
        return;
    }

    // Expand each match into a 1-based inclusive [start, end] window and
    // merge windows that touch or overlap. A match past `max_matches` that
    // would otherwise fall inside an admitted window is never emitted:
    // truncation caps the window before that line, so it is not mislabeled
    // as context.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for &line_no in &match_lines {
        let start = line_no.saturating_sub(options.context_before).max(1);
        let mut end = line_no.saturating_add(options.context_after).min(last_line);
        if let Some(excluded) = first_excluded {
            end = end.min(excluded - 1);
        }
        match windows.last_mut() {
            Some((_, last_end)) if start <= last_end.saturating_add(1) => {
                *last_end = (*last_end).max(end);
            }
            _ => windows.push((start, end)),
        }
    }

    // Emit each merged window as one context block. Context lines use `-`
    // where match lines use `:`; non-contiguous blocks are separated by `--`.
    // Since windows are capped to exclude any match past `max_matches`, every
    // emitted line that matches the pattern is a genuine admitted match.
    for (block_idx, &(start, end)) in windows.iter().enumerate() {
        if out.blocked || block_idx > 0 {
            out.matches.push("--".to_string());
        }
        out.blocked = true;
        for line_no in start..=end {
            let line = lines[line_no - 1];
            let separator = if pattern.is_match(line) { ':' } else { '-' };
            out.matches.push(format!(
                "{}{}{}{}{}",
                path.display(),
                separator,
                line_no,
                separator,
                line
            ));
        }
    }
}

fn scan_file(
    path: &Path,
    roots: Option<&[PathBuf]>,
    pattern: &Regex,
    options: &GrepOptions,
    out: &mut GrepCollector,
) {
    // Skip files that resolve outside any allowed root (e.g. via a symlink).
    // `None` roots means containment is disabled (AllowAll).
    if let Some(roots) = roots {
        if io_enforce(path, roots).is_some() {
            return;
        }
    }
    if is_binary_prefix(path) {
        return;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    append_file_matches(path, &content, pattern, options, out);
}

fn walk_grep(
    dir: &Path,
    roots: Option<&[PathBuf]>,
    pattern: &Regex,
    glob_filter: Option<&str>,
    options: &GrepOptions,
    out: &mut GrepCollector,
) {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in entries {
        let name = e.file_name();
        let name_str = name.to_string_lossy();
        let ft = match e.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            dirs.push(e.path());
        } else if ft.is_file() || ft.is_symlink() {
            files.push((e.path(), name_str.into_owned()));
        }
    }
    for (path, name) in files {
        if out.truncated {
            return;
        }
        if let Some(g) = glob_filter {
            if !fnmatch_name(&name, g) {
                continue;
            }
        }
        scan_file(&path, roots, pattern, options, out);
    }
    for d in dirs {
        if out.truncated {
            return;
        }
        // Skip directory symlinks that resolve outside any allowed root.
        if let Some(roots) = roots {
            if io_enforce(&d, roots).is_some() {
                continue;
            }
        }
        walk_grep(&d, roots, pattern, glob_filter, options, out);
    }
}

pub(crate) async fn grep_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || grep_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn grep_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pat = match req_str(&args, "pattern") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let pattern = match Regex::new(pat) {
        Ok(r) => r,
        Err(e) => return format!("Error: invalid regex: {e}"),
    };
    let search_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let base = resolve(search_path, cwd);
    if !base.exists() {
        return format!("Error: path does not exist: {}", base.display());
    }
    let glob_filter = args
        .get("glob")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let options = GrepOptions {
        max_matches: args
            .get("max_matches")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(GREP_MAX_LINES),
        context_before: args
            .get("context_before")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        context_after: args
            .get("context_after")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
    };
    let mut out = GrepCollector {
        matches: Vec::new(),
        match_count: 0,
        blocked: false,
        truncated: false,
    };
    if base.is_file() {
        let name = base.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(roots) = roots {
            if let Some(err) = io_enforce(&base, roots) {
                return err;
            }
        }
        if glob_filter.is_none_or(|g| fnmatch_name(name, g)) {
            if is_binary_prefix(&base) {
                return "(no matches)".into();
            }
            let content = match std::fs::read_to_string(&base) {
                Ok(content) => content,
                Err(e) => return format!("Error: {e}"),
            };
            append_file_matches(&base, &content, &pattern, &options, &mut out);
        }
    } else {
        walk_grep(&base, roots, &pattern, glob_filter, &options, &mut out);
    }
    if out.matches.is_empty() {
        return "(no matches)".into();
    }
    let mut result = out.matches.join("\n");
    if out.truncated {
        result.push_str(&format!("\n[truncated at {} matches]", options.max_matches));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(context_before: usize, context_after: usize, max_matches: usize) -> GrepOptions {
        GrepOptions {
            max_matches,
            context_before,
            context_after,
        }
    }

    fn collect(content: &str, pattern: &str, opts: GrepOptions) -> GrepCollector {
        let re = regex::Regex::new(pattern).unwrap();
        let mut out = GrepCollector {
            matches: Vec::new(),
            match_count: 0,
            blocked: false,
            truncated: false,
        };
        append_file_matches(std::path::Path::new("f.txt"), content, &re, &opts, &mut out);
        out
    }

    #[test]
    fn zero_context_emits_legacy_path_line_content() {
        let out = collect("a\nb\nc\n", "b", options(0, 0, 2000));
        assert_eq!(out.matches.join("\n"), "f.txt:2:b");
    }

    #[test]
    fn zero_context_emits_every_match() {
        let out = collect("b\nb\n", "b", options(0, 0, 2000));
        assert_eq!(out.matches.join("\n"), "f.txt:1:b\nf.txt:2:b");
    }

    #[test]
    fn zero_context_truncates_at_max_matches() {
        let body: String = (1..=5).map(|i| format!("hit {i}\n")).collect();
        let out = collect(&body, "hit", options(0, 0, 3));
        assert_eq!(
            out.matches.join("\n"),
            "f.txt:1:hit 1\nf.txt:2:hit 2\nf.txt:3:hit 3"
        );
        assert!(out.truncated, "truncation must be flagged");
    }

    #[test]
    fn context_expands_with_line_numbers_and_clips() {
        let out = collect(
            "lead\nzero\nalpha\nbeta\ngamma\n",
            "alpha",
            options(5, 1, 2000),
        );
        assert_eq!(
            out.matches.join("\n"),
            "f.txt-1-lead\nf.txt-2-zero\nf.txt:3:alpha\nf.txt-4-beta"
        );
    }

    #[test]
    fn context_separates_non_contiguous_blocks() {
        let out = collect("a\nb\nc\nd\ne\nf\ng\n", "b|f", options(1, 1, 2000));
        assert_eq!(
            out.matches.join("\n"),
            "f.txt-1-a\nf.txt:2:b\nf.txt-3-c\n--\nf.txt-5-e\nf.txt:6:f\nf.txt-7-g"
        );
    }

    #[test]
    fn context_merges_touching_windows() {
        let out = collect("a\nb\nc\nd\ne\n", "b|d", options(1, 1, 2000));
        assert_eq!(
            out.matches.join("\n"),
            "f.txt-1-a\nf.txt:2:b\nf.txt-3-c\nf.txt:4:d\nf.txt-5-e"
        );
    }

    #[test]
    fn context_max_matches_counts_matches_not_lines() {
        let body: String = (1..=10).map(|i| format!("hit {i}\n")).collect();
        let out = collect(&body, "hit", options(1, 1, 3));
        // All ten lines match, but only three are admitted; the fourth match
        // must be excluded entirely rather than emitted as `-` context.
        assert_eq!(
            out.matches.join("\n"),
            "f.txt:1:hit 1\nf.txt:2:hit 2\nf.txt:3:hit 3"
        );
        assert!(out.truncated, "truncation must be flagged");
    }

    #[test]
    fn is_binary_prefix_detects_nul_byte() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bin.dat");
        std::fs::write(&p, [0u8, 1, 2]).unwrap();
        assert!(is_binary_prefix(&p));
    }

    #[test]
    fn is_binary_prefix_accepts_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("text.txt");
        std::fs::write(&p, "hello world\n").unwrap();
        assert!(!is_binary_prefix(&p));
    }
}
