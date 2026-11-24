//! Bind operator input when it is composed, not when Enter is pressed.
//! Target names survive repeated executions; stage_run_id values do not.

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

/// The caller renders the result only in `target`'s transcript. On failure
/// `text` remains recoverable even if a newer draft has replaced this one.
pub(crate) struct Receipt {
    pub target: Target,
    pub text: String,
    pub clear_draft: bool,
    pub result: Result<(), String>,
}

#[derive(Clone, PartialEq, Eq)]
enum Address {
    IdleHome,
    StartingHome(String),
    Execution(Execution),
    Unavailable,
}

#[derive(Clone)]
struct Draft {
    address: Address,
    revision: u64,
}

struct Pending {
    target: Target,
    // Keep the exact addressed snapshot even if the composer is replaced.
    revision: u64,
    buffer: InputBuffer,
}

#[derive(Default)]
pub(crate) struct Conversations {
    starting_home: Option<String>,
    live: HashMap<Target, Execution>,
    drafts: HashMap<Target, Draft>,
    pending: HashMap<u64, Pending>,
    sequence: u64,
}

impl Conversations {
    pub fn begin_home_turn(&mut self, turn: String) {
        self.live.remove(&Target::Home);
        self.starting_home = Some(turn);
    }

    pub fn home_turn_started(&mut self, turn: &str, execution: Option<Execution>) {
        let starting_draft = self.drafts.get_mut(&Target::Home).filter(|draft| {
            self.starting_home.as_deref() == Some(turn)
                && matches!(&draft.address, Address::StartingHome(id) if id == turn)
        });
        if let (Some(draft), Some(execution)) = (starting_draft, &execution) {
            draft.address = Address::Execution(execution.clone());
        }
        self.set_execution(Target::Home, execution);
    }

    pub fn set_execution(&mut self, target: Target, execution: Option<Execution>) {
        if target.is_home() {
            self.starting_home = None;
        }
        if let Some(execution) = execution {
            self.live.insert(target, execution);
        } else {
            self.live.remove(&target);
        }
        // Never rebind an existing draft to the replacement execution.
    }

    pub fn is_correction_draft(&self, target: &Target) -> bool {
        self.drafts
            .get(target)
            .is_some_and(|draft| draft.address != Address::IdleHome)
    }

    pub fn dismiss(&mut self, target: &Target) {
        self.drafts.remove(target);
    }

    pub fn retain_target(&self, target: &Target) -> bool {
        self.drafts.contains_key(target) || self.pending.values().any(|p| &p.target == target)
    }

    fn address(&self, target: &Target, home_busy: bool) -> Address {
        if let Some(execution) = self.live.get(target) {
            Address::Execution(execution.clone())
        } else if target.is_home() {
            match &self.starting_home {
                Some(turn) => Address::StartingHome(turn.clone()),
                None if !home_busy => Address::IdleHome,
                None => Address::Unavailable,
            }
        } else {
            Address::Unavailable
        }
    }

    /// Call after edits, including explicit dismissal, and before lifecycle
    /// changes. Clearing the input intentionally releases its old address.
    pub fn observe_draft(&mut self, target: &Target, input: &InputBuffer, home_busy: bool) {
        if input.is_empty() {
            self.drafts.remove(target);
        } else if let Some(draft) = self.drafts.get_mut(target) {
            self.sequence += 1;
            draft.revision = self.sequence;
        } else {
            self.sequence += 1;
            self.drafts.insert(
                target.clone(),
                Draft {
                    address: self.address(target, home_busy),
                    revision: self.sequence,
                },
            );
        }
    }

    pub fn submit(&mut self, target: &Target, input: &InputBuffer, home_busy: bool) -> Submission {
        if input.is_empty() || !self.drafts.contains_key(target) {
            self.observe_draft(target, input, home_busy);
        }
        let Some(draft) = self.drafts.get(target) else {
            return Submission::Blocked("empty draft");
        };
        if self.pending.values().any(|p| &p.target == target) {
            return Submission::Blocked("delivery is pending for this conversation");
        }
        if draft.address != self.address(target, home_busy) {
            return Submission::Blocked(
                "the addressed execution has changed; dismiss this draft to start another",
            );
        }
        match &draft.address {
            Address::StartingHome(_) | Address::Unavailable => {
                Submission::Blocked("this conversation has no writable execution")
            }
            Address::IdleHome => Submission::HomePrompt,
            Address::Execution(execution) => {
                self.sequence += 1;
                let id = self.sequence;
                let text = input.clone().resolve();
                self.pending.insert(
                    id,
                    Pending {
                        target: target.clone(),
                        revision: draft.revision,
                        buffer: input.clone(),
                    },
                );
                Submission::Correction {
                    id,
                    execution: execution.clone(),
                    text,
                }
            }
        }
    }

