/// Whether tools are available to the model. NoTools mode produces zero
/// tool definitions, so the model cannot call any tool — including Task and
/// Done. Used by single-turn naked completions (local compaction, etc.).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolMode {
    Full,
    NoTools,
}

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

/// One tool parameter: the name and description shown in the JSON schema,
/// the JSON-schema `ty`, whether the model must supply it, and whether
/// `validate_tool_args` type-checks it. A handful of fields are advertised in
/// the JSON schema but were historically *not* validated (Read's
/// `limit`/`offset`, Bash's `timeout`, Grep's `path`/`glob`/`max_matches`,
/// Glob's `path`); those carry `validated: false` so argument checking stays
/// byte-for-byte identical to the old hand-written `tool_param_schema`.
pub(super) struct FieldSpec {
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    pub(super) ty: &'static str,
    pub(super) required: bool,
    pub(super) validated: bool,
}

/// One tool: the single source of truth shared by `tool_param_schema`
/// (validation triples) and `tool_definitions` (model-facing JSON schema).
/// `description` may contain the `{TASK_MAX_PER_TURN}` /
/// `{MAX_CONCURRENT_TOOLS}` placeholders, substituted when the definition is
/// emitted.
pub(super) struct ToolSpec {
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    pub(super) fields: &'static [FieldSpec],
    /// Chat-only tools are omitted from tool definitions in headless mode
    /// and bypass the tool filter in chat mode. Only LoadSkill and
    /// WebSearch set this to `true`.
    pub(super) chat_only: bool,
}

