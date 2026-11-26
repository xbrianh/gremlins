//! Synchronous wire decoding, independent of execution input delivery.
use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Descriptor<'a> {
    pub run_id: &'a str,
    pub stage_run_id: &'a str,
    pub stage: Option<&'a str>,
    pub nonce: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command<'a> {
    Message { req_id: &'a str, text: &'a str },
    Pause { req_id: &'a str },
    Step { req_id: &'a str },
    Resume { req_id: &'a str },
    Bail { req_id: &'a str },
}

pub(super) fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("missing or invalid '{key}'"))
}

pub(super) fn retired_target_message(request: &Value) -> Option<String> {
    for key in ["stage_id", "execution_id"] {
        if request.get(key).is_some() {
            return Some(format!("retired '{key}' field; use 'stage_run_id'"));
        }
        if request
            .get("target")
            .and_then(|target| target.get(key))
            .is_some()
        {
            return Some(format!("target.{key} is retired; use target.stage_run_id"));
        }
    }
    None
}

pub(super) fn descriptor(
    request: &Value,
    validate_id: impl FnOnce(&str) -> Result<(), String>,
) -> Result<Descriptor<'_>, String> {
    if let Some(message) = retired_target_message(request) {
        return Err(message);
    }
    let fields = request
        .as_object()
        .ok_or("input request must be an object")?;
    for key in fields.keys() {
        if !matches!(
            key.as_str(),
            "op" | "session_id" | "req_id" | "target" | "id" | "stage" | "stage_run_id" | "nonce"
        ) && !(key == "text" && request.get("op").and_then(Value::as_str) == Some("chat"))
        {
            return Err(format!("unsupported input field '{key}'"));
        }
    }
    let target = request.get("target").unwrap_or(request);
    if !target.is_object() || target.get("target").is_some() {
        return Err("target must be a flat descriptor object".into());
    }
    if request.get("target").is_some() {
        for key in ["id", "stage", "stage_run_id", "nonce"] {
            if request.get(key).is_some() {
                return Err(format!("'{key}' conflicts with target descriptor"));
            }
        }
        for key in target.as_object().expect("validated descriptor").keys() {
            if !matches!(key.as_str(), "id" | "stage" | "stage_run_id" | "nonce") {
                return Err(format!("unsupported input target field '{key}'"));
            }
        }
    }
    let nonce = target
        .get("nonce")
        .map(|value| {
            value
                .as_u64()
                .filter(|nonce| *nonce != 0)
                .ok_or_else(|| "missing or invalid 'nonce'".to_string())
        })
        .transpose()?;
    let run_id = required(target, "id")?;
    validate_id(run_id)?;
    let stage_run_id = required(target, "stage_run_id")?;
    let stage = if target.get("stage").is_some() {
        Some(required(target, "stage")?)
    } else {
        None
    };
    Ok(Descriptor {
        run_id,
        stage_run_id,
        stage,
        nonce,
    })
}

