# Gremlins — System Design

This document describes how the gremlins workflow system is put together: where
we use deterministic code, where we delegate to a model, how context flows (and
is deliberately *not* shared) between stages, and the cost model that follows
from those choices.

It is not a reference for individual modules — see the per-package `AGENTS.md`
files for that. It is the rationale you need to evaluate proposed changes
without re-deriving the trade-offs each time.

## 1. The shape of a gremlin

A gremlin is a sequence of **stages** executed by a thin orchestrator. The
sequence is described in a YAML pipeline (`gremlins/pipelines/local.yaml`,
`gh.yaml`, optionally a project-scoped override at `.gremlins/pipelines/`).

A typical pipeline looks like this:

    plan → implement → review-code → address-code → verify → github-open-pull-request → ...

Each stage is one of two kinds:

- **Deterministic stages** are plain Python. They run shell commands, talk to
  `gh`, manage worktrees, parse JSON, wait on CI. They do not invoke a model.
  Examples: `verify`, `request-copilot`, `github-wait-copilot`, `github-wait-ci`.
- **Agentic stages** invoke `claude -p` via an injected `ClaudeClient`. They
  receive a prompt assembled from pipeline-declared prompt files, run to
  completion, and produce an artifact on disk (a markdown file, a commit, a
  PR comment). Examples: `plan`, `implement`, `review-code`, `address-code`,
  `github-review-pull-request`, `github-address-pull-request-reviews`. Most agentic stages invoke the model exactly
  once; the two self-healing stages described in §2.2 are the exception.

The orchestrator (`runner.run_stages`) is responsible for sequencing,
`--resume-from <stage>` semantics, and SIGINT/SIGTERM reaping of live `claude`
children. It is *not* responsible for any decision the model could plausibly
make better.

## 2. Determinism and agency

Gremlins run unattended — sometimes overnight, sometimes in chains of a
dozen. The system has to be robust against model misbehavior, transient
infrastructure failures, and operator interruption. That shapes where we
allow agency and where we refuse to.

**Use deterministic code for:**

- The sequence itself. The pipeline YAML is the contract; stages do not get
  to decide what runs next. This is what makes resumption,
  rescue-after-bail, and chained-boss workflows tractable — the operator
  always knows what stage a gremlin is in, and what comes after, by reading
  one file.
- Anything observed by another process. Stage names, bail classes, and the
  marker-protocol bail reasons are byte-stable strings. They are written to
  `state.json` and read by the launcher, the fleet manager, the rescue
  protocol, and shell hooks. A model rewording any of these would silently
  break cross-process consumers, so they live in Python constants and YAML
  rather than in prompts.
- Filesystem and git mechanics. Worktree creation, branch handling, commit
  authorship, PR opening — these are deterministic helpers. The model writes
  *content* (a commit message, a plan, a code change) but does not run `git`
  itself.
- Bookkeeping. `state.set_stage` and `state.emit_bail` write atomically and
  never raise. A gremlin that crashes mid-stage must leave behind a state
  file the rescue protocol can interpret.

**Delegate to a model for:**

- Reading code and forming a plan from an issue or a free-text prompt.
- Making the code change.
- Reviewing a diff against a lens.
- Deciding which review findings are worth addressing and editing the code
  accordingly.
- Writing a commit message, a PR description, or a reply to a reviewer
  comment.
- Chain-step decisions: the `handoff` agent decides whether a boss chain is
  done, and if not, what the next child should plan.

The dividing line is consistent: **the model produces content; deterministic
code moves it around.** A stage that needed a model to decide *whether to run*
would be a sign the pipeline was modeled wrong.

### 2.1 Why a YAML pipeline rather than an agent loop

We could build this as a single long-lived agent that reads tools, makes
decisions, and produces a PR — and we considered it. We don't, for three
reasons:

1. **Resumability.** A pipeline with named stages and a `state.json` cursor
   can be resumed from any stage by an operator or a rescue script. A
   single agent loop has no equivalent — its "stage" is whatever its scratchpad
   says it is.
2. **Observability.** Stage transitions are logged events that downstream
   tools (`gremlins` status, `fleet/`, the session-summary hook) consume.
   An agent loop's progress is opaque without parsing its transcript.
