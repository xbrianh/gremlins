use super::defaults::global_registry;
use super::runtime::{AgentLoopRuntime, ToolOutcome};
use super::sandbox::{audit, audit_key_arg, check_tool};
use super::schema::validate_tool_args;
use super::tool_specs::always_available;
use super::util::{parse_args, result_status};
use crate::clients::conversation::Conversation;

/// Dispatch a single tool call. `conv` is the parent conversation;
/// fan-out handlers (`Task`) use it to seed a child loop.
pub(crate) async fn invoke_with_conversation(
    id: &str,
    name: &str,
    ctx: &AgentLoopRuntime,
    args_json: &str,
    conv: &dyn Conversation,
) -> ToolOutcome {
    // Chat-only tools are never available in headless mode. The registry
    // provides `chat_only()` as a trait hook so dispatch never names a tool.
    let chat = ctx.skills_available;
    let registry = global_registry();
    let handler = registry.handlers.get(name);
    if let Some(h) = handler {
        if !chat && h.chat_only() {
            return ToolOutcome::text(format!("Error: unknown tool {name}"));
        }
    }

    if let Some(allowed) = &ctx.allowed_tools {
        // Gate: bypass the filter when the tool is always-available (Task /
        // Done), has a bypass_filter() hook (LoadSkill), is a chat-only tool
        // in a chat session, or is explicitly listed in the allowlist.
        let bypass = handler.map(|h| h.bypass_filter()).unwrap_or(false);
        let chat_bypass = chat && handler.map(|h| h.chat_only()).unwrap_or(false);
        if !always_available(name) && !bypass && !chat_bypass && !allowed.iter().any(|n| n == name)
        {
            return ToolOutcome::text(format!("Error: unknown tool {name}"));
        }
    }
    let ka = audit_key_arg(args_json);
    let mut args = match parse_args(args_json) {
        Ok(v) => v,
        Err(_) => {
            audit(
                ctx.audit_log.as_deref(),
                ctx.audit_lock.as_deref(),
                name,
                &ka,
                "error",
            );
            return ToolOutcome::text("Error: invalid arguments");
        }
    };

    // Before schema validation, give the handler a chance to auto-repair
    // common argument malformations.
    let mut repaired_args_json = args_json.to_string();
    if let Some(h) = handler {
        if let Some(repaired) = h.repair_args(args.clone()) {
            if repaired != args {
                repaired_args_json =
                    serde_json::to_string(&repaired).unwrap_or_else(|_| args_json.to_string());
                args = repaired;
            }
        }
    }

    // Validate args against the tool's expected schema before dispatching.
    if let Some(err) = validate_tool_args(name, &args) {
        audit(
            ctx.audit_log.as_deref(),
            ctx.audit_lock.as_deref(),
            name,
            &ka,
            "error",
        );
        return ToolOutcome::text(err);
    }
    if let Some(err) = check_tool(name, ctx, &args) {
        audit(
            ctx.audit_log.as_deref(),
            ctx.audit_lock.as_deref(),
            name,
            &ka,
            "denied",
        );
        return ToolOutcome::text(err);
    }

    // Dispatch through the registry.
    let mut outcome = match handler {
        Some(h) => h.invoke(id, ctx, conv, &repaired_args_json).await,
        None => {
            return ToolOutcome::text(format!("Error: unknown tool {name}"));
        }
    };
    audit(
        ctx.audit_log.as_deref(),
        ctx.audit_lock.as_deref(),
        name,
        &ka,
        result_status(&outcome.output),
    );
    outcome.output = cap_output(name, outcome.output);
    outcome
}

/// Cap output for tools that can produce unbounded results. Per-tool policy
/// (`output_cap` + `output_marker`) lives on each handler.
pub(crate) fn cap_output(name: &str, output: String) -> String {
    let cap = crate::config::max_tool_output_bytes();
    let handler = match global_registry().handlers.get(name) {
        Some(h) => h,
        None => return output,
    };
    if !handler.output_cap() {
        return output;
    }
    if cap > 0 && output.len() as u64 > cap {
        let marker = handler.output_marker(cap);
        let marker_len = marker.len() as u64;
        let keep = cap.saturating_sub(marker_len) as usize;
        if keep == 0 {
            super::util::truncate_to_bytes(&marker, cap as usize)
        } else {
            let mut trunc = String::with_capacity(cap as usize);
            let mut used = 0usize;
            for c in output.chars() {
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
    } else {
        output
    }
}

/// Convenience wrapper for tests that don't need conversation contexts.
#[cfg(test)]
pub(crate) async fn invoke(
    id: &str,
    name: &str,
    ctx: &AgentLoopRuntime,
    args_json: &str,
) -> String {
    let conv = Box::new(crate::clients::conversation::ConversationStub::new(""));
    invoke_with_conversation(id, name, ctx, args_json, &*conv)
        .await
        .output
}
