use super::task_kind::TaskKind;
use rig_core::completion::ToolDefinition;

/// Maximum number of concurrent tool calls the harness will execute from a
/// single model message. General tools (Read, Write, Grep, Glob, etc.) queue
/// when this limit is exceeded — only Task calls are subject to a hard
/// rejection cap (`TASK_MAX_PER_TURN`).
pub(crate) const MAX_CONCURRENT_TOOLS: usize = 8;
/// Cap on Task calls spawned from a single model message. Without this, one
/// response could fan out unbounded child loops and overflow the context.
pub(crate) const TASK_MAX_PER_TURN: usize = 8;

/// Tools that stay available even when a tool filter is set. `task_fn`
/// re-applies both the tool filter and the sandbox mode to every nested agent,
/// so bypassing the filter here cannot escape containment.
const ALWAYS_AVAILABLE: &[&str] = &["Task", "Done"];

pub(crate) fn always_available(name: &str) -> bool {
    ALWAYS_AVAILABLE.contains(&name)
}

/// Mischief is a chat-only tool that bypasses tool filters in chat sessions.
/// Headless gremlins must get it from their native block (which never lists
/// it), so this helper keeps it out of headless loops.
pub(crate) fn chat_only_always_available(name: &str, chat: bool) -> bool {
    chat && name == "Mischief"
}

/// One tool parameter: the name and description shown in the JSON schema,
/// the JSON-schema `ty`, whether the model must supply it, and whether
/// `validate_tool_args` type-checks it. A handful of fields are advertised in
/// the JSON schema but were historically *not* validated (Read's
/// `limit`/`offset`, Bash's `timeout`, Grep's `path`/`glob`/`max_matches`,
/// Glob's `path`); those carry `validated: false` so argument checking stays
/// byte-for-byte identical to the old hand-written `tool_param_schema`.
struct FieldSpec {
    name: &'static str,
    description: &'static str,
    ty: &'static str,
    required: bool,
    validated: bool,
}

/// One tool: the single source of truth shared by `tool_param_schema`
/// (validation triples) and `tool_definitions` (model-facing JSON schema).
/// `description` may contain the `{TASK_MAX_PER_TURN}` /
/// `{MAX_CONCURRENT_TOOLS}` placeholders, substituted when the definition is
/// emitted.
struct ToolSpec {
    name: &'static str,
    description: &'static str,
    fields: &'static [FieldSpec],
}