3. **Cost predictability.** Each stage has a bounded prompt and a bounded
   workspace; per-stage cost is roughly stable for the one-shot stages
   (and bounded by `--test-max-attempts` for the self-healing two). An
   agent loop's cost is a function of how long it stays interested,
   which is not a property we want to discover in production.

The pipeline is the deterministic skeleton; agency is intentionally confined
to one stage at a time.

### 2.2 Self-healing stages

Two stages — `verify` and `wait-ci` — embed an agent retry loop inside the
stage body. They run a deterministic check (a test command, a CI status
poll), and on failure invoke a fixer agent against the failure output, then
re-run the check. Up to `--test-max-attempts` iterations per stage call.

This is a deliberate exception to the "one agent invocation per stage"
shape implied above. The justification is that the artifact these stages
produce is *the green check itself*: the loop's exit condition is a
deterministic re-run, the number of fix attempts isn't known up front, and
from the outside the stage still either produces its artifact or bails.
Splitting the loop across pipeline stages would require either loop
semantics in the pipeline YAML (a much larger change) or unrolled stages
that decide whether to skip — which §2 explicitly forbids.

The cost of this exception:

- §5's "per-stage cost is roughly stable" does not hold for these two.
  Cost scales with the number of fix attempts and the size of the
  accumulated check output. Per-attempt streams are written to the
  session directory (`stream-verify-N.jsonl`, `verify-attempt-N.log`)
  so post-hoc cost analysis can resolve a loopy stage from a one-shot
  one; the per-stage `total_cost_usd` rollup cannot.
- A self-healing stage is the one place where an agent invocation sees
  content the *same stage* produced earlier — the failing check output
  following its own fix. The isolation rules in §3 still hold across
  stages; they bend within these two.

We accept the exception because the alternatives are worse and the set
is closed: exactly two stages, both deterministic-check-plus-fixer, and
we don't expect a third.

### 2.3 Bail as a control-flow channel

A stage can halt the pipeline by raising a `Bail` exception or by calling
`state.emit_bail`, which writes a `bail_class` (and optional `bail_detail`)
to `state.json`.

The two routes serve different jobs:

- **`Bail` exception** is raised when a structured bail condition is detected.
  A `Bail` exception includes the `bail_class` (one of
  `reviewer_requested_changes`, `security`, `secrets`, `other`). The exception
  propagates up and halts the pipeline.
- **`emit_bail`** records a *structured*, *persistent* halt reason
  in `state.json`. Both `bail_class` and `bail_detail` (a one-line human note)
  live in `state.json` after the process exits.

The persistence is the point. `bail_class` is read by the rescue
protocol (§4.3), the fleet manager, the boss recovery table, and shell
hooks — exactly the cross-process consumers §2 says we serve with
byte-stable strings rather than prose. A stage that only raises tells a
human; a stage that calls `emit_bail` first also tells a *script*.

`emit_bail` does not itself halt the pipeline. It writes the marker and
returns; the caller raises immediately afterward (either explicitly or by
allowing a `Bail` exception to propagate), or an in-stage agent
invokes `python -m gremlins.bail` and the stage's normal exit-code
handling raises on its behalf. The pairing — write the marker, then
raise — is the pattern. The marker outlives the raise.

`Bail` is raised when a structured bail condition is detected (e.g., when
checking `state.json` for a recorded `bail_class` in stages that follow
soft-failure points like `github-address-pull-request-reviews` and
`github-review-pull-request`, or in the self-healing stages (§2.2) after
each fixer agent runs). This allows a stage to halt cleanly when an agent
has already bailed via `python -m gremlins.bail` without the stage having
to inspect agent output.

`run_stages` does not explicitly catch `Bail`; any unhandled exception ends
the run and is logged. `Bail` exceptions are caught in specific contexts
(e.g., parallel fan-in) to aggregate and handle multiple concurrent bails
before re-raising.

## 3. Context management

Context is the central design constraint. The cheapest, most reliable, and
most reproducible run is the one where each agent gets exactly the
information it needs and nothing else. We push hard on this.

### 3.1 Stages do not share an in-memory context

Every agentic stage starts a **fresh `claude -p` subprocess** with a
**fresh model context**. There is no shared scratchpad, no in-memory history
threaded between stages, no rolling summary. When `address-code` runs after
`review-code`, it does not inherit anything from the review process — it gets
the same starting context any cold invocation would, plus the artifacts the
review wrote to disk.

