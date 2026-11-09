//! Cut-point helpers and compaction orchestration state.
//!
//! Cut-point functions operate purely on [`Message`] slices — they have no
//! dependency on [`Backend`], [`Compaction`], or any Anthropic-specific type.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rig_core::completion::message::UserContent;
use rig_core::completion::Message;

use crate::clients::protocol::CompactionBlock;

/// Compute a cut index so `to_compact = &history[..cut]` and
/// `kept = &history[cut..]`. The cut lands on a completed-turn boundary:
/// `keep_recent` whole user/assistant exchanges are kept verbatim and
/// `to_compact` never ends in an unanswered tool call.
pub(crate) fn compaction_cut_index(history: &[Message], keep_recent: usize) -> usize {
    let mut cut = history.len();
    let mut kept_turns = 0;
    while cut > 0 && kept_turns < keep_recent {
        // Skip trailing user tool_result messages belonging to the
        // preceding turn.
        while cut > 0 && is_user_tool_result(&history[cut - 1]) {
            cut -= 1;
        }
        // Step back over this turn's user opener and its assistant/tool
        // messages to the previous completed-turn boundary.
        cut = previous_turn_boundary(history, cut);
        kept_turns += 1;
    }
    cut
}

/// Walk backward from `cut` (exclusive) to the start of the preceding
/// completed user+assistant turn, skipping assistant/tool messages.
fn previous_turn_boundary(history: &[Message], mut cut: usize) -> usize {
    // Step back over a chain of (assistant, user tool_result) pairs until
    // we hit a user message that is NOT a tool result (the turn opener).
    loop {
        // Skip the assistant message.
        if cut > 0 && matches!(history[cut - 1], Message::Assistant { .. }) {
            cut -= 1;
        } else {
            break;
        }
        // Skip any user tool_result messages after the assistant.
        while cut > 0 && is_user_tool_result(&history[cut - 1]) {
            cut -= 1;
        }
    }
    // Step back over the user opener.
    if cut > 0 && matches!(history[cut - 1], Message::User { .. }) {
        cut -= 1;
    }
    cut
}

/// True when the message is a user message containing exclusively tool
/// results (no mixed text + tool-result blocks).
fn is_user_tool_result(msg: &Message) -> bool {
    match msg {
        Message::User { content } => {
            !content.is_empty()
                && content
                    .iter()
                    .all(|c| matches!(c, UserContent::ToolResult(_)))
        }
        _ => false,
    }
}

// ── Orchestration state ──────────────────────────────────────────────

/// What triggers compaction for a conversation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionTrigger {
    /// Auto-compaction fires when input tokens exceed the threshold.
    Threshold,
    /// No automatic trigger; only fires when the force flag is set
    /// (via the `/compact` TUI command).
    Immediate,
}

/// Per-conversation compaction orchestration state. Built once in `run.rs`
/// and passed to `StandardConversation::new`.
///
/// Cloning creates a snapshot of the runtime state (`block`, `applied`,
/// `cooldown`) — forked conversations inherit but do not share
/// compaction state.
#[derive(Clone)]
pub(crate) struct CompactionState {
    threshold: u64,
    keep_recent: usize,
    block: Option<CompactionBlock>,
    applied: bool,
    cooldown: bool,
    /// One-shot flag: when `true`, `should_compact()` skips threshold and
    /// cooldown checks, clears the flag, and runs compaction if there is
    /// material to compact. Set by the TUI `/compact` command.
    force: Arc<AtomicBool>,
    /// What triggers compaction for this conversation.
    trigger: CompactionTrigger,
}

impl CompactionState {
    pub(crate) fn new(
        threshold: u64,
        keep_recent: usize,
        force: Arc<AtomicBool>,
        trigger: CompactionTrigger,
    ) -> Self {
        Self {
            threshold,
            keep_recent,
            block: None,
            applied: false,
            cooldown: false,
            force,
            trigger,
        }
    }

    pub(crate) fn keep_recent(&self) -> usize {
        self.keep_recent
    }

    pub(crate) fn block(&self) -> Option<&CompactionBlock> {
        self.block.as_ref()
    }

    /// Record that a compaction result has been applied to history.
    /// Sets `applied` and stores the block (if any) atomically, preserving
    /// the invariant that `block` is set iff a server-side compaction was used.
    /// Clears the one-shot force flag — it is only consumed on success.
    /// Server strategy → `Some(signed_block)`
    /// Local strategy  → `None`
    pub(crate) fn mark_applied(&mut self, block: Option<CompactionBlock>) {
        self.block = block;
        self.applied = true;
        self.force.store(false, Ordering::Release);
    }

    /// Prevent retrying the expensive API call on every subsequent turn.
    ///
    /// Also clears the one-shot force flag: cooldown means a `/compact` was
    /// attempted this turn (and failed or produced no summary), so it must not
    /// re-fire on later turns. Pending requests are only preserved while there
    /// is insufficient material, which is handled in `should_compact` and never
    /// reaches this method.
    pub(crate) fn set_cooldown(&mut self) {
        self.cooldown = true;
        self.force.store(false, Ordering::Release);
    }