    /// Retire this generation's receipts before exposing the replacement session.
    /// Submission IDs are monotonic, so a late result can never match a new draft.
    pub fn retire_home_session(&mut self) {
        self.pending.retain(|_, pending| !pending.target.is_home());
        self.drafts.remove(&Target::Home);
        self.set_execution(Target::Home, None);
    }

    /// Call on detach to ignore late receipts without discarding the draft.
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
        let clear_draft = result.is_ok()
            && self
                .drafts
                .get(&pending.target)
                .is_some_and(|draft| draft.revision == pending.revision);
        if clear_draft {
            self.drafts.remove(&pending.target);
        }
        Some(Receipt {
            target: pending.target,
            text: pending.buffer.text().to_owned(),
            clear_draft,
            result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution(run: &str, nonce: u64) -> Execution {
        Execution {
            run_id: run.into(),
            stage_run_id: format!("{run}-{nonce}"),
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
        let mut buf = InputBuffer::new();
        buf.replace_all(text);
        buf
    }

    fn correction(submission: Submission, expected: &Execution, expected_text: &str) -> u64 {
        let Submission::Correction {
            id,
            execution,
            text,
        } = submission
        else {
            panic!("unexpected route: {submission:?}");
        };
        assert_eq!(&execution, expected);
        assert_eq!(text, expected_text);
        id
    }

    #[test]
    fn correction_resolves_substitutions_but_receipt_preserves_display_text() {
        let mut state = Conversations::default();
        let exec = execution("home", 1);
        state.set_execution(Target::Home, Some(exec.clone()));
        let mut draft = input("fix src");
        draft.add_substitution(4..7, "/work/src".into());
        let id = correction(
            state.submit(&Target::Home, &draft, true),
            &exec,
            "fix /work/src",
        );
        assert_eq!(state.finish(id, Ok(())).unwrap().text, "fix src");
    }

    #[test]
    fn reset_retires_only_home_receipts() {
        let mut state = Conversations::default();
        let exec = execution("run", 2);
        let send = |state: &mut Conversations, target: Target| {
            state.set_execution(target.clone(), Some(exec.clone()));
            correction(state.submit(&target, &input("hello"), true), &exec, "hello")
        };
        let home = send(&mut state, Target::Home);
        let stage = send(&mut state, stage());
        state.retire_home_session();
        assert!(state.finish(home, Err("late".into())).is_none());
        assert!(state.finish(stage, Ok(())).is_some());
    }

    #[test]
    fn detach_retires_pending_but_preserves_draft_and_allows_new_submission() {
        for target in [Target::Home, stage()] {
            let mut state = Conversations::default();
            let old = execution("old", 1);
            let next = execution("next", 1);
            state.set_execution(target.clone(), Some(old.clone()));
            let id = correction(state.submit(&target, &input("old"), true), &old, "old");
            let other = if target.is_home() {
                stage()
            } else {
                Target::Home
            };
            state.set_execution(other.clone(), Some(old.clone()));
            let other_id = correction(state.submit(&other, &input("other"), true), &old, "other");

            state.retire_pending(&target);
            state.set_execution(target.clone(), None);
            assert!(state.retain_target(&target));
            assert!(state.drafts.contains_key(&target));
            state.set_execution(target.clone(), Some(next.clone()));
            assert!(matches!(
                state.submit(&target, &input("old"), true),
                Submission::Blocked(_)
            ));

            state.observe_draft(&target, &InputBuffer::new(), true);
            let new_id = correction(state.submit(&target, &input("new"), true), &next, "new");
            assert_ne!(id, new_id);
            assert!(state.finish(id, Ok(())).is_none());
            assert!(state.finish(id, Err("late".into())).is_none());
            assert!(state.drafts.contains_key(&target));
            assert!(state.finish(new_id, Ok(())).unwrap().clear_draft);
            assert!(state.finish(other_id, Ok(())).is_some());
        }
    }

    #[test]
    fn detached_execution_can_retry_without_accepting_its_retired_receipt() {
        for target in [Target::Home, stage()] {
            let mut state = Conversations::default();
            let exec = execution("run", 1);
            let draft = input("retry this correction");
            state.set_execution(target.clone(), Some(exec.clone()));
            let retired = correction(state.submit(&target, &draft, true), &exec, draft.text());

            state.retire_pending(&target);
            state.set_execution(target.clone(), None);
            assert!(matches!(
                state.submit(&target, &draft, true),
                Submission::Blocked(_)
            ));
            assert!(state.retain_target(&target));

            state.set_execution(target.clone(), Some(exec.clone()));
            assert!(
                state.pending.is_empty(),
                "reselection must not send automatically"
            );
            let retry = correction(state.submit(&target, &draft, true), &exec, draft.text());
            assert_ne!(retired, retry);
            for result in [Ok(()), Err("late failure".into())] {
                assert!(state.finish(retired, result).is_none());
                assert!(state.pending.contains_key(&retry));
                assert!(state.drafts.contains_key(&target));
                assert!(matches!(
                    state.submit(&target, &draft, true),
                    Submission::Blocked(_)
                ));
            }

            let receipt = state.finish(retry, Ok(())).unwrap();
            assert_eq!(receipt.target, target);
            assert_eq!(receipt.text, draft.text());
            assert_eq!(receipt.result, Ok(()));
            assert!(receipt.clear_draft);
            assert!(!state.retain_target(&target));
            assert!(state.pending.is_empty());
        }
    }

    #[test]
    fn command_error_without_request_id_unlocks_only_target_pending() {
        let mut state = Conversations::default();
        let live = execution("run", 1);
        state.set_execution(stage(), Some(live.clone()));
        state.set_execution(Target::Home, Some(live.clone()));
        let id = correction(
            state.submit(&stage(), &input("original"), true),
            &live,
            "original",
        );
        let home_id = correction(
            state.submit(&Target::Home, &input("home"), true),
            &live,
            "home",
        );
        state.observe_draft(&stage(), &input("new draft"), true);

        let receipts = state.fail_pending(&stage(), "command failed");
        assert_eq!(receipts.len(), 1);
        let receipt = &receipts[0];
        assert_eq!(receipt.target, stage());
        assert_eq!(receipt.text, "original");
        assert_eq!(receipt.result, Err("command failed".into()));
        assert!(!receipt.clear_draft);
        assert!(state.drafts.contains_key(&stage()));
        assert!(state.finish(id, Ok(())).is_none());
        assert!(state.fail_pending(&stage(), "again").is_empty());
        correction(
            state.submit(&stage(), &input("new draft"), true),
            &live,
            "new draft",
        );
        assert!(state.finish(home_id, Ok(())).is_some());
    }

    #[test]
    fn simultaneous_live_conversations_have_independent_submissions_and_receipts() {
        let mut state = Conversations::default();
        let prompt = input("new turn");
        assert_eq!(
            state.submit(&Target::Home, &prompt, false),
            Submission::HomePrompt
        );
        state.observe_draft(&Target::Home, &InputBuffer::new(), true);
        let home = execution("session", 1);
        let job = execution("runner", 2);
        state.set_execution(Target::Home, Some(home.clone()));
        state.set_execution(stage(), Some(job.clone()));
        let id = correction(
            state.submit(&Target::Home, &input("change course"), true),
            &home,
            "change course",
        );
        let job_id = correction(
            state.submit(&stage(), &input("stop building"), true),
            &job,
            "stop building",
        );
        assert!(matches!(
            state.submit(&stage(), &input("stop building"), true),
            Submission::Blocked(_)
        ));
        let ok = state.finish(job_id, Ok(())).unwrap();
        assert_eq!(ok.target, stage());
        assert_eq!(ok.text, "stop building");
        assert!(ok.clear_draft);
        assert!(state.finish(job_id, Ok(())).is_none());
        let rejected = state.finish(id, Err("read-only".into())).unwrap();
        assert_eq!(rejected.target, Target::Home);
        assert_eq!(rejected.result.unwrap_err(), "read-only");
        assert!(!rejected.clear_draft);
    }

    #[test]
    fn correction_composed_during_home_start_becomes_writable_for_that_turn() {
        let mut state = Conversations::default();
        let draft = input("change course");
        let home = execution("session", 1);
        state.begin_home_turn("1".into());
        state.observe_draft(&Target::Home, &draft, true);
        assert!(matches!(
            state.submit(&Target::Home, &draft, true),
            Submission::Blocked(_)
        ));
        state.home_turn_started("1", Some(home.clone()));
        correction(
            state.submit(&Target::Home, &draft, true),
            &home,
            draft.text(),
        );
    }

    #[test]
    fn successor_cannot_inherit_a_correction_for_a_turn_that_never_started() {
        let mut state = Conversations::default();
        let draft = input("only for the first turn");
        state.begin_home_turn("1".into());
        state.observe_draft(&Target::Home, &draft, true);
        state.set_execution(Target::Home, None);
        state.begin_home_turn("2".into());
        state.home_turn_started("2", Some(execution("session", 2)));
        assert!(matches!(
            state.submit(&Target::Home, &draft, true),
            Submission::Blocked(_)
        ));
    }

    #[test]
    fn ended_execution_stays_addressed_until_explicit_dismissal() {
        for target in [Target::Home, stage()] {
            let mut state = Conversations::default();
            let draft = input("not for the next turn");
            let old = execution("run", 1);
            let next = Execution {
                stage_run_id: "successor".into(),
                ..old.clone()
            };
            state.set_execution(target.clone(), Some(old.clone()));
            state.observe_draft(&target, &draft, true);
            state.set_execution(target.clone(), None);
            assert!(matches!(
                state.submit(&target, &draft, false),
                Submission::Blocked(_)
            ));
            // Only the stage_run_id changes within the same run and nonce.
            state.set_execution(target.clone(), Some(next));
            assert!(state.retain_target(&target));
            assert!(state.drafts.contains_key(&target));
            assert!(matches!(
                state.submit(&target, &draft, true),
                Submission::Blocked(_)
            ));

            assert!(state.pending.is_empty());
            // The original draft is still recoverable on its own execution.
            state.set_execution(target.clone(), Some(old.clone()));
            let recovered = correction(state.submit(&target, &draft, true), &old, draft.text());
            assert_eq!(
                state
                    .finish(recovered, Err("not delivered".into()))
                    .unwrap()
                    .text,
                draft.text()
            );
            assert!(state.drafts.contains_key(&target));

            state.set_execution(target.clone(), None);
            state.observe_draft(&target, &InputBuffer::new(), false);
            let route = state.submit(&target, &input("start again"), false);
            if target.is_home() {
                assert_eq!(route, Submission::HomePrompt);
            } else {
                assert!(matches!(route, Submission::Blocked(_)));
            }
        }
    }

    #[test]
    fn acknowledgment_never_clears_retyped_text_even_when_identical() {
        let mut state = Conversations::default();
        let live = execution("session", 7);
        state.set_execution(Target::Home, Some(live.clone()));
        let text = input("again");
        let job = execution("runner", 7);
        state.set_execution(stage(), Some(job.clone()));
        let job_id = correction(
            state.submit(&stage(), &input("stage draft"), true),
            &job,
            "stage draft",
        );
        let id = correction(state.submit(&Target::Home, &text, true), &live, "again");
        state.observe_draft(&Target::Home, &InputBuffer::new(), true);
        assert!(
            state.retain_target(&Target::Home),
            "receipt still outstanding"
        );
        state.observe_draft(&Target::Home, &text, true);
        let receipt = state.finish(id, Ok(())).unwrap();
        assert_eq!(receipt.target, Target::Home);
        assert!(!receipt.clear_draft);
        assert!(state.drafts.contains_key(&Target::Home));
        assert!(state.drafts.contains_key(&stage()));
        assert!(state.pending.contains_key(&job_id));
        assert!(state.finish(job_id, Ok(())).unwrap().clear_draft);
        assert!(state.drafts.contains_key(&Target::Home));
    }

    #[test]
    fn uncertain_delivery_retains_original_and_new_drafts_without_resubmitting() {
        let mut state = Conversations::default();
        let live = execution("runner", 4);
        state.set_execution(stage(), Some(live.clone()));
        let id = correction(state.submit(&stage(), &input("one"), true), &live, "one");
        state.observe_draft(&stage(), &input("two"), true);
        state.observe_draft(&Target::Home, &input("unrelated"), false);
        let receipt = state
            .finish(id, Err("disconnected; delivery unknown".into()))
            .unwrap();
        assert_eq!(receipt.target, stage());
        assert_eq!(receipt.text, "one");
        assert!(!receipt.clear_draft);

        assert!(state.pending.is_empty());
    }
}