This is a deliberate constraint, not an oversight:

- It bounds the per-stage prompt to something we can reason about.
- It makes stages independently testable — a stage's behavior is a function
  of its prompt and the worktree, not of a hidden history.
- It forces communication between stages to go through **artifacts** —
  files that are also useful to the operator: `plan.md`,
  `review-code-*.md`, the commit itself, the PR description.
- It makes `--resume-from <stage>` semantically clean. Resuming from
  `address-code` means re-running `address-code` with whatever artifacts
  exist on disk; there is no "but the previous run's reviewer was thinking
  about X" hidden state to recover.

### 3.2 Prompts are composed, not inherited

A stage's prompt is the concatenation of:

1. Prompt files declared in the pipeline YAML (`prompts/code_style.md`,
   `prompts/implement_local.md`, etc.). These are pinned per-pipeline.
2. The artifacts produced by upstream stages, read from disk and embedded
   into the prompt by the stage body.
3. The minimum task framing the stage needs to do its job.

A reviewer does not see the planner's prompt. The implementer does not see
the reviewer's lens. Each stage is given its own job in its own words, and
upstream output crosses the boundary as data, not as context.

### 3.3 The worktree is the workspace

Every gremlin runs in its own git worktree. The agent inside a stage uses
its own tools (Read, Edit, Bash) to navigate that worktree; the orchestrator
does not pre-load files into the prompt. This keeps the prompt small even
for large codebases — the agent only loads what it actually needs — and it
means the same prompt scales from a 100-file repo to a 10,000-file repo
without modification.

The cost of this is a bit of redundant exploration: `implement` re-reads
files that `plan` already read. We accept that cost (see §5) because the
alternative — pre-loading the union of files plan touched — would couple
the stages together, defeat resumption, and bloat the prompt.

### 3.4 Across-gremlin context isolation

Different gremlins share nothing. Different `gremlin_id`s have different
worktrees, different `state.json` files, different log directories. A boss
chain coordinates child gremlins by reading their `state.json` files, not
by sharing context with them. This is what lets a boss recover from a child
bail without inheriting any of the child's confusion.

### 3.5 Parallel stages

A `type: parallel` block in a pipeline YAML runs N children concurrently.
At runtime the block materialises as **three stages**, keeping §2's
deterministic-vs-agentic line intact:

- **`<group>-fanout`** (deterministic). Creates per-child artifact
  subdirs and per-child git worktrees, each a detached checkout of the
  current branch tip. Runs `git worktree prune` first to clear leftovers
  from any previous interrupted run.
- **`<group>`** (agentic, N concurrent). Runs N `claude -p` invocations in
  a thread pool, each in its own `StageContext` with its `child_key` and
  the worktree path from fan-out. Children write `bail_class` and
  `bail_detail` into `state.json` under `parallel_bails[child_key]`, never
  into the top-level bail slot, so children cannot see each other's bails.
  `check_bail` called with a `child_key` reads only that child's shard.
- **`<group>-fanin`** (deterministic). Reads `parallel_bails`, applies the
  block's `bail_policy`, promotes a bail to the top-level `bail_class` if
  warranted, raises `Bail` if needed, clears `parallel_bails`, and tears
  down all per-child worktrees with `git worktree remove --force` +
  `git worktree prune`. Fan-in is also responsible for cleanup on crash —
  it runs teardown in a `try/finally` so worktrees don't accumulate from
  aborted runs.

This decomposition fixes two latent bugs in the prior single-stage
parallel wrapper:

- **Lost bail.** `patch_state` did a read-modify-write without a lock.
  Concurrent `emit_bail` calls raced; last writer won. The fix is twofold:
  `patch_state` now holds an exclusive `fcntl.flock` on a per-`state.json`
  lock file for the duration of each read-modify-write, and child bails go
  into `parallel_bails[child_key]` rather than the shared top-level slot.
- **Bail cross-contamination.** Bail exceptions are scoped to per-child
  bails stored in `parallel_bails[child_key]`. A parallel child completing
  after a sibling bailed would not falsely report itself as bailed because
  bail detection is child-specific.

Both fixes are backward-compatible: `child_key=None` (the default, used by
all sequential stages) preserves existing top-level bail semantics.

**Per-block knobs** (declared on the parallel block in the pipeline YAML):

