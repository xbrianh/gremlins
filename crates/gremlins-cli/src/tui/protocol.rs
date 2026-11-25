//! Wire identities and selectors for conversation-scoped attachments.

use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Invocation {
    pub run_id: String,
    pub stage: String,
    pub stage_type: String,
    pub stage_run_id: String,
}

pub(super) fn invocation_from_frame(frame: &Value) -> Option<Invocation> {
    let field = |key: &str| {
        frame[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(Invocation {
        run_id: field("run_id").or_else(|| field("id"))?,
        stage: field("correlation_stage").or_else(|| field("stage"))?,
        stage_type: field("stage_type")?,
        stage_run_id: field("stage_run_id")?,
    })
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct Execution {
    pub run_id: String,
    pub stage_run_id: String,
    // Zero denotes an unpinned native bus identity; never send it as a nonce.
    pub nonce: u64,
}

impl Execution {
    /// Native watch identity, independent of debugger attachment metadata.
    pub(super) fn from_bus_frame(frame: &Value) -> Option<Self> {
        let field = |key: &str| frame[key].as_str().filter(|s| !s.trim().is_empty());
        Some(Self {
            run_id: field("run_id")?.to_owned(),
            stage_run_id: field("stage_run_id")?.to_owned(),
            nonce: 0,
        })
    }

    pub(super) fn input_request(&self, session: &str, req_id: &str) -> Value {
        let mut request = json!({"op":"input", "session_id":session,
            "id":self.run_id, "stage_run_id":self.stage_run_id, "req_id":req_id});
        if self.nonce != 0 {
            request["nonce"] = json!(self.nonce);
        }
        request
    }

    pub(super) fn from_frame(frame: &Value) -> Option<Self> {
        Some(Self {
            run_id: frame["id"].as_str()?.to_owned(),
            stage_run_id: frame["stage_run_id"]
                .as_str()
                .filter(|s| !s.is_empty())?
                .to_owned(),
            nonce: frame["nonce"].as_u64()?,
        })
    }
}

pub(super) enum Selector<'a> {
    Run { run: &'a str },
    Stage { run: &'a str, stage: &'a str },
    Exact(&'a Execution),
}

impl Selector<'_> {
    pub(super) fn request(&self, session: &str) -> Value {
        let mut req =
            json!({"op":"attach", "scope":"conversation", "mode":"control", "session_id":session});
        match self {
            Self::Run { run } => req["id"] = json!(run),
            Self::Stage { run, stage } => {
                req["id"] = json!(run);
                req["stage"] = json!(stage);
            }
            Self::Exact(execution) => {
                req["id"] = json!(execution.run_id);
                req["stage_run_id"] = json!(execution.stage_run_id);
                req["nonce"] = json!(execution.nonce);
            }
        }
        req
    }

    pub(super) fn binding(&self, ack: &Value) -> Result<Execution, String> {
        let execution = self.observed_binding(ack)?;
        if !ack["capabilities"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "message"))
        {
            return Err("addressed execution is read-only".into());
        }
        Ok(execution)
    }

    pub(super) fn observed_binding(&self, ack: &Value) -> Result<Execution, String> {
        if ack["type"] == "error" {
            return Err(ack["message"]
                .as_str()
                .unwrap_or("attachment rejected")
                .into());
        }
        if ack["type"] != "attached" || ack["scope"] != "conversation" {
            return Err("not attached to a conversation".into());
        }
        let execution = Execution::from_frame(ack).ok_or("attachment has no stage_run_id")?;
        let matches = match self {
            Self::Run { run } => execution.run_id == *run,
            Self::Stage { run, stage } => execution.run_id == *run && ack["stage"] == *stage,
            Self::Exact(expected) => &execution == *expected,
        };
        if !matches {
            return Err("not attached to the addressed execution".into());
        }
        Ok(execution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_bus_identity_needs_no_debugger_metadata() {
        let frame = json!({"type":"stream_chunk", "run_id":"home", "stage":"chat",
            "stage_run_id":"invocation", "sequence":12, "text":"hello"});
        let execution = Execution::from_bus_frame(&frame).unwrap();
        assert_eq!(
            execution.input_request("owner", "7"),
            json!({"op":"input",
            "session_id":"owner", "id":"home", "stage_run_id":"invocation", "req_id":"7"})
        );
        assert!(Execution::from_frame(&frame).is_none());
        for key in ["run_id", "stage_run_id"] {
            for value in [Value::Null, json!(""), json!(" "), json!(9)] {
                let mut invalid = frame.clone();
                invalid[key] = value;
                assert!(Execution::from_bus_frame(&invalid).is_none());
            }
        }
        let mut pinned = execution;
        pinned.nonce = 9;
        assert_eq!(pinned.input_request("owner", "7")["nonce"], 9);
    }

    #[test]
    fn exact_selector_sends_and_compares_advertised_stage_run_id() {
        let ack: Value = serde_json::from_str(
            r#"{"type":"attached","scope":"conversation","id":"run","stage_run_id":"advertised","nonce":5,"capabilities":["message"]}"#,
        ).unwrap();
        let bound = Selector::Run { run: "run" }.binding(&ack).unwrap();
        assert_eq!(bound.stage_run_id, "advertised");
        let exact = Selector::Exact(&bound);
        assert_eq!(
            exact.request("owner"),
            json!({"op":"attach","scope":"conversation","mode":"control","session_id":"owner","id":"run","stage_run_id":"advertised","nonce":5})
        );
        assert_eq!(exact.binding(&ack).unwrap(), bound);
        for (key, value) in [
            ("stage_run_id", json!("replacement")),
            ("nonce", json!(6)),
            ("capabilities", json!(["observe"])),
        ] {
            let mut changed = ack.clone();
            changed[key] = value;
            assert!(exact.binding(&changed).is_err());
        }
        let mut missing = ack;
        missing.as_object_mut().unwrap().remove("stage_run_id");
        assert!(Execution::from_frame(&missing).is_none());
    }

    #[test]
    fn binds_only_the_requested_writable_stage() {
        let ack = json!({"type":"attached", "scope":"conversation", "id":"run", "stage":"review", "stage_run_id":"A", "nonce":5, "capabilities":["message"]});
        let selector = Selector::Stage {
            run: "run",
            stage: "review",
        };
        assert_eq!(
            selector.request("owner"),
            json!({"op":"attach", "mode":"control", "scope":"conversation", "id":"run", "stage":"review", "session_id":"owner"})
        );
        assert_eq!(selector.binding(&ack).unwrap().stage_run_id, "A");
        for (key, other) in [
            ("id", json!("other")),
            ("stage", json!("successor")),
            ("nonce", Value::Null),
            ("stage_run_id", json!("")),
            ("capabilities", json!(["observe"])),
        ] {
            let mut wrong = ack.clone();
            wrong[key] = other;
            assert!(selector.binding(&wrong).is_err());
        }
    }
}
