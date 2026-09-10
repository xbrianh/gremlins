use std::collections::{HashMap, HashSet};

use crate::schemas::error::SchemaError;

/// A flattened stage descriptor used for name-filling.
pub struct StageEntry {
    pub name: Option<String>,
    pub auto_name: Option<String>,
    pub stage_type: Option<String>,
    pub is_parallel: bool,
}

/// A stage node for duplicate-producer checking.
pub struct StageNode {
    pub name: String,
    pub stage_type: String,
    pub bind_map: HashMap<String, String>,
    pub interpolation_map: HashMap<String, String>,
    pub skip_if_exists: String,
    pub body: Vec<StageNode>,
}

pub fn fill_names(stages: &mut [StageEntry]) -> Result<(), SchemaError> {
    let mut used: HashSet<String> = HashSet::new();
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for stage in stages.iter() {
        if let Some(ref name) = stage.name {
            if !name.is_empty() {
                let count = name_counts.entry(name.clone()).or_insert(0);
                *count += 1;
            }
        }
    }

    let mut counts: HashMap<String, usize> = HashMap::new();

    for stage in stages.iter_mut() {
        if let Some(ref name) = stage.name {
            if !name.is_empty() {
                // If this explicit name is a duplicate, rename subsequent occurrences
                let count = name_counts.get(name.as_str()).copied().unwrap_or(0);
                if count > 1 && used.contains(name.as_str()) {
                    // This is a duplicate — append -N suffix
                    let base = name.clone();
                    let mut n = 2;
                    let mut candidate = format!("{base}-{n}");
                    while used.contains(&candidate) {
                        n += 1;
                        candidate = format!("{base}-{n}");
                    }
                    stage.name = Some(candidate.clone());
                    used.insert(candidate);
                } else {
                    used.insert(name.clone());
                }
                stage.auto_name = None;
                continue;
            }
        }

        let auto = stage.auto_name.take().unwrap_or_default();
        let stage_type = if !auto.is_empty() {
            auto
        } else if stage.is_parallel {
            "parallel".to_string()
        } else {
            stage.stage_type.clone().unwrap_or_default()
        };

        let count = counts.entry(stage_type.clone()).or_insert(0);
        *count += 1;
        let n = *count;
        let mut candidate = if n == 1 {
            stage_type.clone()
        } else {
            format!("{stage_type}-{n}")
        };

        while used.contains(&candidate) {
            *count += 1;
            let m = *count;
            candidate = format!("{stage_type}-{m}");
        }
        stage.name = Some(candidate.clone());
        used.insert(candidate);
    }

    Ok(())
}

pub fn check_duplicate_producers(
    stages: &[StageNode],
    extra_out: &HashMap<String, String>,
) -> Result<(), SchemaError> {
    let mut seen: HashMap<String, (String, String)> = HashMap::new();

    for (key, uri_str) in extra_out {
        if key.ends_with('?') {
            continue;
        }
        if !uri_str.starts_with("artifact://") {
            continue;
        }
        let clean_uri = uri_str.clone();
        if let Some((prev_name, _)) = seen.get(&clean_uri) {
            return Err(SchemaError::Generic(format!(
                "duplicate artifact producer: stage '{}' and stage '{}' both produce '{}'",
                prev_name, "bootstrap", clean_uri
            )));
        }
        seen.insert(clean_uri, ("bootstrap".to_string(), uri_str.clone()));
    }

    for stage in stages {
        // Stages with skip_if_exists set are conditional producers — they only
        // produce the artifact if it doesn't already exist, so they should not
        // trigger duplicate-producer errors.
        let is_conditional = !stage.skip_if_exists.is_empty();

        if !is_conditional {
            for (raw_key, uri_str) in &stage.bind_map {
                if raw_key.ends_with('?') {
                    continue;
                }
                if !uri_str.starts_with("artifact://") {
                    continue;
                }
                let clean_uri = uri_str.clone();
                if let Some((prev_name, _)) = seen.get(&clean_uri) {
                    return Err(SchemaError::Generic(format!(
                        "duplicate artifact producer: stage '{}' and stage '{}' both produce '{}'",
                        prev_name, stage.name, clean_uri
                    )));
                }
                seen.insert(clean_uri, (stage.name.clone(), uri_str.clone()));
            }
        }

        // For both parallel and sequential bodies, check each child in its own
        // scope.  Sequential stages intentionally overwrite artifacts (e.g. a
        // sanitize step rewrites rolling-plan.md), so siblings must not share
        // a seen map.
        for child in &stage.body {
            let child_slice = std::slice::from_ref(child);
            check_duplicate_producers(child_slice, &HashMap::new())?;
        }
    }

    Ok(())
}

