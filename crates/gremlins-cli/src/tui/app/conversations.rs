//! Resolve operator input against the current execution when Enter is pressed.

use super::{Execution, InputBuffer, Target};
use std::collections::HashMap;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ComposerAction {
    SessionChatPrompt,
    Correction {
        id: u64,
        execution: Execution,
        text: String,
    },
    Blocked(&'static str),
}

/// Render only in `target`'s transcript; receipts never change the composer.
pub(crate) struct Receipt {
    pub target: Target,
    pub text: String,
    pub result: Result<(), String>,
}

struct Pending {
    target: Target,
    execution: Execution,
    text: String,
}

pub(crate) struct Conversations {
    session_chat_target: Target,
    starting_session_chat: Option<String>,
    latest_session_chat_turn: Option<String>,
    live: HashMap<Target, Execution>,
    pending: HashMap<u64, Pending>,
    sequence: u64,
}

impl Default for Conversations {
    fn default() -> Self {
        Self {
            session_chat_target: Target::Run {
                run_id: super::PENDING_SESSION_ID.to_owned(),
            },
            starting_session_chat: None,
            latest_session_chat_turn: None,
            live: HashMap::new(),
            pending: HashMap::new(),
            sequence: 0,
        }
    }
}

impl Conversations {
    pub fn session_chat_target(&self) -> &Target {
        &self.session_chat_target
    }

    pub fn set_session_chat_target(&mut self, target: Target) {
        if self.session_chat_target != target {
            self.retire_session_chat_session();
            self.session_chat_target = target;
        }
    }

    /// An enqueue acknowledgment renames this session; it does not retire its owner.
    pub fn migrate_session_chat_target(&mut self, target: Target) {
        if self.session_chat_target == target {
            return;
        }
        let old = std::mem::replace(&mut self.session_chat_target, target.clone());
        self.live.remove(&target);
        if let Some(execution) = self.live.remove(&old) {
            self.live.insert(target.clone(), execution);
        }
        for pending in self.pending.values_mut() {
            if pending.target == old {
                pending.target = target.clone();
            }
        }
    }

    pub fn begin_session_chat_turn(&mut self, turn: String) {
        self.live.remove(&self.session_chat_target);
        self.latest_session_chat_turn = Some(turn.clone());
        self.starting_session_chat = Some(turn);
    }

    pub fn session_chat_turn(&self) -> Option<&str> {
        self.starting_session_chat.as_deref()
    }

    pub fn retire_session_chat_turn(&mut self) -> Option<String> {
        self.live.remove(&self.session_chat_target);
        self.starting_session_chat.take()
    }

    pub fn latest_session_chat_turn(&self) -> Option<&str> {
        self.latest_session_chat_turn.as_deref()
    }

    pub fn session_chat_turn_started(&mut self, turn: &str, execution: Option<Execution>) {
        if self.starting_session_chat.as_deref() != Some(turn) {
            return;
        }
        if let Some(execution) = execution {
            // Keep the turn identity for subsequent tagged agent frames.
            self.live
                .insert(self.session_chat_target.clone(), execution);
        }
    }

    /// Clearing session chat retires its turn owner so late frames cannot revive it.
    pub fn set_execution(&mut self, target: Target, execution: Option<Execution>) {
        if target == self.session_chat_target && execution.is_none() {
            self.retire_session_chat_turn();
        }
        let execution = execution.filter(|execution| match &target {
            Target::Invocation {
                run_id,
                stage_run_id,
                ..
            } => execution.run_id == *run_id && execution.stage_run_id == *stage_run_id,
            _ => true,
        });
        if let Some(execution) = execution {
            self.live.insert(target, execution);
        } else {
            self.live.remove(&target);
        }
    }

    #[cfg(test)]
    pub(crate) fn snapshot_execution(&self, target: &Target) -> Option<Execution> {
        self.live.get(target).cloned()
    }

    pub fn submit(&mut self, target: &Target, input: &InputBuffer) -> ComposerAction {
        if input.is_empty() {
            return ComposerAction::Blocked("empty draft");
        }
        let Some(execution) = self.live.get(target) else {
            return if target == &self.session_chat_target && self.starting_session_chat.is_none() {
                ComposerAction::SessionChatPrompt
            } else {
                ComposerAction::Blocked("this conversation has no writable execution")
            };
        };
        if self
            .pending
            .values()
            .any(|pending| &pending.execution == execution)
        {
            return ComposerAction::Blocked("delivery is pending for this execution");
        }
        self.sequence += 1;
        let id = self.sequence;
        let text = input.clone().resolve();
        let execution = execution.clone();
        self.pending.insert(
            id,
            Pending {
                target: target.clone(),
                execution: execution.clone(),
                text: text.clone(),
            },
        );
        ComposerAction::Correction {
            id,
            execution,
            text,
        }
    }

    /// Retire this generation's receipts before exposing the replacement session.
    pub fn retire_session_chat_session(&mut self) {
        self.latest_session_chat_turn = None;
        self.pending
            .retain(|_, pending| pending.target != self.session_chat_target);
        self.retire_session_chat_turn();
    }

    /// Ignore late receipts after detach.
    pub fn retire_pending(&mut self, target: &Target) {
        self.pending.retain(|_, pending| &pending.target != target);
    }

    /// Fail target-scoped commands whose error carries no request ID.
    pub fn fail_pending(&mut self, target: &Target, reason: &str) -> Vec<Receipt> {
        let ids: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(&id, pending)| (&pending.target == target).then_some(id))
            .collect();
        ids.into_iter()
            .filter_map(|id| self.finish(id, Err(reason.to_owned())))
            .collect()
    }

    pub fn finish(&mut self, id: u64, result: Result<(), String>) -> Option<Receipt> {
        let pending = self.pending.remove(&id)?;
        Some(Receipt {
            target: pending.target,
            text: format!(
                "{} [execution {}/{} nonce {}]",
                pending.text,
                pending.execution.run_id,
                pending.execution.stage_run_id,
                pending.execution.nonce
            ),
            result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution(nonce: u64) -> Execution {
        Execution {
            run_id: "runner".into(),
            stage_run_id: format!("stage-{nonce}"),
            nonce,
        }
    }

    fn session_chat() -> Target {
        Target::Run {
            run_id: super::super::PENDING_SESSION_ID.to_owned(),
        }
    }

    // Aggregate routing may change executions; receipts keep the submitted snapshot.
    fn stage() -> Target {
        Target::Run {
            run_id: "runner".into(),
        }
    }

    fn input(text: &str) -> InputBuffer {
        let mut input = InputBuffer::new();
        input.replace_all(text);
        input
    }

    fn correction(
        state: &mut Conversations,
        target: &Target,
        input: &InputBuffer,
        expected: &Execution,
    ) -> u64 {
        let ComposerAction::Correction {
            id,
            execution,
            text,
        } = state.submit(target, input)
        else {
            panic!("expected correction");
        };
        assert_eq!(&execution, expected);
        assert_eq!(text, input.clone().resolve());
        id
    }

    #[test]
    fn submission_uses_current_execution_and_receipt_keeps_submitted_snapshot() {
        let mut state = Conversations::default();
        let mut text = input("old");
        state.set_execution(stage(), Some(execution(1)));
        state.set_execution(stage(), Some(execution(2)));
        let id = correction(&mut state, &stage(), &text, &execution(2));
        text.replace_all("new");
        state.set_execution(stage(), Some(execution(3)));
        let next = correction(&mut state, &stage(), &text, &execution(3));
        let receipt = state.finish(id, Ok(())).unwrap();
        assert_eq!(receipt.target, stage());
        assert_eq!(receipt.text, "old [execution runner/stage-2 nonce 2]");
        assert!(state.finish(id, Ok(())).is_none());
        assert!(state.finish(next, Ok(())).is_some());
    }

    #[test]
    fn session_chat_routes_from_current_start_and_live_state() {
        let mut state = Conversations::default();
        let text = input("hello");
        assert_eq!(
            state.submit(&session_chat(), &text),
            ComposerAction::SessionChatPrompt
        );
        state.begin_session_chat_turn("turn".into());
        assert_eq!(state.session_chat_turn(), Some("turn"));
        assert!(matches!(
            state.submit(&session_chat(), &text),
            ComposerAction::Blocked(_)
        ));
        state.session_chat_turn_started("other", Some(execution(1)));
        assert!(matches!(
            state.submit(&session_chat(), &text),
            ComposerAction::Blocked(_)
        ));
        state.session_chat_turn_started("turn", Some(execution(2)));
        state.session_chat_turn_started("turn", None);
        let id = correction(&mut state, &session_chat(), &text, &execution(2));
        state.set_execution(session_chat(), None);
        state.session_chat_turn_started("turn", Some(execution(2)));
        assert_eq!(
            state.submit(&session_chat(), &text),
            ComposerAction::SessionChatPrompt
        );
        assert!(state.finish(id, Ok(())).is_some());

        state.begin_session_chat_turn("B".into());
        state.session_chat_turn_started("B", Some(execution(3)));
        state.session_chat_turn_started("turn", Some(execution(2)));
        correction(&mut state, &session_chat(), &text, &execution(3));
    }

    #[test]
    fn pending_blocks_same_execution_across_targets_and_failure_allows_retry() {
        let mut state = Conversations::default();
        let text = input("edit");
        state.set_execution(stage(), Some(execution(1)));
        state.set_execution(session_chat(), Some(execution(1)));
        let id = correction(&mut state, &stage(), &text, &execution(1));
        assert!(matches!(
            state.submit(&stage(), &text),
            ComposerAction::Blocked(_)
        ));
        assert!(matches!(
            state.submit(&session_chat(), &text),
            ComposerAction::Blocked(_)
        ));
        state.set_execution(session_chat(), Some(execution(2)));
        let session_chat_id = correction(&mut state, &session_chat(), &text, &execution(2));
        let receipts = state.fail_pending(&stage(), "busy");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].result, Err("busy".into()));
        assert!(state.finish(id, Ok(())).is_none());
        correction(&mut state, &stage(), &text, &execution(1));
        assert!(state.finish(session_chat_id, Ok(())).is_some());
    }

    #[test]
    fn retirement_is_scoped_and_ids_are_not_reused() {
        let mut state = Conversations::default();
        let text = input("edit");
        state.set_execution(stage(), Some(execution(1)));
        state.begin_session_chat_turn("turn".into());
        state.session_chat_turn_started("turn", Some(execution(2)));
        let stage_id = correction(&mut state, &stage(), &text, &execution(1));
        let session_chat_id = correction(&mut state, &session_chat(), &text, &execution(2));
        state.retire_session_chat_session();
        assert!(state.latest_session_chat_turn().is_none());
        assert!(state.finish(session_chat_id, Ok(())).is_none());
        let receipt = state.finish(stage_id, Ok(())).unwrap();
        assert_eq!(receipt.target, stage());
        assert_eq!(receipt.result, Ok(()));

        let detached_id = correction(&mut state, &stage(), &text, &execution(1));
        state.begin_session_chat_turn("next-turn".into());
        state.session_chat_turn_started("next-turn", Some(execution(2)));
        let concurrent_id = correction(&mut state, &session_chat(), &text, &execution(2));
        state.retire_pending(&stage());
        let next = correction(&mut state, &stage(), &text, &execution(1));
        for result in [Ok(()), Err("late failure".into())] {
            assert!(state.finish(detached_id, result).is_none());
            assert_eq!(
                state.submit(&stage(), &text),
                ComposerAction::Blocked("delivery is pending for this execution")
            );
        }
        assert!(state.finish(next, Ok(())).is_some());
        correction(&mut state, &stage(), &text, &execution(1));
        let receipt = state.finish(concurrent_id, Ok(())).unwrap();
        assert_eq!(receipt.target, session_chat());
        assert_eq!(receipt.result, Ok(()));
        assert!(detached_id > session_chat_id && session_chat_id > stage_id);
        assert!(next > concurrent_id && concurrent_id > detached_id);
    }

    #[test]
    fn enqueue_identity_migration_preserves_turn_execution_and_receipts() {
        let mut state = Conversations::default();
        let old = session_chat();
        let replacement = Target::Run {
            run_id: "replacement".into(),
        };
        let text = input("edit");
        state.set_execution(stage(), Some(execution(1)));
        let other_id = correction(&mut state, &stage(), &text, &execution(1));
        state.begin_session_chat_turn("acknowledged-turn".into());
        state.session_chat_turn_started("acknowledged-turn", Some(execution(2)));
        let id = correction(&mut state, &old, &text, &execution(2));

        state.migrate_session_chat_target(replacement.clone());
        state.migrate_session_chat_target(replacement.clone());
        assert_eq!(state.session_chat_turn(), Some("acknowledged-turn"));
        assert_eq!(state.latest_session_chat_turn(), Some("acknowledged-turn"));
        assert!(!state.live.contains_key(&old));
        assert_eq!(state.live.get(&replacement), Some(&execution(2)));
        assert_eq!(
            state.submit(&replacement, &text),
            ComposerAction::Blocked("delivery is pending for this execution")
        );
        let receipt = state.finish(id, Ok(())).unwrap();
        assert_eq!(receipt.target, replacement);
        assert_eq!(receipt.text, "edit [execution runner/stage-2 nonce 2]");
        assert_eq!(state.finish(other_id, Ok(())).unwrap().target, stage());
        correction(&mut state, &replacement, &text, &execution(2));
        assert!(matches!(
            state.submit(&old, &text),
            ComposerAction::Blocked(_)
        ));
    }

    #[test]
    fn enqueue_identity_migration_preserves_queued_and_completed_turn_identity() {
        for completed in [false, true] {
            let mut state = Conversations::default();
            state.begin_session_chat_turn("turn".into());
            if completed {
                state.retire_session_chat_turn();
            }
            let replacement = Target::Run {
                run_id: "replacement".into(),
            };
            state.migrate_session_chat_target(replacement.clone());
            assert_eq!(state.latest_session_chat_turn(), Some("turn"));
            if completed {
                assert!(state.session_chat_turn().is_none());
                state.session_chat_turn_started("turn", Some(execution(2)));
                assert_eq!(
                    state.submit(&replacement, &input("next")),
                    ComposerAction::SessionChatPrompt
                );
            } else {
                assert_eq!(state.session_chat_turn(), Some("turn"));
                assert!(matches!(
                    state.submit(&replacement, &input("next")),
                    ComposerAction::Blocked(_)
                ));
                state.session_chat_turn_started("turn", Some(execution(2)));
                correction(&mut state, &replacement, &input("edit"), &execution(2));
            }
        }
    }

    #[test]
    fn changing_session_chat_target_retires_only_its_generation() {
        let mut state = Conversations::default();
        let text = input("edit");
        let replacement = Target::Run {
            run_id: "replacement".into(),
        };
        state.set_execution(stage(), Some(execution(1)));
        let stage_id = correction(&mut state, &stage(), &text, &execution(1));
        state.begin_session_chat_turn("old-turn".into());
        state.session_chat_turn_started("old-turn", Some(execution(2)));
        let old_id = correction(&mut state, &session_chat(), &text, &execution(2));
        state.set_session_chat_target(session_chat());
        assert_eq!(state.session_chat_turn(), Some("old-turn"));
        state.set_session_chat_target(replacement.clone());
        assert!(state.session_chat_turn().is_none());
        assert!(state.latest_session_chat_turn().is_none());
        assert!(state.finish(old_id, Ok(())).is_none());
        assert!(state.finish(stage_id, Ok(())).is_some());
        state.session_chat_turn_started("old-turn", Some(execution(2)));
        assert_eq!(
            state.submit(&replacement, &text),
            ComposerAction::SessionChatPrompt
        );
        assert!(matches!(
            state.submit(&session_chat(), &text),
            ComposerAction::Blocked(_)
        ));
        state.begin_session_chat_turn("new-turn".into());
        state.set_execution(session_chat(), None);
        assert_eq!(state.session_chat_turn(), Some("new-turn"));
        state.session_chat_turn_started("new-turn", Some(execution(3)));
        let next = correction(&mut state, &replacement, &text, &execution(3));
        assert!(next > old_id);
        assert_eq!(state.finish(next, Ok(())).unwrap().target, replacement);
    }

    #[test]
    fn invocation_mismatch_and_empty_input_are_unwritable() {
        let mut state = Conversations::default();
        let target = Target::Invocation {
            run_id: "runner".into(),
            stage: "build".into(),
            stage_run_id: "stage-1".into(),
        };
        state.set_execution(target.clone(), Some(execution(2)));
        assert!(matches!(
            state.submit(&target, &input("edit")),
            ComposerAction::Blocked(_)
        ));
        state.set_execution(target.clone(), Some(execution(1)));
        assert!(matches!(
            state.submit(&target, &input("")),
            ComposerAction::Blocked(_)
        ));
        for mismatch in [
            execution(2),
            Execution {
                run_id: "other-runner".into(),
                ..execution(1)
            },
        ] {
            state.set_execution(target.clone(), Some(execution(1)));
            let id = correction(&mut state, &target, &input("edit"), &execution(1));
            state.set_execution(target.clone(), Some(mismatch));
            assert_eq!(
                state.submit(&target, &input("new edit")),
                ComposerAction::Blocked("this conversation has no writable execution")
            );
            let receipt = state.finish(id, Ok(())).unwrap();
            assert_eq!(receipt.target, target);
            assert_eq!(receipt.text, "edit [execution runner/stage-1 nonce 1]");
            assert_eq!(receipt.result, Ok(()));
            assert_eq!(
                state.submit(&target, &input("new edit")),
                ComposerAction::Blocked("this conversation has no writable execution")
            );
        }
    }

    #[test]
    fn receipts_retain_resolved_substitutions() {
        let mut state = Conversations::default();
        state.set_execution(stage(), Some(execution(1)));
        let mut text = input("[paste]");
        text.add_substitution(0..7, "resolved content".into());
        let id = correction(&mut state, &stage(), &text, &execution(1));
        let receipt = state.finish(id, Err("rejected".into())).unwrap();
        assert!(receipt.text.starts_with("resolved content [execution"));
        assert_eq!(text.text(), "[paste]");
    }
}