/// Every tool's contract. The order is the order `tool_definitions`
/// advertises them; `LoadSkill` is emitted only for chat sessions.
static TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "Read",
        description: "Read the contents of a file. Paths outside the sandbox are rejected. Use offset/limit to paginate large files — continue reading with offset until you have the whole file. Each returned line is prefixed with its true 1-based line number in the source file followed by a `|` separator (e.g. `12|content`); `offset` is a 0-indexed line skip count, so line numbers stay aligned with `Grep` results even when paging.",
        fields: &[
            FieldSpec { name: "file_path", description: "Path to the file to read (relative or absolute)", ty: "string", required: true, validated: true },
            FieldSpec { name: "limit", description: "Maximum number of lines to read", ty: "integer", required: false, validated: false },
            FieldSpec { name: "offset", description: "Number of lines to skip from the start of the file (0-indexed, default 0)", ty: "integer", required: false, validated: false },
        ],
    },
    ToolSpec {
        name: "Landscape",
        description: "This is your primary discovery tool — use it FIRST whenever you need to understand codebase layout, find where functions/types/tests live, or explore an unfamiliar directory. Show the structural outline of a file or directory. For a file, returns space-separated `line|kind|name` items on a single line. Kinds include fn, struct, impl, trait, enum, mod, class, method, heading, and key. Test functions inside `#[cfg(test)]` modules are shown individually. Unknown file extensions yield the file path. For a directory, returns a tree-formatted outline using the same `line|kind|name` item format as the landscape block: directory headers end with `/` and file headers end with `:` followed by space-separated items. The `depth` argument limits how many nested directory levels to descend (default: all levels). The optional `token_budget` argument applies skeletonization and depth back-off when the tree is too large, just like the system prompt landscape block. Paths outside the sandbox are rejected.",
        fields: &[
            FieldSpec { name: "file_path", description: "Path to the file or directory to outline (relative or absolute)", ty: "string", required: true, validated: true },
            FieldSpec { name: "depth", description: "For directories, how many nested directory levels to descend (default: all levels)", ty: "integer", required: false, validated: true },
            FieldSpec { name: "token_budget", description: "Optional token budget for directory outlines. When set, the tree is trimmed with skeletonization and depth back-off to fit the budget.", ty: "integer", required: false, validated: true },
        ],
    },
    ToolSpec {
        name: "Edit",
        description: "Make one or more non-overlapping replacements in a file. Each edit replaces old_string with new_string (requires unique match). Use start_line/end_line to scope the search to a line range when old_string appears multiple times. Edits are validated against the original file content and must not overlap. After a failed edit due to non-uniqueness, re-read only the failing region (with offset/limit), not the whole file. All edits in the array target the same file, specified by the top-level `file_path` parameter — do not nest `file_path` inside individual edit objects.",
        fields: &[
            FieldSpec { name: "file_path", description: "Path to the file to edit (relative or absolute)", ty: "string", required: true, validated: true },
            // `edits` has no top-level description; its nested item schema is
            // built specially in `tool_parameters`.
            FieldSpec { name: "edits", description: "", ty: "array", required: true, validated: true },
        ],
    },
    ToolSpec {
        name: "Bash",
        description: "Execute a shell command via `bash -c`. Returns combined stdout and stderr. Exit codes are reported as `[exit N]`; commands exceeding the timeout return `[timeout]`. Runs in the gremlin's working directory with no stdin. Path arguments are checked against the sandbox before execution — escapes return an error without running the command. Default timeout is 120 seconds.",
        fields: &[
            FieldSpec { name: "command", description: "Shell command to execute (run via bash -c)", ty: "string", required: true, validated: true },
            FieldSpec { name: "timeout", description: "Timeout in seconds (optional, default 120)", ty: "integer", required: false, validated: false },
        ],
    },
    ToolSpec {
        name: "Write",
        description: "Write content to a file, creating it if necessary. Overwrites the file if it already exists. Parent directories are created as needed.",
        fields: &[
            FieldSpec { name: "file_path", description: "Path to the file to write (relative or absolute)", ty: "string", required: true, validated: true },
            FieldSpec { name: "content", description: "Content to write to the file", ty: "string", required: true, validated: true },
        ],
    },
    ToolSpec {
        name: "Grep",
        description: "Search file contents using a Rust regex pattern. Returns matches in `path:line_number:line_content` format. When `context_before`/`context_after` are nonzero, each match expands to include up to that many surrounding lines; context lines use a `-` separator instead of `:` (e.g. `path-12-context`), touching/overlapping windows merge into one block, and non-contiguous blocks are separated by a single `--` line. `max_matches` counts matches, not emitted lines. Binaries and dot-directories are skipped, as are common build dirs (node_modules, target, __pycache__). Results are truncated at max_matches (default 2000). The regex engine does NOT support lookahead or lookbehind.",
        fields: &[
            FieldSpec { name: "pattern", description: "Regex pattern to search for (Rust regex syntax — no lookahead/lookbehind)", ty: "string", required: true, validated: true },
            FieldSpec { name: "path", description: "Directory or file to search (default: current directory)", ty: "string", required: false, validated: false },
            FieldSpec { name: "glob", description: "Glob filter for file names (e.g. `*.rs`)", ty: "string", required: false, validated: false },
            FieldSpec { name: "max_matches", description: "Maximum number of matches to return (optional, default 2000)", ty: "integer", required: false, validated: false },
            FieldSpec { name: "context_before", description: "Number of context lines to include before each match (optional, default 0)", ty: "integer", required: false, validated: true },
            FieldSpec { name: "context_after", description: "Number of context lines to include after each match (optional, default 0)", ty: "integer", required: false, validated: true },
        ],
    },
    ToolSpec {
        name: "Glob",
        description: "Find files matching a glob pattern. Supports `**` for recursive matching. Returns sorted paths, one per line.",
        fields: &[
            FieldSpec { name: "pattern", description: "Glob pattern (e.g. `**/*.rs`, `src/**/mod.rs`)", ty: "string", required: true, validated: true },
            FieldSpec { name: "path", description: "Base directory to search in (default: current directory)", ty: "string", required: false, validated: false },
        ],
    },
    ToolSpec {
        name: "WebSearch",
        description: "Fetch a web page and extract its text via `curl -sL`. Returns the body text capped at a byte limit. Use this to look up documentation, check package versions, or read external references. Default timeout is 30 seconds.",
        fields: &[
            FieldSpec { name: "url", description: "The URL to fetch (e.g. https://example.com/page)", ty: "string", required: true, validated: true },
            FieldSpec { name: "timeout", description: "Timeout in seconds (optional, default 30)", ty: "integer", required: false, validated: true },
        ],
    },
    ToolSpec {
        name: "Mischief",
        description: "Return the gremlins YAML DSL reference. Call with no arguments to get a complete reference for definition files, stage types, artifact URIs, interpolation, bootstrap, land, and stage definitions.",
        fields: &[],
    },
    ToolSpec {
        name: "Done",
        description: "Signal that your work is complete — call Done(summary) alongside your final message to finish. The summary parameter briefly describes what you accomplished.",
        fields: &[
            FieldSpec { name: "summary", description: "Brief summary of what was accomplished", ty: "string", required: true, validated: true },
        ],
    },
    ToolSpec {
        name: "Task",
        description: "Delegate a self-contained task to a worker. Non-Fork kinds start with a clean conversation context; the Fork kind inherits the parent conversation history. Multiple Task calls in the same message run concurrently, up to {TASK_MAX_PER_TURN} per message (the harness supports up to {MAX_CONCURRENT_TOOLS} total concurrent tool calls). Returns the worker's final text output.",
        fields: &[
            FieldSpec { name: "kind", description: "The kind of task to delegate.", ty: "string", required: true, validated: true },
            FieldSpec { name: "prompt", description: "Full task briefing for the worker. Non-Fork kinds start with a clean conversation context and see nothing from your conversation — include all relevant file contents, diffs, plans, and instructions the worker needs directly in this field. The Fork kind inherits the parent conversation history before this prompt.", ty: "string", required: true, validated: true },
        ],
    },
    ToolSpec {
        name: "LoadSkill",
        description: "Load a chat skill by name. Use this when a task matches a skill's description from the available skills index. Returns the skill body and a listing of its bundled files. The skill is then active for the rest of the session.",
        fields: &[
            FieldSpec { name: "name", description: "Name of the skill to load", ty: "string", required: true, validated: true },
        ],
    },
];

