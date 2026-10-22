---
name: gremlin-analyze
description: Analyze a gremlin run for token efficiency and per-turn throughput. Inspects state.json, log telemetry, and the pipeline definition to find where tokens are wasted, turns are excessive, and stages spend too long waiting. Produces an actionable report with concrete changes to the definition, prompts, or harness.
---

Analyze a gremlin run to identify inefficiencies in token consumption and per-turn throughput. The
operator supplies a gremlin ID (or runs `gremlins ls` first) and you produce a report with concrete
recommendations.

## Workflow

### 1. Gather data

- **State**: `gremlins info <id>` — get the state directory path and definition name.
- **Token usage**: Read `state.json` from the state directory, extract the `token_usage` object
  (`prompt_tokens`, `completion_tokens`, `cached_input_tokens`, `cache_creation_input_tokens`,
  `reasoning_tokens`).
- **Logs**: `gremlins log <id>` — scan for per-turn telemetry. Each turn emits a JSON line:
  ```json
  {"stats":{"prompt_tokens":N,"completion_tokens":N,"cached_input_tokens":N,"cache_creation_input_tokens":N,"reasoning_tokens":N,"turn":K},"totals":{...cumulative...},"turn":K,"type":"turn_stats"}
  ```
  The `stats` object is the per-turn delta; `totals` is the cumulative total after that turn.
  The log also records `think:` (model reasoning), `tool:` (tool invocations with args),
  `result:` (tool results, truncated), and `text:` (model text responses) lines.
- **Aggregate usage from logs**: Search for all `"type":"turn_stats"` lines across stages and
  sum their per-turn `stats` deltas to cross-check against `state.json` totals.
- **Definition**: Read the `.gremlins/<name>.yaml` definition file. Map it out: stage types, prompts
  (full text of referenced `.md` files), artifact wiring, bootstrap commands, land commands.

### 2. Analyze

Run through each dimension below. For each finding, record the wasted tokens or wall-clock time
(where estimable).

#### A. System prompt bloat

The system prompt is assembled in `run.rs::assemble_system_prompt` and includes a landscape block
(a structural outline of the project). Check:

- `max_landscape_tokens` config (default 64 000): is the landscape block at or near the cap? Check
  the info log for a `[landscape]` line reporting file count / item count / token count. If the
  block is near or at the cap, the project tree is too large for the budget and the model is seeing
  a skeleton — suggest raising `max_landscape_tokens` or adding a `.gitignore`-style
  `.gremlinsignore` if the repo has large irrelevant subtrees.
- Repository context block: the untrusted metadata injected by the harness (branch, remotes, recent
  commits, top-level listing). Large commit histories or directory listings in the context block
  consume tokens every turn. If the block is verbose, suggest trimming via `.gremlins/config`.
- Skill index size: long skill descriptions or many skills. If the index is large, suggest shorter
  descriptions or `disable-model-invocation: true` for skills the model rarely self-selects.
- Active skill bodies: loaded skills inject their full body into the system prompt. If a skill is
  loaded but not relevant to the current task, suggest the operator `/clear` and reload only what's
  needed.

#### B. Prompt efficiency (agent stages)

- **Prompt file size**: Read each `.md` file referenced by agent stage prompts. For each:
  - Count estimated tokens (roughly chars ÷ 3 for a lower-bound estimate).
  - Flag prompts over ~2 000 tokens: every turn pays this cost.
  - Does the prompt repeat information already in the system prompt (landscape, skills, repository
    context)? Suggest deduplication.
  - Does the prompt include a full file listing that the model could discover via `Landscape`?
  - Does the prompt instruct the model to use `Read` when `Landscape` would be more
    efficient? `Landscape` returns a structural outline in far fewer tokens. Suggest replacing
    "read the file" guidance with "use Landscape to explore" where appropriate.
- **Artifact inlining**: `content("artifact://...")` inlines entire file contents into the prompt.
  Check whether large artifacts are being inlined unnecessarily. Could they be referenced by path
  instead and read on demand?

#### C. Tool call patterns (from logs)

- Look at `tool:` and `result:` lines to reconstruct the call pattern.
- **Concurrent vs serial calls**: Check whether multiple `tool:` lines appear at the same
  timestamp without an intervening `result:` — these are concurrent (batched) calls. If most
  turns have single tool→result→tool→result chains, the model isn't batching.
- **Landscape vs Read**: If the log shows many `Read` calls on files that could have been explored
  with `Landscape` first, the model is spending tokens on full file contents. Suggest adjusting the
  prompt to surface `Landscape` as the primary discovery tool.