pub(super) fn command(request: &Value) -> Result<Command<'_>, String> {
    if let Some(message) = retired_target_message(request) {
        return Err(message);
    }
    for key in ["target", "id", "stage", "stage_run_id", "nonce"] {
        if request.get(key).is_some() {
            return Err(format!("'{key}' is not allowed on a bound input command"));
        }
    }
    let req_id = required(request, "req_id")?;
    Ok(match required(request, "cmd")? {
        "message" => Command::Message {
            req_id,
            text: required(request, "text")?,
        },
        "pause" => Command::Pause { req_id },
        "step" => Command::Step { req_id },
        "resume" => Command::Resume { req_id },
        "bail" => Command::Bail { req_id },
        _ => return Err("unknown input command".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(request: &Value) -> Result<Descriptor<'_>, String> {
        descriptor(request, |_| Ok(()))
    }

    #[test]
    fn inline_and_nested_descriptors_are_identical_and_borrow_original_strings() {
        for stage in [None, Some(" build ")] {
            for nonce in [None, Some(1), Some(u64::MAX)] {
                let mut target = json!({"id":" run ", "stage_run_id":" invocation "});
                if let Some(stage) = stage {
                    target["stage"] = json!(stage);
                }
                if let Some(nonce) = nonce {
                    target["nonce"] = json!(nonce);
                }
                let nested =
                    json!({"target":target, "op":"input", "session_id":null, "req_id":false});
                let expected = Descriptor {
                    run_id: " run ",
                    stage_run_id: " invocation ",
                    stage,
                    nonce,
                };
                assert_eq!(decode(&target).unwrap(), expected);
                let actual = decode(&nested).unwrap();
                assert_eq!(actual, expected);
                assert!(std::ptr::eq(
                    actual.run_id,
                    nested["target"]["id"].as_str().unwrap()
                ));
            }
        }
        assert!(decode(&json!({"op":"chat", "text":null, "id":"run", "stage_run_id":"s"})).is_ok());
    }

    #[test]
    fn descriptor_error_table() {
        for (request, error) in [
            (json!(null), "input request must be an object"),
            (json!([]), "input request must be an object"),
            (json!({"extra":null}), "unsupported input field 'extra'"),
            (
                json!({"op":"input", "text":"x"}),
                "unsupported input field 'text'",
            ),
            (
                json!({"target":null}),
                "target must be a flat descriptor object",
            ),
            (
                json!({"target":{"target":null}, "id":null}),
                "target must be a flat descriptor object",
            ),
            (
                json!({"target":{"extra":null}}),
                "unsupported input target field 'extra'",
            ),
            (json!({}), "missing or invalid 'id'"),
            (json!({"id":"run"}), "missing or invalid 'stage_run_id'"),
            (
                json!({"id":"run", "stage_run_id":"s", "stage":null}),
                "missing or invalid 'stage'",
            ),
        ] {
            assert_eq!(decode(&request).unwrap_err(), error, "{request}");
        }
        for key in ["id", "stage", "stage_run_id", "nonce"] {
            let mut request = json!({"target":{"extra":true}});
            request[key] = Value::Null;
            assert_eq!(
                decode(&request).unwrap_err(),
                format!("'{key}' conflicts with target descriptor")
            );
        }
        for value in [
            Value::Null,
            json!(false),
            json!("1"),
            json!(0),
            json!(-1),
            json!(1.5),
        ] {
            assert_eq!(
                decode(&json!({"nonce":value})).unwrap_err(),
                "missing or invalid 'nonce'"
            );
        }
        for key in ["id", "stage_run_id", "stage"] {
            for value in [
                Value::Null,
                json!(false),
                json!(3),
                json!(""),
                json!(" \t\n "),
                json!([]),
                json!({}),
            ] {
                let mut request = json!({"id":"run", "stage_run_id":"s"});
                request[key] = value;
                assert_eq!(
                    decode(&request).unwrap_err(),
                    format!("missing or invalid '{key}'")
                );
            }
        }
    }

    #[test]
    fn validator_diagnostic_and_precedence_are_preserved() {
        let request = json!({"id":" invalid ", "stage":null});
        let error = descriptor(&request, |id| {
            assert_eq!(id, " invalid ");
            Err("canonical validator diagnostic".into())
        })
        .unwrap_err();
        assert_eq!(error, "canonical validator diagnostic");
        for request in [json!({"nonce":0, "id":"invalid"}), json!({"id":null})] {
            assert!(descriptor(&request, |_| panic!(
                "validator called before field validation"
            ))
            .is_err());
        }
        assert_eq!(
            decode(&json!({"id":"run", "stage_run_id":null, "stage":null})).unwrap_err(),
            "missing or invalid 'stage_run_id'"
        );
    }

    #[test]
    fn retired_fields_precede_every_other_error() {
        for key in ["stage_id", "execution_id"] {
            let mut inline = json!({"target":false, "extra":true});
            inline[key] = Value::Null;
            let mut nested = json!({"target":{}, "extra":true});
            nested["target"][key] = Value::Null;
            for (request, expected) in [
                (inline, format!("retired '{key}' field; use 'stage_run_id'")),
                (
                    nested,
                    format!("target.{key} is retired; use target.stage_run_id"),
                ),
            ] {
                assert_eq!(retired_target_message(&request), Some(expected.clone()));
                assert_eq!(decode(&request).unwrap_err(), expected);
                assert_eq!(command(&request).unwrap_err(), expected);
            }
        }
        let request = json!({"execution_id":null, "target":{"stage_id":null}});
        assert_eq!(
            retired_target_message(&request).unwrap(),
            "target.stage_id is retired; use target.stage_run_id"
        );
        let request = json!({"stage_id":null, "target":{"stage_id":null}});
        assert_eq!(
            retired_target_message(&request).unwrap(),
            "retired 'stage_id' field; use 'stage_run_id'"
        );
        assert_eq!(retired_target_message(&json!(null)), None);
    }

    #[test]
    fn commands_preserve_padding_and_ignore_extras() {
        let request = json!({"cmd":"message", "req_id":" r ", "text":" fix \n", "extra":true});
        assert_eq!(
            command(&request).unwrap(),
            Command::Message {
                req_id: " r ",
                text: " fix \n"
            }
        );
        for (cmd, expected) in [
            ("pause", Command::Pause { req_id: " r " }),
            ("step", Command::Step { req_id: " r " }),
            ("resume", Command::Resume { req_id: " r " }),
            ("bail", Command::Bail { req_id: " r " }),
        ] {
            assert_eq!(
                command(&json!({"cmd":cmd, "req_id":" r ", "text":null, "extra":true})).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn command_error_table_and_selector_precedence() {
        for (request, expected) in [
            (json!(null), "missing or invalid 'req_id'"),
            (json!([]), "missing or invalid 'req_id'"),
            (json!({}), "missing or invalid 'req_id'"),
            (json!({"req_id":"r"}), "missing or invalid 'cmd'"),
            (
                json!({"req_id":"r", "cmd":"detach"}),
                "unknown input command",
            ),
            (
                json!({"req_id":"r", "cmd":" pause "}),
                "unknown input command",
            ),
            (
                json!({"req_id":"r", "cmd":"message"}),
                "missing or invalid 'text'",
            ),
        ] {
            assert_eq!(command(&request).unwrap_err(), expected);
        }
        for key in ["req_id", "cmd", "text"] {
            for value in [
                Value::Null,
                json!(3),
                json!(false),
                json!(""),
                json!(" \n "),
                json!([]),
                json!({}),
            ] {
                let mut request = json!({"req_id":"r", "cmd":"message", "text":"fix"});
                request[key] = value;
                assert_eq!(
                    command(&request).unwrap_err(),
                    format!("missing or invalid '{key}'")
                );
            }
        }
        for key in ["target", "id", "stage", "stage_run_id", "nonce"] {
            let mut request = json!({});
            request[key] = Value::Null;
            assert_eq!(
                command(&request).unwrap_err(),
                format!("'{key}' is not allowed on a bound input command")
            );
        }
        assert_eq!(
            command(&json!({"cmd":"pause", "target":null, "id":null})).unwrap_err(),
            "'target' is not allowed on a bound input command"
        );
    }
}