- `cancel_on_bail: false` (default). All children run to completion even if
  one bails. Right for review lenses where each lens is independent.
  Set to `true` for parallel implementers where a structural bail by one
  child makes the others irrelevant — on first bail a cancel flag is set
  and children that have not yet started are skipped.
- `bail_policy: any` (default). Any bailing child causes the group to bail
  after fan-in. Set to `all` to require every child to bail before the
  group bails. The top-level `bail_class` is populated from the first
  bailing child's shard.

**Worktrees are always-on.** Every parallel child gets its own worktree,
regardless of whether it mutates. The cost — one full working-tree checkout
per child, with object storage shared via `.git/worktrees/` — is small
relative to gremlin runtime. Unconditional worktrees remove a flag and a
code path: read-only and mutating parallel are architecturally identical;
the only difference is what the children write and what fan-in does with it.

**The merge problem is unsolved.** Fan-in for blocks whose children mutated
their worktrees raises `NotImplementedError`. Deciding what to do when N
agents each produced a different diff — pick the best, merge all,
cherry-pick — requires a concrete use case before the right shape is clear.
The current parallel use (review lenses) is read-only; it does not hit this
path.

**Resumability.** The three-stage decomposition makes resume targets
explicit:

- `--resume-from <group>-fanout`: re-create slots and run end-to-end.
- `--resume-from <group>`: rerun all children from cold worktrees (fan-out
  must have already run). Do not try to skip "already-completed" children —
  partial state from a prior run is the in-memory-context-leak §3 forbids.
- `--resume-from <group>-fanin`: re-aggregate whatever shards exist without
  rerunning workers. The clean win when workers finished but fan-in crashed.

## 4. Boss gremlins and chained workflows

A single gremlin produces one PR from one plan. Many real tasks don't fit
that shape — they are sequences of related changes that have to land in
order, where each step's plan depends on what the previous step actually
did. The **boss gremlin** is the pattern for those.

A boss is itself a long-running process, but it is not a stage pipeline
in the §1 sense. It runs a loop:

    1. Decide what the next child should do (handoff agent).
    2. Launch a child gremlin with that plan.
    3. Wait for the child to finish.
    4. Land the child's PR (or recognize an externally-landed one).
    5. Goto 1, until the handoff agent says the chain is done.

The boss's own state lives in `boss_state.json`, separate from any child's
`state.json`. Children are ordinary `local` or `gh` gremlins — the boss
doesn't run them in-process; it spawns them through the same launcher an
operator would, with their own worktrees, their own logs, their own
lifecycles. From a child's perspective there is no boss; it just has a
plan and runs the pipeline.

Boss resumption is keyed off `boss_state.json`, not the pipeline stage
vocabulary. The shared launcher resume path still tracks `state.json.stage`
for fleet status, but it does not pass `--resume-from` when re-spawning a
boss. If a caller does provide `--resume-from`, `boss_main` logs that the
flag is being ignored and resumes from the chain cursor in `boss_state.json`.

### 4.1 Where the agency lives

The boss reuses the §2 dividing line, applied at a different scale:

- **Deterministic:** spawning children, polling for completion, landing
  PRs, writing `boss_state.json`, parsing children's `state.json`,
  deciding when the chain has structurally stalled.
- **Agentic, exactly once per step:** the **handoff agent**
  (`gremlins/stages/handoff.py`). It reads the rolling plan, the chain spec, and
  the diff accumulated on the branch, and produces one of three
  decisions: `next-plan` (here is the plan for child N+1), `chain-done`
  (we are finished), or `bail` (something is structurally wrong, stop and
  ask the operator).

Everything else in the boss loop is plain Python. Notably, the boss does
*not* use a model to decide whether a child succeeded — it reads the
child's `state.json`. This is the same byte-stable-strings discipline
from §2 applied across the parent/child boundary.

### 4.2 Context isolation across the chain

Children inherit nothing from each other in-memory. The chain accumulates
context the same way stages within a single gremlin do (§3): through
**artifacts** — landed commits on the shared branch, an updated rolling
plan file, the chain spec. The handoff agent reads those artifacts to
decide step N+1; child N+1 then runs cold against the post-step-N
worktree.

