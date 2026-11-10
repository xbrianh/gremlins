use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::clients::tools::{
    blocking_string, parse_args, push_u64_arg, req_str, str_detail, AgentLoopRuntime, ToolHandler,
    ToolOutcome,
};
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
    let mut lines: Vec<&str> = content.split_inclusive('\n').collect();
    if offset > 0 {
        lines = lines.into_iter().skip(offset).collect();
    }
    if let Some(limit) = limit {
        lines.truncate(limit);
    }
    let mut numbered = String::new();
    for (idx, line) in lines.iter().enumerate() {
        // `offset` is a 0-indexed skip count, so the file line number is
        // offset + window index + 1 — never the window-relative index.
        numbered.push_str(&format!("{}|{line}", offset + idx + 1));
    }

    // Apply the global output cap here (rather than via `cap_output`) so the
    // truncation marker can carry an offset-aware continuation hint. When the
    // numbered output exceeds the budget, keep a byte-safe prefix and append a
    // marker telling the model the exact `offset` that resumes where this
    // window was cut. `lines.len()` counts the window we just emitted, so
    // `offset + lines.len()` is the 0-indexed line to read next.
    let cap = crate::config::max_tool_output_bytes();
    if cap == 0 || numbered.len() as u64 <= cap {
        return numbered;
    }
    let next_offset = offset + lines.len();
    let marker =
        format!("\n[...truncated at {cap} bytes. Read with offset={next_offset} to continue...]");
    let marker_len = marker.len() as u64;
    let keep = cap.saturating_sub(marker_len) as usize;
    if keep == 0 {
        // Cap is smaller than the marker itself — emit a byte-safe slice of the
        // marker so the whole result still fits the budget.
        return truncate_to_bytes(&marker, cap as usize);
    }
    let mut trunc = String::with_capacity(cap as usize);
    let mut used = 0usize;
    for c in numbered.chars() {
        let cl = c.len_utf8();
        if used + cl > keep {
            break;
        }
        trunc.push(c);
        used += cl;
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