- **Done tool usage**: `Done` signals turn completion. If the log shows turns ending without
  `Done` (e.g. plain text responses), the model is wasting a turn boundary. Flag this.
- **Task tool usage**: `Task` delegates work to sub-agents. If the log shows a single agent doing
  all the work without delegating, suggest breaking the prompt into parallel `Task` calls.
- **Redundant reads**: If the same file is read across multiple consecutive turns, the model may
  not be retaining context. Suggest compaction threshold adjustments or prompt guidance to cache
  findings.

#### D. Cache efficiency

- **Cache hit ratio**: `cached_input_tokens / prompt_tokens`. Low ratios mean the provider cache
  is not being used effectively. Common causes:
  - The system prompt changes between turns (skills loaded mid-session, turn-boundary amendments).
  - `CHAT_SYSTEM_PROMPT` prefix is stable but dynamic content after it shifts — suggest keeping
    the prompt prefix stable and adding guidance to batch skill loads at session start.
  - Large conversation histories that exceed the provider's cache window.
- **Cache creation cost**: `cache_creation_input_tokens` reports how many tokens were written
  into the cache. If this is high relative to `cached_input_tokens`, the cache is being rebuilt
  frequently rather than reused. Check whether tool results are varying the prompt shape.
- **Reasoning token ratio**: `reasoning_tokens / completion_tokens`. For reasoning models, high
  reasoning ratios may indicate the model is overthinking simple tasks. Consider switching to a
  non-reasoning model for straightforward stages (e.g. exec wrappers, format conversions).

#### E. Pipeline structure

- **Stage count vs utility**: Does every stage pull its weight? Flag stages that:
  - Run `git add -A && git commit` but produce no diff (wasted exec).
  - Recompute data that an earlier stage already produced (missing `skip_if_exists`).
  - Pass large artifacts between stages when only a summary is needed.
- **Parallel opportunities**: Are there agent stages that are independent and could run in
  `parallel`? Example: multiple code reviews of the same diff. Parallel stages reduce wall-clock
  time but increase total token spend — note the trade-off.
- **Sequence loop efficiency**: For `sequence` stages with `max-iterations > 1`:
  - Does the loop converge early via `skip_if_exists` / convergence markers? If not, suggest
    adding a convergence check.
  - Does each iteration re-read or re-compute context that hasn't changed? Suggest caching
    stable context before the loop.
- **Exec stage overhead**: Exec stages that invoke `gremlins-drag`, `cargo check`, or other
  build tools may dominate wall-clock time. Check whether they could run in parallel with
  other stages or whether results could be cached across iterations.

#### F. Conversation compaction

Gremlins compacts the conversation history when it exceeds the provider's context window.
Check the log for compaction events. If the conversation is being compacted frequently:
- The model may be losing context and re-asking for files it already read.
- Suggest increasing `chat_compaction_threshold_bytes` or adjusting `max_prompt_bytes`.
- Alternatively, suggest breaking a long agent run into multiple shorter stages.

### 3. Produce the report

Output a markdown report with sections:

```
## Gremlin Analysis: <definition> (<id>)

### Token Summary
| Metric | Value |
|---|---|
| Total prompt tokens | ... |
| Total completion tokens | ... |
| Cached tokens | ... (X% hit rate) |
| Cache creation tokens | ... |
| Reasoning tokens | ... (Y% of completion) |
| Total turns | ... |
| Wall-clock time | ... |

### Top Recommendations (by token savings)

1. **<Title>** — ~N tokens saved per run
   - Current: ...
   - Change: ...
   - How to implement: ...

2. ...

### Detailed Analysis

#### System Prompt
...

#### Prompt Efficiency
...

#### Tool Call Patterns
...

#### Cache Efficiency
...

#### Pipeline Structure
...

### Summary of Changes

| Change | Est. Token Savings | File(s) to edit |
|---|---|---|
| ... | ... | ... |
```

### Bounds

- **Only analyze one gremlin at a time.** The operator specifies the ID.
- **Prioritize by impact.** Sort recommendations by estimated token savings.
- **Be concrete.** Every recommendation must name the specific file, line, or stanza to change.
- **Don't propose rewrites without evidence.** Show the numbers before making claims.
- **The definition file is read-only for analysis — don't edit it unless the operator asks.**
- If the operator says "fix it" or "apply", edit the definition file and referenced prompts.