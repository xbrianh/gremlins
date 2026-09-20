use std::time::Instant;

use rig_core::completion::Usage;
use rig_core::message::ToolCall;

fn trunc(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > n {
        format!("{}...", s.chars().take(n).collect::<String>())
    } else {
        s
    }
}

pub(crate) fn emit_init(prefix: &str, model: &str, cwd: &str, reasoning_effort: Option<&str>) {
    let effort = trunc(reasoning_effort.unwrap_or("default"), 50);
    log::info!(
        "{}init model={} cwd={} reasoning_effort={}",
        prefix,
        model,
        cwd,
        effort
    );
}

pub(crate) fn emit_text(prefix: &str, text: &str) {
    log::info!("{}text: {}", prefix, trunc(text, 200));
}

pub(crate) fn emit_think(prefix: &str, thinking: &str) {
    log::info!("{}think: {}", prefix, trunc(thinking, 200));
}

pub(crate) fn emit_tool(prefix: &str, name: &str, arg: &str) {
    log::info!("{}tool: {} {}", prefix, name, trunc(arg, 200));
}

pub(crate) fn emit_result(prefix: &str, content: &str, is_error: bool) {
    if is_error {
        log::warn!("{}result ERROR: {}", prefix, trunc(content, 200));
    } else {
        log::info!("{}result: {}", prefix, trunc(content, 200));
    }
}

pub(crate) fn flush() {
    // env_logger flushes each record; no-op kept for API compatibility
}

/// Check if per-turn telemetry is enabled via GREMLINS_TELEMETRY env var.
fn telemetry_enabled() -> bool {
    crate::config::telemetry_enabled()
}

/// Emit per-turn telemetry: timing, token counts, cache hit ratio, reasoning ratio.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_turn_metrics(
    prefix: &str,
    turn: usize,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    turn_start: Instant,
    reasoning: &str,
    text: &str,
    tool_calls: &[ToolCall],
    usage: Option<&Usage>,
) {
    if !telemetry_enabled() {
        return;
    }

    let ttft = first_token
        .map(|ft| ft.duration_since(turn_start))
        .map(|d| format!("{:.1}s", d.as_secs_f64()))
        .unwrap_or_else(|| "-".into());

    let gen_time = match (first_token, last_token) {
        (Some(ft), Some(lt)) => {
            let d = lt.duration_since(ft);
            format!("{:.1}s", d.as_secs_f64())
        }
        _ => "-".into(),
    };

    let prompt = usage.map(|u| u.input_tokens).unwrap_or(0);
    let completion = usage.map(|u| u.output_tokens).unwrap_or(0);
    let cached = usage.map(|u| u.cached_input_tokens).unwrap_or(0);
    let reasoning_tok = usage.map(|u| u.reasoning_tokens).unwrap_or(0);

    let cache_pct = if prompt > 0 {
        format!("{:.0}%", (cached as f64 / (prompt as f64).max(1.0)) * 100.0)
    } else {
        "-".into()
    };

    // Byte lengths (not char counts) — a cheap proxy for output volume.
    let reasoning_bytes = reasoning.len();
    let text_bytes = text.len();
    let total_bytes = reasoning_bytes + text_bytes;
    let reasoning_byte_ratio = if total_bytes > 0 {
        format!(
            "{:.0}%",
            (reasoning_bytes as f64 / total_bytes as f64) * 100.0
        )
    } else {
        "-".into()
    };

    log::debug!(
        "{}metrics: turn={} ttft={} gen={} tools={} prompt={} completion={} cached={}({}) reasoning_tok={} reasoning_byte_ratio={}",
        prefix,
        turn,
        ttft,
        gen_time,
        tool_calls.len(),
        prompt,
        completion,
        cached,
        cache_pct,
        reasoning_tok,
        reasoning_byte_ratio,
    );
}

/// Emit stage-end telemetry summary (always emitted, not gated by GREMLINS_TELEMETRY).
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_summary(
    prefix: &str,
    turns: usize,
    loop_start: Instant,
    total_prompt: u64,
    total_completion: u64,
    total_cached: u64,
    total_cache_creation: u64,
    total_reasoning: u64,
) {
    let wall = loop_start.elapsed();
    let token_total = total_prompt + total_completion;

    let prompt_avg = if turns > 0 {
        total_prompt / turns as u64
    } else {
        0
    };
    let completion_avg = if turns > 0 {
        total_completion / turns as u64
    } else {
        0
    };
    let cached_avg = if total_prompt > 0 {
        format!(
            "{:.0}%",
            (total_cached as f64 / total_prompt as f64) * 100.0
        )
    } else {
        "-".into()
    };
    // Reasoning tokens are a subset of completion/output tokens, so report the
    // ratio against completion — "how much of the model's output was reasoning".
    let reasoning_pct = if total_completion > 0 {
        format!(
            "{:.0}%",
            (total_reasoning as f64 / total_completion as f64) * 100.0
        )
    } else {
        "-".into()
    };

    log::info!(
        "{}summary: turns={} wall={:.1}s token_total={} prompt_avg={} completion_avg={} cached_avg={} cache_creation={} reasoning_pct={}",
        prefix,
        turns,
        wall.as_secs_f64(),
        token_total,
        prompt_avg,
        completion_avg,
        cached_avg,
        total_cache_creation,
        reasoning_pct,
    );
}