    /// Decide whether to trigger compaction on this turn.
    pub(crate) fn should_compact(
        &self,
        last_input_tokens: Option<u64>,
        history: &[Message],
    ) -> bool {
        // One-shot: operator forced this turn. The flag is cleared on success
        // (mark_applied) and on failure (set_cooldown). If there is not enough
        // material yet (cut == 0), leave it set so it fires once material
        // accumulates.
        if self.force.load(Ordering::Acquire) {
            let cut = compaction_cut_index(history, self.keep_recent);
            return cut > 0;
        }
        // Auto path — only active for Threshold trigger.
        if matches!(self.trigger, CompactionTrigger::Immediate) {
            return false;
        }
        if self.cooldown {
            return false;
        }
        let tokens = last_input_tokens.unwrap_or(0);
        if tokens <= self.threshold {
            return false;
        }
        // After compaction has been applied, don't re-compact until there is
        // genuinely new material after the block.
        if self.applied {
            let cut = compaction_cut_index(history, self.keep_recent);
            if cut == 0 {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::{AssistantContent, Text, UserContent};

    /// Build a simple user message with one text block.
    fn user_msg<S: Into<String>>(s: S) -> Message {
        Message::User {
            content: vec![UserContent::Text(Text {
                text: s.into(),
                additional_params: None,
            })],
        }
    }

    /// Build a simple assistant message with one text block.
    fn asst_msg<S: Into<String>>(s: S) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::Text(Text {
                text: s.into(),
                additional_params: None,
            })],
        }
    }

    /// Build a history with `pairs` user→assistant exchanges.
    fn paired_history(pairs: usize) -> Vec<Message> {
        let mut h = Vec::with_capacity(pairs * 2);
        for i in 0..pairs {
            h.push(user_msg(format!("user {i}")));
            h.push(asst_msg(format!("assistant {i}")));
        }
        h
    }

    /// Return a [`CompactionState`] with defaults, accepting overrides via a
    /// builder closure.
    fn make_state(f: impl FnOnce(&mut CompactionState)) -> CompactionState {
        let mut s = CompactionState::new(
            1000,
            10,
            Arc::new(AtomicBool::new(false)),
            CompactionTrigger::Threshold,
        );
        f(&mut s);
        s
    }

    #[test]
    fn should_compact_threshold_below_returns_false() {
        // 1 pair = 1 user + 1 asst. With keep_recent=10,
        // compaction_cut_index returns 0 — no material to compact.
        let history = paired_history(1);
        let s = make_state(|_| {});
        assert!(!s.should_compact(Some(500), &history));
    }

    #[test]
    fn should_compact_threshold_exceeded_returns_true() {
        // 20 pairs = 40 messages. keep_recent=10 → cut > 0.
        let history = paired_history(20);
        let s = make_state(|_| {});
        assert!(s.should_compact(Some(1500), &history));
    }

    #[test]
    fn should_compact_immediate_trigger_ignores_threshold() {
        // Immediate trigger skips auto — should return false even above threshold.
        let history = paired_history(20);
        let s = make_state(|s| {
            s.trigger = CompactionTrigger::Immediate;
        });
        assert!(!s.should_compact(Some(1500), &history));
    }

    #[test]
    fn should_compact_force_overrides_all() {
        // force=true should return true even if cooldown, threshold=0, trigger=Immediate.
        let history = paired_history(20);
        let s = make_state(|s| {
            s.trigger = CompactionTrigger::Immediate;
            s.cooldown = true;
            s.threshold = 0;
        });
        s.force.store(true, Ordering::Release);
        assert!(s.should_compact(Some(0), &history));
    }

    #[test]
    fn should_compact_force_with_insufficient_material_returns_false_preserves_flag() {
        // 1 pair + keep_recent=10 → cut=0. Force stays set for next turn.
        let history = paired_history(1);
        let s = make_state(|_| {});
        s.force.store(true, Ordering::Release);
        assert!(!s.should_compact(Some(0), &history));
        assert!(
            s.force.load(Ordering::Acquire),
            "force flag must survive insufficient material"
        );
    }

    #[test]
    fn should_compact_cooldown_returns_false() {
        let history = paired_history(20);
        let s = make_state(|s| {
            s.cooldown = true;
        });
        assert!(!s.should_compact(Some(1500), &history));
    }

    #[test]
    fn should_compact_applied_with_no_new_material_returns_false() {
        // 10 pairs, keep_recent=10 → cut=0 after applied.
        let history = paired_history(10);
        let s = make_state(|s| {
            s.applied = true;
        });
        assert!(!s.should_compact(Some(1500), &history));
    }

    #[test]
    fn mark_applied_clears_force_flag() {
        let s = make_state(|_| {});
        s.force.store(true, Ordering::Release);
        let mut s = s.clone();
        s.mark_applied(None);
        assert!(!s.force.load(Ordering::Acquire));
    }

    #[test]
    fn set_cooldown_clears_force_flag() {
        let s = make_state(|_| {});
        s.force.store(true, Ordering::Release);
        let mut s = s.clone();
        s.set_cooldown();
        assert!(!s.force.load(Ordering::Acquire));
    }
}