This is what makes the chain resumable. A boss can be killed and rescued
mid-chain. The operator can stop a child, edit its PR by hand, and tell
the boss to continue. None of that requires reconstructing in-memory
context, because there isn't any — every decision is a function of files
on disk and `state.json` cursors.

### 4.3 The child-bail recovery protocol

When a child bails, the boss halts and the operator decides what
happened. There are three operator commands, each writing one
unambiguous fact to the child's `state.json`:

- `gremlins resume <child-id>` — re-spawn the bailed child at its bail
  point. The child's work is still in flight; the operator pushed a fix
  or edited the worktree.
- `gremlins ack <child-id>` — assert the child's work is already in
  main. Writes `external_outcome=landed`. Used after a manual merge.
- `gremlins skip <child-id>` — give up on the child's plan. Writes
  `external_outcome=abandoned`. The handoff agent will plan something
  different.

The boss's rescue logic is then a deterministic table lookup on the
child's recorded state. If the operator hasn't recorded a decision, the
boss prints the three options and exits non-zero — it never silently
re-handoffs and spawns a near-duplicate child. This is a deliberate
design choice: ambiguity at the chain level is surfaced to the operator
rather than papered over by another model call.

### 4.4 Why a boss isn't just a longer pipeline

We could express boss workflows as a single longer YAML pipeline with
many `plan → implement → review-code → ...` repetitions. We don't,
because:

- The number of steps isn't known up front. A real chain ends when the
  feature is done, not at a step count we picked yesterday.
- Each step's plan is a function of the previous step's diff. That's an
  agentic decision (the handoff agent), and putting it inside a stage
  pipeline would mean a stage that decides whether the next stage runs
  — which §2 forbids.
- Children need to be independently rescuable, landable, and abandonable
  by an operator. That works because each child is a separately
  launched gremlin with its own state file. A flattened pipeline would
  collapse them into one process and lose the granularity.

The boss is the right abstraction precisely because it stays out of the
child's pipeline and confines its own agency to one decision per step.

### 4.5 PR stacking in looped pipelines

When a pipeline contains a `loop` stage and that loop body includes an
`github-open-pull-request` stage, every PR after the first is automatically based on
the previous PR's branch. No per-pipeline configuration is required.

**The mechanism.** `GitHubOpenPullRequest.run` resolves the PR base ref with this
fallback chain:

    last_pr_branch(gremlin_id)  →  stage base_ref option  →  state.json base_ref_name  →  "main"

`last_pr_branch` walks the artifact list in reverse and returns the `branch`
field of the most recent `pr`-type artifact. Because all loop iterations share
one `gremlin_id` and one `state.json`, every iteration after the first finds the
previous iteration's PR branch at the head of the artifact list.

**The invariant.** `github_open_pull_request.py` raises if `impl_materialized_branch`
is empty before appending a PR artifact, so every `pr` artifact in the list
has a non-empty `branch` field. `last_pr_branch` therefore always returns a
real branch name, never an empty string that would fall through to `main`.

**What this means in practice.** A boss pipeline (or any looped gh pipeline)
produces a stack of PRs by default. PR #1 targets `main` (or
`base_ref_name` from state). PR #2 targets PR #1's branch. PR #3 targets
PR #2's branch, and so on. The artifact list is the authoritative record.

