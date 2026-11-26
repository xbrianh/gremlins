//! Native bus identities and exact execution input requests.

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
}