/// Extract artifact:// URIs referenced in an interpolation value string.
/// Returns (uri, is_optional).
///
/// Handles:
///   content("artifact://foo")        → (artifact://foo, false)
///   content("artifact://foo")?       → (artifact://foo, true)
///   content("artifact://foo", "$")   → (artifact://foo, false)
///   artifact://foo                   → (artifact://foo, false)
///   artifact://foo?                  → (artifact://foo, true)
fn extract_artifact_uris(raw: &str) -> Vec<(String, bool)> {
    // Match artifact://... URIs embedded in content() calls or bare
    let re = regex::Regex::new(r#"artifact://[^\s"')?]+"#).unwrap();
    re.find_iter(raw)
        .map(|m| {
            let uri = m.as_str().to_string();
            // Each URI is optional if it's immediately followed by '?' or
            // ')', optional whitespace/quote, and then '?' (content()-style).
            let after = raw[m.end()..].trim_start();
            let uri_optional = after.starts_with('?')
                || after
                    .trim_start_matches(|c: char| {
                        c == ')' || c == '"' || c == '\'' || c.is_whitespace()
                    })
                    .starts_with('?');
            (uri, uri_optional)
        })
        .collect()
}

/// Validate that every artifact:// URI consumed via interpolation has been
/// produced by a prior stage's bind or by bootstrap.
///
/// Bootstrap-produced URIs come from `launch_cmds` (parsed gremlins:bind_artifact
/// calls) and `cli_out` values.  `artifact://base_sha` and `artifact://base_ref`
/// are always available.
pub fn check_unresolved_consumers(
    stages: &[StageNode],
    launch_cmds: &[String],
    cli_out: &HashMap<String, String>,
) -> Result<(), SchemaError> {
    let mut produced: HashSet<String> = HashSet::new();

    // Implicit artifacts always bound at launch
    produced.insert("artifact://base_sha".to_string());
    produced.insert("artifact://base_ref".to_string());

    // Parse launch_cmds for gremlins:bind_artifact(...) — any argument
    // position, quoted or unquoted (2-arg and legacy 3-arg forms).
    let bind_call_re = regex::Regex::new(r#"gremlins:bind_artifact\(([^)]*)\)"#).unwrap();
    let uri_re = regex::Regex::new(r#"artifact://[^\s"',)]+"#).unwrap();
    for cmd in launch_cmds {
        for caps in bind_call_re.captures_iter(cmd) {
            for m in uri_re.find_iter(&caps[1]) {
                produced.insert(m.as_str().to_string());
            }
        }
    }

    // cli_out values are artifact URIs
    for uri in cli_out.values() {
        if uri.starts_with("artifact://") {
            produced.insert(uri.clone());
        }
    }

    check_consumers_inner(stages, &mut produced, false)?;
    Ok(())
}

fn check_consumers_inner(
    stages: &[StageNode],
    produced: &mut HashSet<String>,
    is_parallel_child: bool,
) -> Result<(), SchemaError> {
    let base_produced = if is_parallel_child {
        // Each parallel child sees only the pre-parallel produced set
        produced.clone()
    } else {
        HashSet::new() // unused — sequential children share produced
    };

    for stage in stages {
        let check_set = if is_parallel_child {
            &base_produced
        } else {
            &*produced
        };

        // Check interpolation references
        for raw in stage.interpolation_map.values() {
            for (uri, optional) in extract_artifact_uris(raw) {
                if optional {
                    continue;
                }
                if !check_set.contains(&uri) {
                    return Err(SchemaError::UnresolvedArtifactConsumer {
                        stage: stage.name.clone(),
                        uri,
                    });
                }
            }
        }

        // Collect this stage's bind outputs.
        // Both bind_map values and keys can serve as producer URIs:
        // - Values like "artifact://plan.md" are direct artifact URIs.
        // - Keys like "review-chain" can be referenced as
        //   artifact://review-chain by downstream consumers (the executor
        //   resolves the key through the artifact registry at runtime).
        // - Runtime templates ({name}, etc.) are resolved against the
        //   stage's own metadata so that recipes produce predictable URIs.
        let mut stage_outputs: Vec<String> = Vec::new();
        for (key, val) in &stage.bind_map {
            // Resolve runtime templates in keys: {name} → stage name
            let resolved_key = key.replace("{name}", &stage.name);
            if val.starts_with("artifact://") {
                stage_outputs.push(val.clone());
            }
            // Plain keys (no :// scheme) also serve as artifact lookup keys.
            if !resolved_key.ends_with('?') && !resolved_key.contains("://") {
                stage_outputs.push(format!("artifact://{resolved_key}"));
            }
        }

        // Add to the shared produced set (needed for sequential siblings)
        for uri in &stage_outputs {
            produced.insert(uri.clone());
        }

        // Recurse into body children.
        // For parallel children, pass only pre-parallel + this child's own
        // outputs so nested parallels don't see sibling outputs.
        if !stage.body.is_empty() {
            let is_parallel = stage.stage_type == "parallel";
            if is_parallel_child {
                let mut child_produced = base_produced.clone();
                for uri in &stage_outputs {
                    child_produced.insert(uri.clone());
                }
                check_consumers_inner(&stage.body, &mut child_produced, is_parallel)?;
            } else {
                check_consumers_inner(&stage.body, produced, is_parallel)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fill_names_basic() {
        let mut stages = vec![
            StageEntry {
                name: None,
                auto_name: None,
                stage_type: Some("agent".to_string()),
                is_parallel: false,
            },
            StageEntry {
                name: None,
                auto_name: None,
                stage_type: Some("agent".to_string()),
                is_parallel: false,
            },
        ];
        fill_names(&mut stages).unwrap();
        assert_eq!(stages[0].name.as_deref(), Some("agent"));
        assert_eq!(stages[1].name.as_deref(), Some("agent-2"));
    }

    #[test]
    fn test_fill_names_parallel() {
        let mut stages = vec![StageEntry {
            name: None,
            auto_name: None,
            stage_type: None,
            is_parallel: true,
        }];
        fill_names(&mut stages).unwrap();
        assert_eq!(stages[0].name.as_deref(), Some("parallel"));
    }

    #[test]
    fn test_fill_names_explicit_wins() {
        let mut stages = vec![StageEntry {
            name: Some("custom".to_string()),
            auto_name: None,
            stage_type: Some("agent".to_string()),
            is_parallel: false,
        }];
        fill_names(&mut stages).unwrap();
        assert_eq!(stages[0].name.as_deref(), Some("custom"));
    }

    // --- StageNode helpers ---

    fn stage_node(name: &str, stage_type: &str) -> StageNode {
        StageNode {
            name: name.to_string(),
            stage_type: stage_type.to_string(),
            bind_map: HashMap::new(),
            interpolation_map: HashMap::new(),
            skip_if_exists: String::new(),
            body: vec![],
        }
    }

    fn stage_with_interp(
        name: &str,
        stage_type: &str,
        interp: HashMap<String, String>,
    ) -> StageNode {
        StageNode {
            interpolation_map: interp,
            ..stage_node(name, stage_type)
        }
    }

    fn stage_with_bind(
        name: &str,
        stage_type: &str,
        bind_map: HashMap<String, String>,
    ) -> StageNode {
        StageNode {
            bind_map,
            ..stage_node(name, stage_type)
        }
    }

    // --- check_duplicate_producers tests ---

    #[test]
    fn test_check_duplicate_producers_errs_on_different_uri() {
        let stages = vec![
            stage_with_bind(
                "s1",
                "agent",
                HashMap::from([("out".to_string(), "uri-a".to_string())]),
            ),
            stage_with_bind(
                "s2",
                "agent",
                HashMap::from([("result".to_string(), "uri-a".to_string())]),
            ),
        ];
        // Same URI value "uri-a" — no error (different keys, same value)
        check_duplicate_producers(&stages, &HashMap::new()).unwrap();
    }

    #[test]
    fn test_check_duplicate_producers_errs_on_same_uri_different_stages() {
        let stages = vec![
            stage_with_bind(
                "s1",
                "agent",
                HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]),
            ),
            stage_with_bind(
                "s2",
                "agent",
                HashMap::from([("result".to_string(), "artifact://plan.md".to_string())]),
            ),
        ];
        let err = check_duplicate_producers(&stages, &HashMap::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate artifact producer"),
            "expected duplicate artifact producer error, got: {msg}"
        );
        assert!(
            msg.contains("artifact://plan.md"),
            "expected URI in error, got: {msg}"
        );
    }

    #[test]
    fn test_check_duplicate_producers_ok_on_different_uri() {
        let stages = vec![
            stage_with_bind(
                "s1",
                "agent",
                HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]),
            ),
            stage_with_bind(
                "s2",
                "agent",
                HashMap::from([("out".to_string(), "artifact://spec.md".to_string())]),
            ),
        ];
        check_duplicate_producers(&stages, &HashMap::new()).unwrap();
    }

    #[test]
    fn test_check_duplicate_producers_skip_if_exists_bypasses_check() {
        let stages = vec![
            stage_with_bind(
                "s1",
                "agent",
                HashMap::from([("out".to_string(), "uri-a".to_string())]),
            ),
            StageNode {
                name: "s2".to_string(),
                stage_type: "agent".to_string(),
                bind_map: HashMap::from([("out".to_string(), "uri-b".to_string())]),
                interpolation_map: HashMap::new(),
                skip_if_exists: "true".to_string(),
                body: vec![],
            },
        ];
        check_duplicate_producers(&stages, &HashMap::new()).unwrap();
    }

    #[test]
    fn test_parallel_children_isolated_scopes() {
        // Two children of a parallel stage can both produce the same URI
        // without triggering a duplicate error (isolated scopes).
        let stages = vec![StageNode {
            name: "par".to_string(),
            stage_type: "parallel".to_string(),
            bind_map: HashMap::new(),
            interpolation_map: HashMap::new(),
            skip_if_exists: String::new(),
            body: vec![
                stage_with_bind(
                    "c1",
                    "agent",
                    HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]),
                ),
                stage_with_bind(
                    "c2",
                    "agent",
                    HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]),
                ),
            ],
        }];
        check_duplicate_producers(&stages, &HashMap::new()).unwrap();
    }

    #[test]
    fn test_non_parallel_children_shared_scope() {
        // Two children of a non-parallel (sequence) stage producing the same
        // URI is allowed — sequential stages intentionally overwrite artifacts
        // (e.g. a sanitize step rewrites rolling-plan.md).
        let stages = vec![StageNode {
            name: "seq".to_string(),
            stage_type: "sequence".to_string(),
            bind_map: HashMap::new(),
            interpolation_map: HashMap::new(),
            skip_if_exists: String::new(),
            body: vec![
                stage_with_bind(
                    "c1",
                    "agent",
                    HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]),
                ),
                stage_with_bind(
                    "c2",
                    "agent",
                    HashMap::from([("result".to_string(), "artifact://plan.md".to_string())]),
                ),
            ],
        }];
        check_duplicate_producers(&stages, &HashMap::new()).unwrap();
    }

    #[test]
    fn test_extra_out_collision_with_stage() {
        let stages = vec![stage_with_bind(
            "s1",
            "agent",
            HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]),
        )];
        let extra_out = HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]);
        let err = check_duplicate_producers(&stages, &extra_out).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate artifact producer"),
            "expected duplicate artifact producer error, got: {msg}"
        );
        assert!(
            msg.contains("bootstrap"),
            "expected 'bootstrap' in error, got: {msg}"
        );
    }

    #[test]
    fn test_extra_out_no_collision_on_different_uri() {
        let stages = vec![stage_with_bind(
            "s1",
            "agent",
            HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]),
        )];
        let extra_out = HashMap::from([("out".to_string(), "artifact://spec.md".to_string())]);
        check_duplicate_producers(&stages, &extra_out).unwrap();
    }

    #[test]
    fn test_optional_bind_ignored_for_duplicates() {
        // Keys ending with '?' are optional — they should not trigger duplicates.
        let stages = vec![
            stage_with_bind(
                "s1",
                "agent",
                HashMap::from([("out".to_string(), "artifact://plan.md".to_string())]),
            ),
            stage_with_bind(
                "s2",
                "agent",
                HashMap::from([("out?".to_string(), "artifact://plan.md".to_string())]),
            ),
        ];
        check_duplicate_producers(&stages, &HashMap::new()).unwrap();
    }

    #[test]
    fn test_empty_stages_ok() {
        check_duplicate_producers(&[], &HashMap::new()).unwrap();
    }

    // --- extract_artifact_uris tests ---

    #[test]
    fn test_extract_content_form() {
        let uris = extract_artifact_uris(r#"content("artifact://diff.txt")"#);
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0].0, "artifact://diff.txt");
        assert!(!uris[0].1);
    }

    #[test]
    fn test_extract_content_optional() {
        let uris = extract_artifact_uris(r#"content("artifact://diff.txt")?"#);
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0].0, "artifact://diff.txt");
        assert!(uris[0].1);
    }

    #[test]
    fn test_extract_bare_uri() {
        let uris = extract_artifact_uris(r#"artifact://instructions.md"#);
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0].0, "artifact://instructions.md");
        assert!(!uris[0].1);
    }

    #[test]
    fn test_extract_bare_uri_optional() {
        let uris = extract_artifact_uris(r#"artifact://instructions.md?"#);
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0].0, "artifact://instructions.md");
        assert!(uris[0].1);
    }

    #[test]
    fn test_extract_content_with_json_path() {
        let uris = extract_artifact_uris(r#"content("artifact://data.json", "$.key")"#);
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0].0, "artifact://data.json");
        assert!(!uris[0].1);
    }

    #[test]
    fn test_extract_multiple_uris() {
        let uris =
            extract_artifact_uris(r#"content("artifact://a.txt") content("artifact://b.txt")"#);
        assert_eq!(uris.len(), 2);
        assert_eq!(uris[0].0, "artifact://a.txt");
        assert_eq!(uris[1].0, "artifact://b.txt");
    }

    #[test]
    fn test_extract_mixed_optionality() {
        // Only the last URI is optional — earlier ones must not inherit it.
        let uris =
            extract_artifact_uris(r#"content("artifact://a.txt") content("artifact://b.txt")?"#);
        assert_eq!(uris.len(), 2);
        assert_eq!(uris[0].0, "artifact://a.txt");
        assert!(!uris[0].1, "first URI must not be optional");
        assert_eq!(uris[1].0, "artifact://b.txt");
        assert!(uris[1].1, "second URI must be optional");
    }

    // --- check_unresolved_consumers tests ---

    #[test]
    fn test_unresolved_ok_when_produced() {
        let stages = vec![
            stage_with_bind(
                "producer",
                "agent",
                HashMap::from([("out".to_string(), "artifact://diff.txt".to_string())]),
            ),
            stage_with_interp(
                "consumer",
                "agent",
                HashMap::from([(
                    "diff".to_string(),
                    r#"content("artifact://diff.txt")"#.to_string(),
                )]),
            ),
        ];
        check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap();
    }

    #[test]
    fn test_unresolved_errs_when_missing() {
        let stages = vec![stage_with_interp(
            "consumer",
            "agent",
            HashMap::from([(
                "diff".to_string(),
                r#"content("artifact://diff.txt")"#.to_string(),
            )]),
        )];
        let err = check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("artifact://diff.txt"),
            "expected uri in error, got: {msg}"
        );
        assert!(msg.contains("consumer"), "expected stage name, got: {msg}");
    }

    #[test]
    fn test_unresolved_optional_ok() {
        let stages = vec![stage_with_interp(
            "consumer",
            "agent",
            HashMap::from([(
                "diff".to_string(),
                r#"content("artifact://diff.txt")?"#.to_string(),
            )]),
        )];
        check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap();
    }

    #[test]
    fn test_unresolved_launch_cmds_produce() {
        let launch_cmds = vec![r#"gremlins:bind_artifact("artifact://plan.md", plan)"#.to_string()];
        let stages = vec![stage_with_interp(
            "consumer",
            "agent",
            HashMap::from([(
                "plan".to_string(),
                r#"content("artifact://plan.md")"#.to_string(),
            )]),
        )];
        check_unresolved_consumers(&stages, &launch_cmds, &HashMap::new()).unwrap();
    }

    #[test]
    fn test_unresolved_launch_cmds_2arg_form() {
        // 2-arg form: (uri, source_key)
        let launch_cmds =
            vec!["gremlins:bind_artifact(\"artifact://session/plan.md\", plan)".to_string()];
        let stages = vec![stage_with_interp(
            "consumer",
            "agent",
            HashMap::from([(
                "plan".to_string(),
                r#"content("artifact://session/plan.md")"#.to_string(),
            )]),
        )];
        check_unresolved_consumers(&stages, &launch_cmds, &HashMap::new()).unwrap();
    }

    #[test]
    fn test_unresolved_base_sha_always_available() {
        let stages = vec![stage_with_interp(
            "consumer",
            "exec",
            HashMap::from([(
                "base".to_string(),
                r#"content("artifact://base_sha")"#.to_string(),
            )]),
        )];
        check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap();
    }

    #[test]
    fn test_unresolved_parallel_children_isolated() {
        // Each parallel child starts with the pre-parallel produced set,
        // so a child cannot consume its sibling's output.
        let stages = vec![
            stage_with_bind(
                "pre",
                "agent",
                HashMap::from([("out".to_string(), "artifact://shared.txt".to_string())]),
            ),
            StageNode {
                name: "par".to_string(),
                stage_type: "parallel".to_string(),
                bind_map: HashMap::new(),
                interpolation_map: HashMap::new(),
                skip_if_exists: String::new(),
                body: vec![
                    stage_with_bind(
                        "c1",
                        "agent",
                        HashMap::from([("out".to_string(), "artifact://c1-out.txt".to_string())]),
                    ),
                    // c2 tries to consume c1's output — should fail
                    stage_with_interp(
                        "c2",
                        "agent",
                        HashMap::from([(
                            "sib".to_string(),
                            r#"content("artifact://c1-out.txt")"#.to_string(),
                        )]),
                    ),
                ],
            },
        ];
        let err = check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("artifact://c1-out.txt"),
            "expected uri in error, got: {msg}"
        );
    }

    #[test]
    fn test_unresolved_nested_parallel_does_not_leak_siblings() {
        // A nested parallel inside parallel child c2 must not see c1's outputs.
        let stages = vec![StageNode {
            name: "par".to_string(),
            stage_type: "parallel".to_string(),
            bind_map: HashMap::new(),
            interpolation_map: HashMap::new(),
            skip_if_exists: String::new(),
            body: vec![
                stage_with_bind(
                    "c1",
                    "agent",
                    HashMap::from([("out".to_string(), "artifact://c1-out.txt".to_string())]),
                ),
                // c2 has a nested parallel whose child tries to consume c1's output
                StageNode {
                    name: "c2".to_string(),
                    stage_type: "agent".to_string(),
                    bind_map: HashMap::new(),
                    interpolation_map: HashMap::new(),
                    skip_if_exists: String::new(),
                    body: vec![StageNode {
                        name: "nested-par".to_string(),
                        stage_type: "parallel".to_string(),
                        bind_map: HashMap::new(),
                        interpolation_map: HashMap::new(),
                        skip_if_exists: String::new(),
                        body: vec![stage_with_interp(
                            "nc1",
                            "agent",
                            HashMap::from([(
                                "bad".to_string(),
                                r#"content("artifact://c1-out.txt")"#.to_string(),
                            )]),
                        )],
                    }],
                },
            ],
        }];
        let err = check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("artifact://c1-out.txt"),
            "nested parallel must not see sibling outputs, got: {msg}"
        );
    }

    #[test]
    fn test_unresolved_sequence_children_share_produced() {
        // Sequential body children see sibling outputs
        let stages = vec![StageNode {
            name: "seq".to_string(),
            stage_type: "sequence".to_string(),
            bind_map: HashMap::new(),
            interpolation_map: HashMap::new(),
            skip_if_exists: String::new(),
            body: vec![
                stage_with_bind(
                    "c1",
                    "agent",
                    HashMap::from([("out".to_string(), "artifact://c1-out.txt".to_string())]),
                ),
                stage_with_interp(
                    "c2",
                    "agent",
                    HashMap::from([(
                        "sib".to_string(),
                        r#"content("artifact://c1-out.txt")"#.to_string(),
                    )]),
                ),
            ],
        }];
        check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap();
    }

    #[test]
    fn test_unresolved_cli_out_produces() {
        let cli_out = HashMap::from([("plan".to_string(), "artifact://plan.md".to_string())]);
        let stages = vec![stage_with_interp(
            "consumer",
            "agent",
            HashMap::from([(
                "plan".to_string(),
                r#"content("artifact://plan.md")"#.to_string(),
            )]),
        )];
        check_unresolved_consumers(&stages, &[], &cli_out).unwrap();
    }

    #[test]
    fn test_unresolved_bare_uri_optional_ok() {
        let stages = vec![stage_with_interp(
            "consumer",
            "agent",
            HashMap::from([(
                "instr".to_string(),
                "artifact://instructions.md?".to_string(),
            )]),
        )];
        check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap();
    }

    #[test]
    fn test_unresolved_bare_uri_missing_errs() {
        let stages = vec![stage_with_interp(
            "consumer",
            "agent",
            HashMap::from([(
                "instr".to_string(),
                "artifact://instructions.md".to_string(),
            )]),
        )];
        let err = check_unresolved_consumers(&stages, &[], &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("artifact://instructions.md"));
    }
}