/// Used by `validate_tool_args` to produce rich error messages when
/// the model sends malformed tool-call arguments. The boolean marks whether
/// the parameter is required; optional parameters are type-checked only when
/// present. Fields are drawn from [`TOOL_SPECS`], keeping the advertised and
/// validated contracts in lockstep.
fn tool_param_schema(name: &str) -> Option<Vec<(&'static str, &'static str, bool)>> {
    let spec = TOOL_SPECS.iter().find(|s| s.name == name)?;
    Some(
        spec.fields
            .iter()
            .filter(|f| f.validated)
            .map(|f| (f.name, f.ty, f.required))
            .collect(),
    )
}

/// Build a short summary of the fields present in a mixed-mode edit object,
/// so the error message shows what the model actually sent.
fn describe_edit_provided_fields(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in &["start_line", "end_line", "old_string", "new_string"] {
        if let Some(v) = obj.get(*key) {
            let display = match v {
                serde_json::Value::String(s) => {
                    if s.len() > 40 {
                        format!("{}<{} chars>", key, s.len())
                    } else {
                        format!("{key}={s:?}")
                    }
                }
                other => format!("{key}={other}"),
            };
            parts.push(display);
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" (provided: {})", parts.join(", "))
    }
}

/// Check that all required tool arguments are present and have the correct types.
/// Returns an error message string on failure, or `None` if valid.
pub(crate) fn validate_tool_args(name: &str, args: &serde_json::Value) -> Option<String> {
    let schema = tool_param_schema(name)?;
    for (field, expected_type, required) in &schema {
        match args.get(field) {
            None if *required => {
                let expected: Vec<String> = schema
                    .iter()
                    .map(|(f, t, required)| {
                        if *required {
                            format!("{f}: {t}")
                        } else {
                            format!("{f}: {t} (optional)")
                        }
                    })
                    .collect();
                let found: Vec<String> = args
                    .as_object()
                    .map(|o| o.keys().map(|k| format!("{k}: …")).collect())
                    .unwrap_or_default();
                let found_str = if found.is_empty() {
                    "<not an object>".into()
                } else {
                    format!("{{{}}}", found.join(", "))
                };
                let mut msg = format!(
                    "Error: missing '{field}' for {name} tool. \
                     Expected: {{{}}}. Got: {found_str}",
                    expected.join(", ")
                );
                // If file_path is missing for Edit and any edit item has a
                // file_path key, the model likely nested it — give a targeted hint.
                if name == "Edit" && *field == "file_path" {
                    if let Some(edits) = args.get("edits").and_then(|v| v.as_array()) {
                        let misplaced = edits.iter().any(|e| {
                            e.as_object()
                                .map(|o| o.contains_key("file_path"))
                                .unwrap_or(false)
                        });
                        if misplaced {
                            msg.push_str(
                                " (Note: file_path belongs at the top level, not inside individual edits — all edits target the same file)",
                            );
                        }
                    }
                }
                return Some(msg);
            }
            None => continue,
            Some(val) => {
                let ok = match *expected_type {
                    "string" => val.is_string(),
                    "array" => val.is_array(),
                    "integer" => val.is_number(),
                    "object" => val.is_object(),
                    _ => true,
                };
                if !ok {
                    let actual = if val.is_string() {
                        "string"
                    } else if val.is_array() {
                        "array"
                    } else if val.is_number() {
                        "number"
                    } else if val.is_object() {
                        "object"
                    } else if val.is_null() {
                        "null"
                    } else if val.is_boolean() {
                        "bool"
                    } else {
                        "?"
                    };
                    return Some(format!(
                        "Error: '{field}' for {name} must be {expected_type}, got {actual}"
                    ));
                }
            }
        }
    }
    // Nested constraints for tools with structured array items.
    if name == "Edit" {
        if let Some(edits) = args.get("edits").and_then(|v| v.as_array()) {
            if edits.is_empty() {
                return Some("Error: 'edits' array must not be empty for Edit tool".into());
            }
            for (i, edit) in edits.iter().enumerate() {
                let obj = match edit.as_object() {
                    Some(o) => o,
                    None => {
                        return Some(format!(
                            "Error: edit {} in 'edits' must be an object for Edit tool",
                            i + 1
                        ));
                    }
                };
                // 'replacement' (the old range-mode field) is removed — reject it
                // explicitly even when old_string/new_string are also present,
                // so legacy mixed-mode requests don't silently apply new_string.
                if obj.contains_key("replacement") {
                    let provided = describe_edit_provided_fields(obj);
                    return Some(format!(
                        "Error: edit {} has 'replacement' which is no longer supported — use 'new_string' instead{}",
                        i + 1,
                        provided
                    ));
                }
                // 'old_string' is always required.
                if !obj.contains_key("old_string") {
                    return Some(format!(
                        "Error: edit {} missing 'old_string' for Edit tool",
                        i + 1
                    ));
                }
                // With 'old_string' present, 'new_string' is also required.
                if !obj.contains_key("new_string") {
                    return Some(format!(
                        "Error: edit {} missing 'new_string' for Edit tool",
                        i + 1
                    ));
                } // 'start_line'/'end_line' are optional scope fields;
                  // both accept zero (clamped to 1 by the executor), and
                  // start_line > end_line is handled by scope_byte_range.
                let has_start = obj.contains_key("start_line");
                let has_end = obj.contains_key("end_line");
                if has_start {
                    match obj.get("start_line").and_then(|v| v.as_u64()) {
                        Some(_) => {}
                        None => {
                            return Some(format!(
                                "Error: edit {} 'start_line' must be an integer for Edit tool",
                                i + 1
                            ));
                        }
                    }
                }
                if has_end {
                    match obj.get("end_line").and_then(|v| v.as_u64()) {
                        Some(_) => {}
                        None => {
                            return Some(format!(
                                "Error: edit {} 'end_line' must be an integer for Edit tool",
                                i + 1
                            ));
                        }
                    }
                }

            }
        }
    }
    None
}

