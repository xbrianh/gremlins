# Remediation Report: `oof.md` Recommendations 3, 4, 5

> Analysis of three diagnostic recommendations from the `gremlin-10851d` implement-stage postmortem.
> All line numbers reference `crates/gremlins/src/clients/tools.rs` unless otherwise noted.

---

## 3. Pre-validate Tool Schemas

### Problem

The agent made **six consecutive `Edit` calls** all failing with the same `Error: missing 'edits'`, retrying identically before eventually correcting the call shape. Each failed attempt burned a full model round-trip (think → tool call → response → think).

### Root cause

The `Edit` tool definition sends a full JSON schema to the model (lines 1327–1348):

```json
{
  "type": "object",
  "properties": {
    "file_path": {"type": "string"},
    "edits": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "properties": {
          "old_string": {"type": "string"},
          "new_string": {"type": "string"}
        },
        "required": ["old_string", "new_string"]
      }
    }
  },
  "required": ["file_path", "edits"]
}
```

However, this schema is **never validated against the model's actual tool-call arguments**. The dispatch path is:

1. `invoke()` (line 1247) — calls `parse_args(args_json)` which deserializes into a lenient `HashMap<String, Value>`. No type or required-field checking.
2. `check_tool()` (line 658) — only validates **path containment** (`enforce` / `bash_check`). No schema checking.
3. `edit_sync()` (line 799) — the handler extracts fields manually:

```rust
let edits_arr = match args.get("edits").and_then(|v| v.as_array()) {
    Some(a) => a,
    None => return "Error: missing 'edits'".into(),  // ← terse, no schema hint
};
```

If `edits` is omitted, or is a string instead of an array, or the inner objects lack `old_string`/`new_string`, the model gets a bare `Error: missing 'edits'` — no information about **what the correct shape is**. The model has to guess what was wrong.

### Proposed fix

Add a schema-validation step in `invoke()` or `check_tool()` that cross-checks the received arguments against the tool's JSON schema **before** dispatching to the handler. On failure, return an error message that includes the expected structure:

```
Error: invalid arguments for Edit tool.
Expected: {"file_path": <string>, "edits": [{"old_string": <string>, "new_string": <string>}, ...]}
Got: file_path="foo.py", edits=<missing>
```

**Implementation sketch** (~30 lines in `invoke`):

```rust
// In invoke(), after parse_args but before check_tool:
if let Some(schema_err) = validate_against_schema(name, &args) {
    return schema_err;
}

fn validate_against_schema(name: &str, args: &HashMap<String, Value>) -> Option<String> {
    let schema = get_tool_schema(name)?;  // look up the ToolDefinition.parameters
    // Check all `required` keys are present
    // Check types match (string → string, array → array, etc.)
    // On mismatch: format("Error: invalid arguments for {name}. Expected: {expected}. Got: ...")
}
```

**Impact:** One retry instead of six. Saves ~5 model round-trips per schema violation. The schema is already defined — this just connects it to the validation path.

---

## 4. Sandbox Path False Positive

### Problem

The command `cd /workdir && for f in $(grep -rl pattern /); do ...; done` was rejected with:

```
Error: path outside sandbox (from /): / → sandbox roots: [/worktree, ...]
```

The same `cd /workdir && grep -rl pattern .` pattern had worked fine ~20 times earlier.

### Root cause

`bash_check()` (line 525) uses `shell_tokenize()` (line 362) to extract path-like tokens from the command string. `shell_tokenize` is a **whitespace-only tokenizer** with no shell grammar. It does not understand:

- `$(...)` command substitution
- `` `...` `` backtick substitution
- `for`/`in`/`do`/`done` keywords
- `|`, `;`, `&&`, `||` separators

Every whitespace-delimited word becomes a `ShellToken`. Any token that starts with `/`, `~`, `..`, or contains `/` is treated as a path and resolved against sandbox roots (line 553).

The offending command contains `$(grep -rl pattern /)` — the bare `/` inside the subshell is tokenized as a standalone token. It starts with `/` → flagged as a path → resolves to root `/` → fails sandbox check.

The `(from /)` in the error message is `tok.raw` (the verbatim token text). The second `/` is `tok.value` (with quotes/escapes resolved).

### Broader class of false positives

Because `shell_tokenize` has no shell grammar, **any path-like token inside a shell construct** triggers a false positive:

| Pattern | False-positive token(s) | Why |
|---|---|---|
| `$(grep -rl foo /)` | `/` | Bare root in subprocess arg |
| `` `find /tmp -name '*.rs'` `` | `/tmp` | Path inside backtick expansion |
| `sed 's/foo/bar/'` | `s/foo/bar/`? | Pre-`/` heuristic catches it? Maybe not since it doesn't start with `/` |
| `sed 's|/tmp|/var|g` | `/tmp`, `/var`? | single quotes are handled, tokens resolved |
| `for f in /some/path/*; do` | `/some/path/*` | Path in for-loop glob |
| `echo 'some / output'` | / output? | tokens inside single quotes |

### Proposed fix

**Strip command substitutions before tokenizing.** The simplest and most targeted fix:

```rust
pub(crate) fn bash_check(roots: &[PathBuf], cmd: &str, cwd: Option<&Path>) -> Option<String> {
    let s = cmd.trim();
    // ... existing guards ...

    // Strip $(...) and `...` content before tokenizing — paths inside
    // command substitutions are runtime-expanded by the shell and are
    // never literal path arguments from the agent.
    let cleaned = strip_subshells(s);

    let canonical_roots = /* ... */;
    for tok in shell_tokenize(&cleaned) {
        // ... existing path heuristic ...
    }
    None
}

fn strip_subshells(s: &str) -> String {
    // Remove $(...) blocks (handling nesting depth)
    // Remove `...` backtick blocks
    // Replace with a placeholder that's not path-like
}
```

This does not weaken security: `$()` content is shell-expanded at runtime, not by the tokenizer, so checking tokens inside it is always a false positive. The real containment boundary is `io_enforce` on file tools.

**Impact:** Eliminates the most common class of sandbox false positives. Prevents wasted turns when the model uses compound shell commands with subshells.

---

## 5. Route Exploration Through Dedicated Tools

### Problem

The main agent used **zero** `Read`, `Grep`, or `Glob` calls during the entire visible ~18-minute window. All 38 exploration/read/search operations went through `bash` (grep, sed, cat, find). This caused three self-inflicted errors:

1. **BSD sed `\b` incompatibility** (18:11:43) — `sed -i '' 's/\bStage\b/Any/'` silently no-op'd because BSD sed lacks `\b`. The `Edit` tool or `Grep` tool would not have had this issue.
2. **Shell quoting bug** (18:19:59) — `grep` failed with `unexpected EOF while looking for matching '` (unbalanced backtick in the pattern). The `Grep` tool takes a JSON string, avoiding shell quoting entirely.
3. **Sandbox false positive** (18:10:44) — `cd <abs> && for f in …` triggered the path check. The `Grep` tool uses `io_enforce` on a directory path, cleanly avoiding the `bash_check` tokenizer issue.

The dedicated tools are **safer, faster, and better-integrated** than raw shell equivalents:

| Feature | `Grep` tool | `bash grep -r` |
|---|---|---|
| Containment | `io_enforce` (canonical, follows all symlinks) | `bash_check` (lexical, heuristic, fail-open on leaf symlinks) |
| Respects `.gitignore` | Yes (skips `__pycache__`, `node_modules`, `target`) | No |
| Glob filtering | Built-in `glob` parameter (fnmatch) | Manual `--include`/`--exclude` |
| Audit trail | JSONL audit log | Generic bash line |
| Regex correctness | Rust `regex` crate (same everywhere) | Platform-dependent (BSD vs GNU) |
| Quoting safety | JSON args | Shell escaping required |
| Truncation | 2000-line output cap | Risk of hanging on large output |

### Why the agent ignored them

The system prompt (`crates/gremlins/src/config.rs:12–35`) says:

> Keep your context lean: delegate self-contained piece of work to a subagent. … parallel is cheaper than serial drift.

And lists writable directories. **It never mentions Read, Grep, Glob, Edit, or Write by name.** The model discovers tools solely through the API's `tools` parameter — a JSON schema array with no reinforcing text. The system prompt is silent about the tool roster.

The model defaults to familiar Unix commands because its training data is saturated with `grep -r`, `cat`, and `sed -n`. It doesn't know that `Grep` has `.gitignore` awareness or that `Read` avoids the BSD sed pitfall — nothing tells it.

### Proposed fix

**Two complementary changes, both respecting the "no behavioural opinions in the harness" convention:**

#### A. Tool roster in system prompt (metadata, not opinion)

Add to `agent_system_prompt` in `crates/gremlins/src/config.rs`:

```
Available tools: Read, Write, Edit, Grep, Glob, Bash, subagent, parallel.
Grep and Glob skip .gitignore'd directories and use strict path containment.
Read reads files directly without shell escaping. Edit does targeted,
validated replacements.
```

This is factual API documentation — it describes what tools exist and their properties. It does not tell the model what to do. The model still decides.

#### B. Pipeline-level guidance (opinion where it belongs)

In `.gremlins/prompts/implement.md` or the bundled stage YAML (`crates/gremlins/src/assets/data/stages/implement.yaml`), add:

```
Prefer Grep for regex search (it skips .gitignore'd dirs and avoids
platform-dependent grep quirks). Use Glob instead of find for file
discovery. Use Read instead of cat/sed for file inspection. Use Edit
for targeted changes instead of sed -i.
```

This keeps the behavioral preference in the pipeline's own prompt, where the pipeline author owns it. The harness remains unopinionated.

**Impact:** The model becomes aware that `Grep` and `Glob` exist and have advantages. Combined with the tool roster, this eliminates the platform-specific Bash errors that dominated the diagnostic report, and cuts exploration latency (Grep is a single tool call instead of Bash grep + shell overhead).

---

## Implementation Priority

| Priority | Item | Effort | Impact |
|---|---|---|---|
| **1** | Strip `$()` / backticks before `shell_tokenize` (#4) | ~20 lines Rust | Eliminates sandbox false positives for compound commands |
| **2** | Tool roster in system prompt (#5-A) | ~5 lines Rust | Model becomes aware tools exist; no behavioural change |
| **3** | Schema validation in `invoke` (#3) | ~30 lines Rust | Prevents serial identical retries on malformed tool calls |
| **4** | Pipeline-level tool guidance (#5-B) | ~3 lines Markdown | Nudges agent toward safer tools in the pipeline's own voice |

All changes are in `crates/gremlins/src/clients/tools.rs` and `crates/gremlins/src/config.rs`. No Python-side changes required.