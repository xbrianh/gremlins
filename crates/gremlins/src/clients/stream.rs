use std::io::Write;
use std::time::{Instant, SystemTime};

use rig_core::completion::Usage;
use rig_core::message::ToolCall;

pub(crate) fn ts_internal() -> String {
    // Manual UTC formatting to avoid chrono dependency
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    // Compute date/time components from UNIX epoch seconds
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Convert days since epoch to year/month/day using civil calendar
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Algorithm from Howard Hinnant
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

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
    eprintln!(
        "{} {}init model={} cwd={} reasoning_effort={}",
        ts_internal(),
        prefix,
        model,
        cwd,
        effort
    );
}

pub(crate) fn emit_text(prefix: &str, text: &str) {
    eprintln!("{} {}text: {}", ts_internal(), prefix, trunc(text, 200));
}

pub(crate) fn emit_think(prefix: &str, thinking: &str) {
    eprintln!(
        "{} {}think: {}",
        ts_internal(),
        prefix,
        trunc(thinking, 200)
    );
}

pub(crate) fn emit_tool(prefix: &str, name: &str, arg: &str) {
    eprintln!(
        "{} {}tool: {} {}",
        ts_internal(),
        prefix,
        name,
        trunc(arg, 200)
    );
}

pub(crate) fn emit_result(prefix: &str, content: &str, is_error: bool) {
    let err = if is_error { " ERROR" } else { "" };
    eprintln!(
        "{} {}result{}: {}",
        ts_internal(),
        prefix,
        err,
        trunc(content, 200)
    );
}

pub(crate) fn flush() {
    let _ = std::io::stderr().flush();
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

    eprintln!(
        "{} {}metrics: turn={} ttft={} gen={} tools={} prompt={} completion={} cached={}({}) reasoning_tok={} reasoning_byte_ratio={}",
        ts_internal(),
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
    flush();
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

    eprintln!(
        "{} {}summary: turns={} wall={:.1}s token_total={} prompt_avg={} completion_avg={} cached_avg={} cache_creation={} reasoning_pct={}",
        ts_internal(),
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
    flush();
}