/// Build the JSON-schema `parameters` object for a tool from its [`ToolSpec`].
/// Field names, descriptions and the `required` set all come from the table;
/// only Edit's nested `edits` item schema and Task's `kind` enum are bespoke.
fn tool_parameters(spec: &ToolSpec) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    for field in spec.fields {
        let value = match (spec.name, field.name) {
            ("Edit", "edits") => serde_json::json!({
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "old_string": {
                            "type": "string",
                            "description": "Exact text to replace — must be unique in the file (or unique within start_line/end_line scope)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Replacement text"
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "When old_string appears multiple times, scope the search to start at this 1-based line (inclusive; clamps to EOF if beyond last line)"
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "When old_string appears multiple times, scope the search to end at this 1-based line (inclusive; clamps to EOF if beyond last line)"
                        }
                    },
                    "additionalProperties": false,
                    "required": ["old_string", "new_string"]
                }            }),
            ("Task", "kind") => serde_json::json!({
                "type": "string",
                "enum": TaskKind::ALL
                    .iter()
                    .map(|kind| serde_json::Value::String(kind.as_str().to_string()))
                    .collect::<Vec<_>>(),
                "description": field.description
            }),
            _ => serde_json::json!({
                "type": field.ty,
                "description": field.description
            }),
        };
        properties.insert(field.name.to_string(), value);
    }
    let required: Vec<&str> = spec
        .fields
        .iter()
        .filter(|f| f.required)
        .map(|f| f.name)
        .collect();
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

pub(crate) fn tool_definitions(filter: Option<&[String]>, is_chat: bool) -> Vec<ToolDefinition> {
    let mut all: Vec<ToolDefinition> = Vec::new();
    for spec in TOOL_SPECS {
        // LoadSkill is chat-only; every other tool is always constructed so
        // the filter below can still admit it via `always_available`.
        if spec.name == "LoadSkill" && !is_chat {
            continue;
        }
        let description = spec
            .description
            .replace("{TASK_MAX_PER_TURN}", &TASK_MAX_PER_TURN.to_string())
            .replace("{MAX_CONCURRENT_TOOLS}", &MAX_CONCURRENT_TOOLS.to_string());
        all.push(ToolDefinition {
            name: spec.name.into(),
            description,
            parameters: tool_parameters(spec),
        });
    }
    match filter {
        Some(names) => all
            .into_iter()
            .filter(|t| {
                always_available(&t.name)
                    || (is_chat && t.name == "LoadSkill")
                    || chat_only_always_available(&t.name, is_chat)
                    || names.iter().any(|n| n == &t.name)
            })
            .collect(),
        None => all,
    }
}