/// Every tool's contract. The order is the order `tool_definitions`
/// advertises them; `LoadSkill` is emitted only for chat sessions.
pub(super) static TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "Read",
        description: "Read the contents of a file. Paths outside the sandbox are rejected. Use offset/limit to paginate large files — continue reading with offset until you have the whole file. Each returned line is prefixed with its true 1-based line number in the source file followed by a `|` separator (e.g. `12|content`); `offset` is a 0-indexed line skip count, so line numbers stay aligned with `Grep` results even when paging.",
        fields: &[
            FieldSpec { name: "file_path", description: "Path to the file to read (relative or absolute)", ty: "string", required: true, validated: true },
            FieldSpec { name: "limit", description: "Maximum number of lines to read", ty: "integer", required: false, validated: false },
            FieldSpec { name: "offset", description: "Number of lines to skip from the start of the file (0-indexed, default 0)", ty: "integer", required: false, validated: false },
        ],
        chat_only: false,
    },
    ToolSpec {
        name: "Landscape",
        description: "This is your primary discovery tool — use it FIRST whenever you need to understand codebase layout, find where functions/types/tests live, or explore an unfamiliar directory. Show the structural outline of a file or directory. For a file, returns space-separated `line|kind|name` items on a single line. Kinds include fn, struct, impl, trait, enum, mod, class, method, heading, and key. Test functions inside `#[cfg(test)]` modules are shown individually. Unknown file extensions yield the file path. For a directory, returns a tree-formatted outline using the same `line|kind|name` item format: directory headers end with `/` and file headers end with `:` followed by space-separated items. The `depth` argument limits how many nested directory levels to descend (default: all levels). The optional `token_budget` argument applies skeletonization and depth back-off when the tree is too large; when omitted, the configured default budget applies (if any). Paths outside the sandbox are rejected.",
        fields: &[
            FieldSpec { name: "file_path", description: "Path to the file or directory to outline (relative or absolute)", ty: "string", required: true, validated: true },
            FieldSpec { name: "depth", description: "For directories, how many nested directory levels to descend (default: all levels)", ty: "integer", required: false, validated: true },
            FieldSpec { name: "token_budget", description: "Optional token budget for directory outlines. When set, the tree is trimmed with skeletonization and depth back-off to fit the budget.", ty: "integer", required: false, validated: true },
        ],
        chat_only: false,
    },
    ToolSpec {
        name: "Edit",
        description: "Make one or more replacements in a file. Edits apply sequentially, so later edits see the results of earlier edits in the same call — useful for chained renames. Each edit replaces old_string with new_string (requires unique match). Use start_line/end_line to scope the search to a line range when old_string appears multiple times (line-number scopes refer to the file state after all prior edits in the same call). After a failed edit due to non-uniqueness, re-read only the failing region (with offset/limit), not the whole file. All edits in the array target the same file, specified by the top-level `file_path` parameter — do not nest `file_path` inside individual edit objects.",
        fields: &[
            FieldSpec { name: "file_path", description: "Path to the file to edit (relative or absolute)", ty: "string", required: true, validated: true },
            FieldSpec { name: "edits", description: "", ty: "array", required: true, validated: true },
        ],
        chat_only: false,
    },
    ToolSpec {
        name: "Bash",
        description: "Execute a shell command via `bash -c`. Returns combined stdout and stderr. Exit codes are reported as `[exit N]`; commands exceeding the timeout return `[timeout]`. Runs in the gremlin's working directory with no stdin. Path arguments are checked against the sandbox before execution — escapes return an error without running the command. Default timeout is 120 seconds.",
        fields: &[
            FieldSpec { name: "command", description: "Shell command to execute (run via bash -c)", ty: "string", required: true, validated: true },
            FieldSpec { name: "timeout", description: "Timeout in seconds (optional, default 120)", ty: "integer", required: false, validated: false },
        ],
        chat_only: false,
    },
    ToolSpec {
        name: "Write",
        description: "Write content to a file, creating it if necessary. Overwrites the file if it already exists. Parent directories are created as needed.",
        fields: &[
            FieldSpec { name: "file_path", description: "Path to the file to write (relative or absolute)", ty: "string", required: true, validated: true },
            FieldSpec { name: "content", description: "Content to write to the file", ty: "string", required: true, validated: true },
        ],
        chat_only: false,
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
        chat_only: false,
    },
    ToolSpec {
        name: "Glob",
        description: "Find files matching a glob pattern. Supports `**` for recursive matching. Returns sorted paths, one per line.",
        fields: &[
            FieldSpec { name: "pattern", description: "Glob pattern (e.g. `**/*.rs`, `src/**/mod.rs`)", ty: "string", required: true, validated: true },
            FieldSpec { name: "path", description: "Base directory to search in (default: current directory)", ty: "string", required: false, validated: false },
        ],
        chat_only: false,
    },
    ToolSpec {
        name: "WebSearch",
        description: "Fetch a web page and extract its text via `curl -sL`. Returns the body text capped at a byte limit. Use this to look up documentation, check package versions, or read external references. Default timeout is 30 seconds.",
        fields: &[
            FieldSpec { name: "url", description: "The URL to fetch (e.g. https://example.com/page)", ty: "string", required: true, validated: true },
            FieldSpec { name: "timeout", description: "Timeout in seconds (optional, default 30)", ty: "integer", required: false, validated: true },
        ],
        chat_only: true,
    },
    ToolSpec {
        name: "Mischief",
        description: "Return the gremlins YAML DSL reference. Call with no arguments to get a complete reference for definition files, stage types, artifact URIs, interpolation, bootstrap, land, and stage definitions.",
        fields: &[],
        chat_only: true,
    },
    ToolSpec {
        name: "Done",
        description: "Signal that your work is complete — call Done(summary) alongside your final message to finish. The summary parameter briefly describes what you accomplished.",
        fields: &[
            FieldSpec { name: "summary", description: "Brief summary of what was accomplished", ty: "string", required: true, validated: true },
        ],
        chat_only: false,
    },
    ToolSpec {
        name: "Task",
        description: "Delegate a self-contained task to a worker. Non-Fork kinds start with a clean conversation context; the Fork kind inherits the parent conversation history. Multiple Task calls in the same message run concurrently, up to {TASK_MAX_PER_TURN} per message (the harness supports up to {MAX_CONCURRENT_TOOLS} total concurrent tool calls). Parallel Tasks launched in the same message share one working directory — each worker can see the other workers' in-progress changes. Give each parallel Task disjoint files or read-only work to avoid edit conflicts. Returns the worker's final text output.",
        fields: &[
            FieldSpec { name: "kind", description: "The kind of task to delegate.", ty: "string", required: true, validated: true },
            FieldSpec { name: "prompt", description: "Full task briefing for the worker. Non-Fork kinds start with a clean conversation context and see nothing from your conversation — include all relevant file contents, diffs, plans, and instructions the worker needs directly in this field. The Fork kind inherits the parent conversation history before this prompt.", ty: "string", required: true, validated: true },
        ],
        chat_only: false,
    },
    ToolSpec {
        name: "LoadSkill",
        description: "Load a chat skill by name. Use this when a task matches a skill's description from the available skills index. Returns the skill body and a listing of its bundled files. The skill is then active for the rest of the session.",
        fields: &[
            FieldSpec { name: "name", description: "Name of the skill to load", ty: "string", required: true, validated: true },
        ],
        chat_only: true,
    },
];

/// Used by `validate_tool_args` to produce rich error messages when
/// the model sends malformed tool-call arguments. The boolean marks whether
/// the parameter is required; optional parameters are type-checked only when
/// present. Fields are drawn from [`TOOL_SPECS`], keeping the advertised and
/// validated contracts in lockstep.
pub(super) fn tool_param_schema(name: &str) -> Option<Vec<(&'static str, &'static str, bool)>> {
    let spec = TOOL_SPECS.iter().find(|s| s.name == name)?;
    Some(
        spec.fields
            .iter()
            .filter(|f| f.validated)
            .map(|f| (f.name, f.ty, f.required))
            .collect(),
    )
}
