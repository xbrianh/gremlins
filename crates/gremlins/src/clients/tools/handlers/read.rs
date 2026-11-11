use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::tools::registry::{push_u64_arg, str_detail, ToolHandler};
use crate::clients::tools::runtime::{AgentLoopRuntime, ToolOutcome};
use crate::clients::tools::util::{blocking_string, parse_args, req_str};
use crate::core::paths::{enforce, io_enforce, resolve};

pub(crate) struct ReadTool;

#[async_trait]
impl ToolHandler for ReadTool {
    fn title_detail(&self, args: &serde_json::Value) -> Option<String> {
        str_detail(args, "file_path")
    }

    fn title_uses_secondaries(&self) -> bool {
        true
    }

    fn secondary_args(&self, args: &serde_json::Value, _string_cap: Option<usize>) -> Vec<String> {
        let mut out = Vec::new();
        push_u64_arg(&mut out, args, "offset");
        push_u64_arg(&mut out, args, "limit");
        out
    }

    fn display_truncation(&self) -> usize {
        500
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

    fn output_cap(&self) -> bool {
        // `read_sync` handles its own truncation so it can emit an
        // offset-aware continuation hint (`Read with offset=N`). Returning
        // `false` keeps `cap_output` from truncating with a generic marker.
        false
    }

    async fn invoke(
        &self,
        _id: &str,
        ctx: &AgentLoopRuntime,
        _conv: &dyn crate::clients::conversation::Conversation,
        args_json: &str,
    ) -> ToolOutcome {
        ToolOutcome::text(read_invoke(ctx, args_json).await)
    }
}

pub(crate) async fn read_invoke(ctx: &AgentLoopRuntime, args_json: &str) -> String {
    let cwd = ctx.cwd.clone();
    let roots = ctx.sandbox.roots().map(|r| r.to_vec());
    let args_json = args_json.to_string();
    blocking_string(move || read_sync(cwd.as_deref(), roots.as_deref(), &args_json)).await
}

fn read_sync(cwd: Option<&Path>, roots: Option<&[PathBuf]>, args_json: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let file_path = match req_str(&args, "file_path") {
        Ok(s) => s,
        Err(e) => return e,
    };
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
    if content.is_empty() {
        return String::new();
    }
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let all_lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total_lines = all_lines.len();
    let mut lines: Vec<&str> = all_lines;
    if offset > 0 {
        lines = lines.into_iter().skip(offset).collect();
    }
    let line_limited = if let Some(limit) = limit {
        if lines.len() > limit {
            lines.truncate(limit);
            true
        } else {
            false
        }
    } else {
        false
    };
    let mut numbered = String::new();
    for (idx, line) in lines.iter().enumerate() {
        // `offset` is a 0-indexed skip count, so the file line number is
        // offset + window index + 1 — never the window-relative index.
        numbered.push_str(&format!("{}|{line}", offset + idx + 1));
    }
    let emitted_line_count = lines.len();

    // Apply the global output cap here (rather than via `cap_output`) so the
    // truncation marker can carry an offset-aware continuation hint. When the
    // numbered output exceeds the budget, keep a byte-safe prefix and append a
    // marker telling the model the exact `offset` that resumes where this
    // window was cut. Count complete lines in the kept prefix to derive the
    // actual number of emitted lines, so the hint never skips truncated-away
    // lines.
    let cap = crate::config::max_tool_output_bytes();
    if cap == 0 || numbered.len() as u64 <= cap {
        // Byte budget not exceeded — but if lines were omitted due to a
        // `limit` parameter (not EOF), emit a continuation hint.
        let more_after_line_cap = line_limited && offset + emitted_line_count < total_lines;
        if more_after_line_cap {
            let next_offset = offset + emitted_line_count;
            numbered.push_str(&format!(
                "\n[Read with offset={next_offset} to continue...]"
            ));
        }
        return numbered;
    }
    // Reserve ~75 bytes for the marker (longest reasonable offset + framing).
    let marker_overhead: u64 = 75;
    let keep = cap.saturating_sub(marker_overhead) as usize;
    if keep == 0 {
        let minimal = format!("\n[...truncated at {cap} bytes...]");
        return truncate_to_bytes(&minimal, cap as usize);
    }
    let mut trunc = String::with_capacity(cap as usize);
    let mut used = 0usize;
    let mut complete_lines = 0usize;
    for c in numbered.chars() {
        let cl = c.len_utf8();
        if used + cl > keep {
            break;
        }
        trunc.push(c);
        used += cl;
        if c == '\n' {
            complete_lines += 1;
        }
    }
    // If the last character pushed was not a newline, the current line was
    // only partially emitted — do not count it.
    let next_offset = offset + complete_lines;
    // When no complete line fits within the budget (a single line exceeds
    // the cap), next_offset == offset and "continue" would be misleading.
    // Report the oversized line explicitly instead.
    let marker = if complete_lines == 0 {
        format!("\n[...truncated at {cap} bytes — line too long for offset hint...]")
    } else {
        format!("\n[...truncated at {cap} bytes. Read with offset={next_offset} to continue...]")
    };
    // If the real marker is larger than our estimate, re-truncate.
    if trunc.len() + marker.len() > cap as usize {
        let actual_keep = cap.saturating_sub(marker.len() as u64) as usize;
        trunc.clear();
        let mut used2 = 0usize;
        let mut complete_lines2 = 0usize;
        for c in numbered.chars() {
            let cl = c.len_utf8();
            if used2 + cl > actual_keep {
                break;
            }
            trunc.push(c);
            used2 += cl;
            if c == '\n' {
                complete_lines2 += 1;
            }
        }
        let next_offset2 = offset + complete_lines2;
        let marker2 = if complete_lines2 == 0 {
            format!("\n[...truncated at {cap} bytes — line too long for offset hint...]")
        } else {
            format!(
                "\n[...truncated at {cap} bytes. Read with offset={next_offset2} to continue...]"
            )
        };
        trunc.push_str(&marker2);
        return trunc;
    }
    trunc.push_str(&marker);
    trunc
}

/// Truncate `s` to at most `max_bytes`, never splitting a UTF-8 codepoint.
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
