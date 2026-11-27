//! Resolve operator input against the current execution when Enter is pressed.

use super::{Execution, InputBuffer, Target};
use std::collections::HashMap;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Submission {
    HomePrompt,
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

#[derive(Default)]
pub(crate) struct Conversations {
    starting_home: Option<String>,
    latest_home_turn: Option<String>,
    live: HashMap<Target, Execution>,
    pending: HashMap<u64, Pending>,
    sequence: u64,
}

impl Conversations {
    pub fn begin_home_turn(&mut self, turn: String) {
        self.live.remove(&Target::Home);
        self.latest_home_turn = Some(turn.clone());
        self.starting_home = Some(turn);
    }

    pub fn home_turn(&self) -> Option<&str> {
        self.starting_home.as_deref()
    }

    pub fn retire_home_turn(&mut self) -> Option<String> {
        self.live.remove(&Target::Home);
        self.starting_home.take()
    }

    pub fn latest_home_turn(&self) -> Option<&str> {
        self.latest_home_turn.as_deref()
    }

    pub fn home_turn_started(&mut self, turn: &str, execution: Option<Execution>) {
        if self.starting_home.as_deref() != Some(turn) {
            return;
        }
        if let Some(execution) = execution {
            // Keep the turn identity for subsequent tagged agent frames.
            self.live.insert(Target::Home, execution);
        }
    }

    /// Clearing Home retires its turn owner so late frames cannot revive it.
    pub fn set_execution(&mut self, target: Target, execution: Option<Execution>) {
        if target.is_home() && execution.is_none() {
            self.retire_home_turn();
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

    pub fn submit(&mut self, target: &Target, input: &InputBuffer) -> Submission {
        if input.is_empty() {
            return Submission::Blocked("empty draft");
        }
        let Some(execution) = self.live.get(target) else {
            return if target.is_home() && self.starting_home.is_none() {
                Submission::HomePrompt
            } else {
                Submission::Blocked("this conversation has no writable execution")
            };
        };
        if self
            .pending
            .values()
            .any(|pending| &pending.execution == execution)
        {
            return Submission::Blocked("delivery is pending for this execution");
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
        Submission::Correction {
            id,
            execution,
            text,
        }
    }

    /// Retire this generation's receipts before exposing the replacement session.
    pub fn retire_home_session(&mut self) {
        self.latest_home_turn = None;
        self.pending.retain(|_, pending| !pending.target.is_home());
        self.retire_home_turn();
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

    fn stage() -> Target {
        Target::StageExecution {
            run_id: "runner".into(),
            stage: "build".into(),
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
        let Submission::Correction {
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
    fn home_routes_from_current_start_and_live_state() {
        let mut state = Conversations::default();
        let text = input("hello");
        assert_eq!(state.submit(&Target::Home, &text), Submission::HomePrompt);
        state.begin_home_turn("turn".into());
        assert!(matches!(
            state.submit(&Target::Home, &text),
            Submission::Blocked(_)
        ));
        state.home_turn_started("other", Some(execution(1)));
        assert!(matches!(
            state.submit(&Target::Home, &text),
            Submission::Blocked(_)
        ));
        state.home_turn_started("turn", Some(execution(2)));
        state.home_turn_started("turn", None);
        let id = correction(&mut state, &Target::Home, &text, &execution(2));
        state.set_execution(Target::Home, None);
        state.home_turn_started("turn", Some(execution(2)));
        assert_eq!(state.submit(&Target::Home, &text), Submission::HomePrompt);
        assert!(state.finish(id, Ok(())).is_some());

        state.begin_home_turn("B".into());
        state.home_turn_started("B", Some(execution(3)));
        state.home_turn_started("turn", Some(execution(2)));
        correction(&mut state, &Target::Home, &text, &execution(3));
    }

    #[test]
    fn pending_blocks_same_execution_across_targets_and_failure_allows_retry() {
        let mut state = Conversations::default();
        let text = input("edit");
        state.set_execution(stage(), Some(execution(1)));
        state.set_execution(Target::Home, Some(execution(1)));
        let id = correction(&mut state, &stage(), &text, &execution(1));
        assert!(matches!(
            state.submit(&stage(), &text),
            Submission::Blocked(_)
        ));
        assert!(matches!(
            state.submit(&Target::Home, &text),
            Submission::Blocked(_)
        ));
        state.set_execution(Target::Home, Some(execution(2)));
        let home = correction(&mut state, &Target::Home, &text, &execution(2));
        let receipts = state.fail_pending(&stage(), "busy");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].result, Err("busy".into()));
        assert!(state.finish(id, Ok(())).is_none());
        correction(&mut state, &stage(), &text, &execution(1));
        assert!(state.finish(home, Ok(())).is_some());
    }

    #[test]
    fn retirement_is_scoped_and_ids_are_not_reused() {
        let mut state = Conversations::default();
        let text = input("edit");
        state.set_execution(stage(), Some(execution(1)));
        state.begin_home_turn("turn".into());
        state.home_turn_started("turn", Some(execution(2)));
        let stage_id = correction(&mut state, &stage(), &text, &execution(1));
        let home_id = correction(&mut state, &Target::Home, &text, &execution(2));
        state.retire_home_session();
        assert!(state.latest_home_turn().is_none());
        assert!(state.finish(home_id, Ok(())).is_none());
        let receipt = state.finish(stage_id, Ok(())).unwrap();
        assert_eq!(receipt.target, stage());
        assert_eq!(receipt.result, Ok(()));

        let detached_id = correction(&mut state, &stage(), &text, &execution(1));
        state.begin_home_turn("next-turn".into());
        state.home_turn_started("next-turn", Some(execution(2)));
        let concurrent_id = correction(&mut state, &Target::Home, &text, &execution(2));
        state.retire_pending(&stage());
        let next = correction(&mut state, &stage(), &text, &execution(1));
        for result in [Ok(()), Err("late failure".into())] {
            assert!(state.finish(detached_id, result).is_none());
            assert_eq!(
                state.submit(&stage(), &text),
                Submission::Blocked("delivery is pending for this execution")
            );
        }
        assert!(state.finish(next, Ok(())).is_some());
        correction(&mut state, &stage(), &text, &execution(1));
        let receipt = state.finish(concurrent_id, Ok(())).unwrap();
        assert_eq!(receipt.target, Target::Home);
        assert_eq!(receipt.result, Ok(()));
        assert!(detached_id > home_id && home_id > stage_id);
        assert!(next > concurrent_id && concurrent_id > detached_id);
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
            Submission::Blocked(_)
        ));
        state.set_execution(target.clone(), Some(execution(1)));
        assert!(matches!(
            state.submit(&target, &input("")),
            Submission::Blocked(_)
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
                Submission::Blocked("this conversation has no writable execution")
            );
            let receipt = state.finish(id, Ok(())).unwrap();
            assert_eq!(receipt.target, target);
            assert_eq!(receipt.text, "edit [execution runner/stage-1 nonce 1]");
            assert_eq!(receipt.result, Ok(()));
            assert_eq!(
                state.submit(&target, &input("new edit")),
                Submission::Blocked("this conversation has no writable execution")
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