**The escape hatch.** Stacking is a consequence of loop iterations sharing
one `gremlin_id`. To produce side-by-side PRs based on a fixed ref, launch each
iteration as an independent gremlin rather than as loop iterations in a
single run — each gets its own `gremlin_id` and an empty artifact list, so
`last_pr_branch` returns nothing and the PR targets `main` (or whatever
`base_ref_name` is in that gremlin's state).

Within a looped pipeline you can also set `base_ref` under the
`github-open-pull-request` stage's `options:` to control the first-iteration base and
the fallback when the artifact list is empty, but this does not suppress
stacking once prior PR artifacts exist.

## 5. Cost model

Per-gremlin cost is dominated by two things:

- **Token volume per stage.** Driven by prompt size + how much the agent
  reads from the worktree. Bounded by §3 — small prompts, scoped agents.
- **Number of stages × per-stage volume.** Bounded by the pipeline YAML.

We measure cost per run via `CompletedRun.cost_usd`, summed across stages
into `SubprocessClaudeClient.total_cost_usd`. That number is the unit we
optimize against.

The cost knobs we *do* use:

- **Model selection per stage.** The pipeline's `clients` block lets a
  stage pick a smaller model. We default everything to Sonnet and would
  drop individual stages to Haiku only with a measured reason.
- **Prompt size discipline.** Prompt files are reviewed for length the same
  way code is. A bloated lens file is a regression.
- **Pipeline length.** Adding a stage is adding a fixed cost to every
  gremlin forever. We resist it.

The cost knobs we have *considered and are not using today*:

### 5.1 Why session-resumption caching is out for now

We did try this.

`CompletedRun.session_id` used to be captured from the `claude -p`
stream-json output so a later stage could call
`claude --resume <session_id>`. The idea was straightforward: if two
stages ran back-to-back, Anthropic's prompt cache might make the second
stage cheaper because the first stage had already paid to build context.

What we learned is not "session continuation is fundamentally wrong."
What we learned is that this implementation sat in an awkward spot in
this design.

1. **Too few stage edges benefited.** The strongest candidate edge was
   `review-code → address-code`, where address could plausibly reuse
   review's reads and findings. Most other edges either do not run
   back-to-back (`verify`, `github-wait-copilot`, `github-wait-ci`, CI gates) or do
   not preserve enough useful context to matter (`implement → review-code`
   rereads a tree that implement just changed).

2. **The best-looking edge in theory is weak in practice.** The
   `plan → implement` handoff often happens minutes or hours later, with
   the plan authored interactively and then handed to the gremlin via
   `--plan` or an issue. That is usually outside the cache window, so the
   main tempting edge often does not cash out.

3. **The plumbing cost was permanent.** Supporting continuation meant
   carrying `session_id` through the client protocol, stage context, and
   resume semantics while still keeping the cold-start path. That added
   ongoing complexity to infrastructure we want to stay boring.

4. **It weakened the stage boundary in exactly the way §3 tries to
   avoid.** A resumed session imports the prior stage's full message
   history. That is in-memory context sharing through a side door.
   Once a stage depends on inherited history, `--resume-from <stage>`
   becomes less clean, prompts stop being as bounded, and individual
   stages are less independently testable.

5. **It did not compose with the whole pipeline.** Sessions are linear.
   Some of our pipelines are not. Parallel `review-code` stages and any
   future fan-out stages still need a cold-start path, because one
   session cannot be resumed into multiple concurrent children.

So the current position is: we removed `CompletedRun.session_id` and are
not pursuing session-resumption caching in the current implementation.
That is a "not now" decision, not a permanent design taboo.

If we ever reopen it, the bar should be concrete:

1. Measured evidence that a specific stage edge is a real cost hot spot.
2. A continuation model that preserves the clean cold-start and
   `--resume-from` paths.
3. A clear answer for which edges may share history and which must stay
   isolated.
4. A story for parallel stages, or an explicit decision that the feature
   only applies to a narrow sequential subset.

Until then, the simpler rule wins: stages communicate through explicit
artifacts, not inherited session history.

### 5.2 What we'd do instead, if cost became a problem

Before reaching for session caching we would:

1. Profile per-stage cost on real gremlins and find the actual hot stage.
   It is almost always `implement`, occasionally `review-code` on large
   diffs.
2. Trim that stage's prompt or split it. Prompt size is the cheapest
   variable to move.
3. Drop non-critical stages to a smaller model. `github-address-pull-request-reviews` and the
   chain-step `handoff` agent are good candidates; they're constrained
   tasks that don't need Sonnet's headroom.
4. Only then consider structural changes to context flow.

The discipline is: cost work follows measurement, not intuition.

## 6. What this design is not good at

Worth stating, so future contributors don't try to bend the system into
shapes it resists:

- **Tight feedback loops between stages.** If you find yourself wanting
  `implement` to ask `plan` a clarifying question, the answer is to make
  the plan better, not to wire a back-channel.
- **Cross-gremlin learning.** Each gremlin is independent. If two
  gremlins are duplicating work, the fix is at the planning layer (one
  bigger plan, or a boss chain), not at the runtime layer.
- **Streaming partial output to operators.** Stages run to completion and
  produce artifacts. The log is tail-able, but no stage commits to
  emitting structured progress mid-flight.

These are non-goals on purpose. The system is designed to be boring,
resumable, and cheap to reason about, in roughly that order.
