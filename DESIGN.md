# Gremlins — System Design

This document describes how the gremlins workflow system is put together: where
we use deterministic code, where we delegate to a model, how context flows (and
is deliberately *not* shared) between stages, and the cost model that follows
from those choices.

It is not a reference for individual modules — see the per-package `AGENTS.md`
files for that. It is the rationale you need to evaluate proposed changes
without re-deriving the trade-offs each time.

### The workflow language is unopinionated

Gremlins is an agentic *workflow language*, not a workflow. The gremlin definition YAML
is the program; the harness is only its runtime. The runtime supplies
mechanics — stage sequencing, resumption, worktrees, artifact plumbing, bail
bookkeeping, client construction — and keeps injected opinion to a minimum.
A stage's model sees the prompts the definition author declared, the artifacts
the definition passed through, and a thin harness system prompt that carries
essential tool/directory framing. Any major behavioral opinion (what to
re-check, how to communicate, when to bail) still belongs in the definition's
own prompt files — not the harness's.

> **Exception — current state:** The harness system prompt currently includes
> some opinionated guidance (e.g. delegation instructions). This is not ideal
> for a workflow language, but pragmatic for now. In the future, model-specific
> guidance will need to live in the harness system prompt; per-definition
> behavioral norms should move back into definition prompt files as the boundary
> firms up.

## 1. The shape of a gremlin

A gremlin is a sequence of **stages** executed by a thin orchestrator. The
sequence is described in a YAML gremlin definition (`gremlins/definitions/local.yaml`,
`gh.yaml`, optionally a project-scoped override at `.gremlins/`).

A typical definition looks like this:

    plan → implement → review-code → address-code → verify → github-open-pull-request → ...

Each stage is one of two kinds:

- **Deterministic stages** are Rust code in the `stages/` module. They run shell
  commands, talk to `gh`, manage worktrees, parse JSON, wait on CI. They do not
  invoke a model. Examples: `exec`, `loop`, `parallel`, `sequence`.
- **Agentic stages** invoke the agent loop in `clients/agent_loop.rs` through a
  backend provider (`openai`, `xai`, `openrouter`, `cmd`). They receive a prompt
  assembled from definition-declared prompt files, run to completion, and produce
  an artifact on disk (a markdown file, a commit, a PR comment). Examples:
  `plan`, `implement`, `review-code`, `address-code`,
  `github-review-pull-request`, `github-address-pull-request-reviews`. Most
  agentic stages invoke the model exactly once; the two self-healing stages
  described in §2.2 are the exception.

The executor (the run loop in the `gremlins` crate) is responsible for
sequencing, `--resume-from <stage>` semantics, and SIGINT/SIGTERM reaping of
live agent subprocesses. It is *not* responsible for any decision the model
could plausibly make better.

## 2. Determinism and agency

Gremlins run unattended — sometimes overnight, sometimes in chains of a
dozen. The system has to be robust against model misbehavior, transient
infrastructure failures, and operator interruption. That shapes where we
allow agency and where we refuse to.

**Use deterministic code for:**

- The sequence itself. The definition YAML is the contract; stages do not get
  to decide what runs next. This is what makes resumption,
  rescue-after-bail, and chained-boss workflows tractable — the operator
  always knows what stage a gremlin is in, and what comes after, by reading
  one file.
- Anything observed by another process. Stage names, bail classes, and the
  marker-protocol bail reasons are byte-stable strings. They are written to
  `state.json` and read by the launcher, the fleet manager, the rescue
  protocol, and shell hooks. A model rewording any of these would silently
  break cross-process consumers, so they live in byte-stable convention
  strings and YAML rather than in prompts.
- Filesystem and git mechanics. Worktree creation, branch handling, commit
  authorship, PR opening — these are deterministic helpers. The model writes
  *content* (a commit message, a plan, a code change) but does not run `git`
  itself.
- Bookkeeping. `StateData::set_stage` and `StateData::record_stage_error` write atomically
  and never panic. A gremlin that crashes mid-stage must leave behind a state
  file the rescue protocol can interpret.

**Delegate to a model for:**

- Reading code and forming a plan from an issue or a free-text prompt.
- Making the code change.
- Reviewing a diff against a lens.
- Deciding which review findings are worth addressing and editing the code
  accordingly.
- Writing a commit message, a PR description, or a reply to a reviewer
  comment.
- Chain-step decisions: the handoff agent (invoked via the `handoff` recipe)
  decides whether a boss chain is done, and if not, what the next child
  should plan.

The dividing line is consistent: **the model produces content; deterministic
code moves it around.** A stage that needed a model to decide *whether to run*
would be a sign the definition was modeled wrong.

### 2.1 Why a YAML definition rather than an agent loop

We could build this as a single long-lived agent that reads tools, makes
decisions, and produces a PR — and we considered it. We don't, for three
reasons:

1. **Resumability.** A definition with named stages and a `state.json` cursor
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

The definition is the deterministic skeleton; agency is intentionally confined
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
Splitting the loop across definition stages would require either loop
semantics in the definition YAML (a much larger change) or unrolled stages
that decide whether to skip — which §2 explicitly forbids.

The cost of this exception:

- §6's "per-stage cost is roughly stable" does not hold for these two.
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

A stage can halt the definition by returning a bail error or by calling
`StateData::record_stage_error`, which writes a `class` (and optional
`detail`) to `bail_{attempt}.json`.

The two routes serve different jobs:

- **Agent bail** is detected when the agent loop's output contains a
  `BAIL: <class>: <detail>` marker. The agent stage in
  `stages/agent.rs` parses this marker, extracts the `bail_class` (one of
  `reviewer_requested_changes`, `security`, `secrets`, `other`), and
  propagates the error up to halt the definition.
- **`record_stage_error`** records a *structured*, *persistent* halt reason
  in `bail_{attempt}.json`. Both `class` and `detail` (a one-line human note)
  live in that file after the process exits. Exec stages can also trigger
  bail by writing to `artifact://bail`.

The persistence is the point. `bail_class` is read by the rescue
protocol (§5.3), the fleet manager, the boss recovery table, and shell
hooks — exactly the cross-process consumers §2 says we serve with
byte-stable strings rather than prose. A stage that only returns an error
tells a human; a stage that calls `record_stage_error` first also tells a
*script*.

`record_stage_error` does not itself halt the definition. It writes the marker
and returns; the caller returns an error immediately afterward. The
pairing — write the marker, then return the error — is the pattern. The
marker outlives the error.

A bail error is returned when a structured bail condition is detected (e.g.,
when calling `StateData::stage_error` in stages that follow
soft-failure points like `github-address-pull-request-reviews` and
`github-review-pull-request`, or in the self-healing stages (§2.2) after
each fixer agent runs). This allows a stage to halt cleanly when an agent
has already bailed via the `BAIL:` marker without the stage having to
inspect agent output.

The run loop does not explicitly catch bail errors; any unhandled error ends
the run and is logged. Bail errors are caught in specific contexts (e.g.,
parallel fan-in) to aggregate and handle multiple concurrent bails before
re-raising.

## 3. Context management

Context is the central design constraint. The cheapest, most reliable, and
most reproducible run is the one where each agent gets exactly the
information it needs and nothing else. We push hard on this.

### 3.1 Stages do not share an in-memory context

Every agentic stage starts a **fresh agent loop** with a **fresh model
context** via the backend trait. There is no shared scratchpad, no in-memory
history threaded between stages, no rolling summary. When `address-code` runs
after `review-code`, it does not inherit anything from the review process —
it gets the same starting context any cold invocation would, plus the
artifacts the review wrote to disk.

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

1. Prompt files declared in the definition YAML (`prompts/code_style.md`,
   `prompts/implement_local.md`, etc.). These are pinned per-definition.
2. The artifacts produced by upstream stages, read from disk and embedded
   into the prompt by the stage body.
3. The minimum task framing the stage needs to do its job.

A reviewer does not see the planner's prompt. The implementer does not see
the reviewer's lens. Each stage is given its own job in its own words, and
upstream output crosses the boundary as data, not as context.

Beyond the definition's own prompts and artifacts, the harness injects a thin
system prompt carrying tool definitions, directory layout, and — currently —
some opinionated guidance (e.g. delegation policy). This is a compromise: the
ideal is zero harness opinion, but pragmatic model steering is needed today.
Over time model-specific guidance will stay in the system prompt while
definition-appropriate norms move back into definition files.

### 3.3 The worktree is the workspace

Every gremlin runs in its own git worktree. The agent inside a stage uses
its own tools (Read, Edit, Bash) to navigate that worktree; the orchestrator
does not pre-load files into the prompt. This keeps the prompt small even
for large codebases — the agent only loads what it actually needs — and it
means the same prompt scales from a 100-file repo to a 10,000-file repo
without modification.

The cost of this is a bit of redundant exploration: `implement` re-reads
files that `plan` already read. We accept that cost (see §6) because the
alternative — pre-loading the union of files plan touched — would couple
the stages together, defeat resumption, and bloat the prompt.

### 3.4 Across-gremlin context isolation

Different gremlins share nothing. Different `gremlin_id`s have different
worktrees, different `state.json` files, different log directories. A boss
chain coordinates child gremlins by reading their `state.json` files, not
by sharing context with them. This is what lets a boss recover from a child
bail without inheriting any of the child's confusion.

### 3.5 Parallel stages

A `type: parallel` block in a definition YAML runs N children concurrently.
At runtime the block materialises as **three stages**, keeping §2's
deterministic-vs-agentic line intact:

- **`<group>-fanout`** (deterministic). Creates per-child artifact
  subdirs and per-child git worktrees, each a detached checkout of the
  current branch tip. Runs `git worktree prune` first to clear leftovers
  from any previous interrupted run.
- **`<group>`** (agentic, N concurrent). Runs N agent invocations
  concurrently via the async runtime, each in its own forked gremlin with
  its own `state.json` and worktree. Children cannot see each other's
  state; each child's outcome (success or error) is collected by fan-in.
- **`<group>-fanin`** (deterministic). Collects child outcomes from the
  `JoinSet`, applies the block's `error_policy`, returns a bail error if
  warranted, merges artifacts from successful children, and tears down all
  per-child worktrees (best-effort). Cleanup runs after all children are
  collected; early-exit paths (cancellation, fork failure) may leave
  worktrees behind.

Each child runs in its own forked gremlin with its own `state.json`, so
bail state is naturally isolated per child. The parent collects outcomes
via the `JoinSet` and applies the error policy in memory — there is no
shared `parallel_bails` slot and no cross-child bail contamination.

State patching holds an exclusive file lock on a per-`state.json` lock
file for the duration of each read-modify-write. This prevents concurrent
state writes from racing within a single child's state file.

**Per-block knobs** (declared on the parallel block in the definition YAML):

- `cancel_on_error: false` (default). All children run to completion even if
  one bails. Right for review lenses where each lens is independent.
  Set to `true` for parallel implementers where a structural bail by one
  child makes the others irrelevant — on first bail a cancel flag is set
  and children that have not yet started are skipped.
- `error_policy: any` (default). Any bailing child causes the group to bail
  after fan-in. Set to `all` to require every child to bail before the
  group bails.

**Worktrees are always-on.** Every parallel child gets its own worktree,
regardless of whether it mutates. The cost — one full working-tree checkout
per child, with object storage shared via `.git/worktrees/` — is small
relative to gremlin runtime. Unconditional worktrees remove a flag and a
code path: read-only and mutating parallel are architecturally identical;
the only difference is what the children write and what fan-in does with it.

**The merge problem is unsolved.** Fan-in merges only registered artifacts
from children; worktree mutations outside bound artifacts are discarded.
Deciding what to do when N agents each produced a different diff — pick the
best, merge all, cherry-pick — requires a concrete use case before the
right shape is clear.
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

## 4. The artifact registry

Artifacts are the only communication channel between stages (§3.1). Every
artifact lives in an **artifact registry** — a key-value store that maps
artifact URIs (`artifact://plan.md`) to filesystem paths or inline content.

### 4.1 One trait, two roles

The `StateStore` trait is the unified storage backend. It provides both
state.json operations (read, patch, lock) and artifact operations (content
lookup, URI resolution, registration, path resolution, file-existence
checks, and file copy) through a single trait. There is no separate
"localized" companion trait — the filesystem-scoped operations are part of
`StateStore` itself.

The store has two consumers with different needs:

- **Agent stages** must be constrained. An agent that can see every artifact
  in the registry can read files it was never meant to see. Section 3 says
  each agent gets exactly the information it needs and nothing else. To
  enforce this, agent stages receive a **checkout** — a scoped, temporary
  `StateStore` containing only the stage's declared bind and interpolation keys.
- **Exec stages** also use a checkout. They run shell commands and need
  filesystem access to artifacts. The checkout gives them a scoped directory
  containing only their declared inputs.
- **Everything else** — bootstrap, `bind_artifact`, the `cli_out` binding
  step — populates the main store directly. These are deterministic
  mechanical operations, not agentic stages. There is no context to
  constrain.

### 4.2 Checkout as the canonical entry point for stages

`StateStore::checkout_registry(&keys)` creates a scoped `StateStore` in a
temporary directory. For each key:

- If the key is already registered, the artifact file (or content) is copied
  into the checkout.
- If the key is not yet registered, an empty placeholder path is created so
  the stage can write the file. The stage's `commit_agent` / `commit_exec`
  then registers it.

After the stage completes, `merge_registry` copies the checkout's new
artifacts back into the main store. The checkout directory is discarded.

Agent and exec stages **must** use checkout to move objects into the
registry. The checkout is what guarantees that:

1. The stage cannot discover artifacts outside its declared inputs.
2. The stage cannot read or write files in the main artifact directory.
3. The `artifact_dir` path injected into the agent's system prompt points
   into the checkout, not the main store.

### 4.3 Why checkout exists

The `FileSystemStateStore` is the single filesystem-backed implementation of
`StateStore`. The alternative — forcing agent and exec stages to never touch
the filesystem and instead call store APIs for every read and write — is too
constraining. Agents already have filesystem tools (Read, Write, Edit, Bash);
exec stages run arbitrary shell commands. Both need real files to work with.

Checkout gives them real files, scoped to exactly what they should see. The
main store remains the authoritative store; checkouts are transient
workspaces.

## 5. Boss gremlins and chained workflows

A single gremlin produces one PR from one plan. Many real tasks don't fit
that shape — they are sequences of related changes that have to land in
order, where each step's plan depends on what the previous step actually
did. The **boss gremlin** is the pattern for those.

A boss is itself a long-running process, but it is not a stage definition
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
plan and runs the definition.

Boss resumption is keyed off `boss_state.json`, not the definition stage
vocabulary. The shared launcher resume path still tracks `state.json.stage`
for fleet status, but it does not pass `--resume-from` when re-spawning a
boss. If a caller does provide `--resume-from`, the boss executor logs that
the flag is being ignored and resumes from the chain cursor in
`boss_state.json`.

### 5.1 Where the agency lives

The boss reuses the §2 dividing line, applied at a different scale:

- **Deterministic:** spawning children, polling for completion, landing
  PRs, writing `boss_state.json`, parsing children's `state.json`,
  deciding when the chain has structurally stalled.
- **Agentic, exactly once per step:** the **handoff agent** (invoked via
  the `handoff` recipe, bundled as
  `crates/gremlins/src/assets/data/stages/handoff.yaml`). It reads
  the rolling plan, the chain spec, and the diff accumulated on the branch,
  and produces one of three decisions: `next-plan` (here is the plan for
  child N+1), `chain-done` (we are finished), or `bail` (something is
  structurally wrong, stop and ask the operator).

Everything else in the boss loop is deterministic Rust code. Notably, the
boss does *not* use a model to decide whether a child succeeded — it reads
the child's `state.json`. This is the same byte-stable-strings discipline
from §2 applied across the parent/child boundary.

### 5.2 Context isolation across the chain

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

### 5.3 The child-bail recovery protocol

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

### 5.4 Why a boss isn't just a longer definition

We could express boss workflows as a single longer YAML definition with
many `plan → implement → review-code → ...` repetitions. We don't,
because:

- The number of steps isn't known up front. A real chain ends when the
  feature is done, not at a step count we picked yesterday.
- Each step's plan is a function of the previous step's diff. That's an
  agentic decision (the handoff agent), and putting it inside a stage
  definition would mean a stage that decides whether the next stage runs
  — which §2 forbids.
- Children need to be independently rescuable, landable, and abandonable
  by an operator. That works because each child is a separately
  launched gremlin with its own state file. A flattened definition would
  collapse them into one process and lose the granularity.

The boss is the right abstraction precisely because it stays out of the
child's definition and confines its own agency to one decision per step.

### 5.5 PR stacking in looped definitions

When a definition contains a `loop` stage and that loop body includes a
`github-open-pull-request` stage, every PR after the first is automatically
based on the previous PR's branch. No per-definition configuration is
required.

**The mechanism.** The `handoff` recipe reads the previous PR's branch from
`artifact://pr-branch.txt` and checks out that branch before running the
next iteration's work. The definition's `base_ref` sets the initial base.
Because all loop iterations share one `gremlin_id` and one artifact
registry, every iteration after the first finds the previous iteration's
PR branch recorded in the registry.

**The invariant.** The PR-opening stage returns an error if the implementation
branch is empty before appending a PR artifact, so every PR artifact in the
registry has a non-empty `branch` field.

**What this means in practice.** A boss definition (or any looped gh definition)
produces a stack of PRs by default. PR #1 targets `main` (or the
definition's `base_ref`). PR #2 targets PR #1's branch. PR #3 targets
PR #2's branch, and so on. The artifact registry is the authoritative record.

**The escape hatch.** Stacking is a consequence of loop iterations sharing
one `gremlin_id`. To produce side-by-side PRs based on a fixed ref, launch each
iteration as an independent gremlin rather than as loop iterations in a
single run — each gets its own `gremlin_id` and an empty artifact registry,
so the PR targets `main` (or whatever `base_ref` is in that gremlin's
definition).

Within a looped definition you can also set `base_ref` under the
`github-open-pull-request` stage's `options:` to control the first-iteration base and
the fallback when the artifact registry is empty, but this does not suppress
stacking once prior PR artifacts exist.

## 6. Cost model

Per-gremlin cost is dominated by two things:

- **Token volume per stage.** Driven by prompt size + how much the agent
  reads from the worktree. Bounded by §3 — small prompts, scoped agents.
- **Number of stages × per-stage volume.** Bounded by the definition YAML.

We measure cost per run via `cost_usd` reported by the backend (from
subprocess output for `cmd:` backends, or computed by API backends) and
token usage accumulated in `state.json`. That number is the unit we
optimize against.

The cost knobs we *do* use:

- **Model selection per stage.** The definition's `clients` block lets a
  stage pick a smaller model. We default everything to Sonnet and would
  drop individual stages to Haiku only with a measured reason.
- **Prompt size discipline.** Prompt files are reviewed for length the same
  way code is. A bloated lens file is a regression.
- **Definition length.** Adding a stage is adding a fixed cost to every
  gremlin forever. We resist it.

The cost knobs we have *considered and are not using today*:

### 6.1 Why session-resumption caching is out for now

We did try this.

Session IDs used to be propagated across stages so a later stage could
resume the session. (Session IDs are still captured and used within a
single stage for timeout/error retry via `--resume`, but they no longer
cross stage boundaries.) The idea was straightforward: if two stages ran
back-to-back, the provider's prompt cache might make the second stage
cheaper because the first stage had already paid to build context.

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
   carrying session IDs through the backend trait, stage context, and
   resume semantics while still keeping the cold-start path. That added
   ongoing complexity to infrastructure we want to stay boring.

4. **It weakened the stage boundary in exactly the way §3 tries to
   avoid.** A resumed session imports the prior stage's full message
   history. That is in-memory context sharing through a side door.
   Once a stage depends on inherited history, `--resume-from <stage>`
   becomes less clean, prompts stop being as bounded, and individual
   stages are less independently testable.

5. **It did not compose with the whole definition.** Sessions are linear.
   Some of our definitions are not. Parallel `review-code` stages and any
   future fan-out stages still need a cold-start path, because one
   session cannot be resumed into multiple concurrent children.

So the current position is: we removed cross-stage session ID propagation
and are not pursuing session-resumption caching in the current implementation. That is
a "not now" decision, not a permanent design taboo.

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

### 6.2 What we'd do instead, if cost became a problem

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

## 7. What this design is not good at

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

## 8. TUI

The TUI is a single fullscreen viewport (main screen, no alternate screen)
composed from **nested widgets**. Each widget owns its render state, reports
min/desired/max heights, and paints itself into the region assigned by a
shared recursive layout engine. `LiveArea` owns a typed `Scene` and a
diff-driven double-buffer engine; it no longer stores widgets in a
long-lived boxed-widget vec and never reifies concrete types.

### 8.1 The widget trait

Every region is a widget behind [`LiveAreaWidget`]. Width travels down as
arguments, height travels up:

- `min_height(width, ctx)` — the floor height.
- `desired_height(width, ctx)` — preferred height; defaults to `min_height`.
- `max_height(width, ctx)` — allocation cap (default `u16::MAX`).
- `flexible(ctx)` — whether the widget may absorb leftover rows.
- `finalize(width)` — mutate the buffer into its final frame state before
  measurement.
- `take_overflow(available_rows, width, ctx)` — drain head lines for
  terminal scrollback; the transcript FIFO is the only non-empty producer.
- `render(area, buf, ctx)` — paint only.

Widgets receive a shared read-only `RenderContext` (input text and cursor,
gremlin count, project name, followed log id, active-run table, completion
preview). `RenderContext` is width-free; parents pass their already-reduced
inner width to children in every width-taking method.

### 8.2 The widget scene

`LiveArea` owns a concrete `Scene` with typed fields:

- `transcript: TranscriptWidget`
- `input_bar: InputBarWidget`
- `info_bar: InfoBarWidget`
- `screen_view: Option<ScreenView>` — a typed enum of full-screen bodies:
  `Runs`, `Log`, `Debug`.

On each frame `compose` takes the scene out temporarily, builds an ordered
`Vec<&mut dyn LiveAreaWidget>` by borrowing the concrete fields (chat =
transcript, input, info; otherwise the single active `ScreenView` body), runs
the shared recursive layout engine, paints, and puts the scene back. The
renderer only calls trait methods.

The input and info bars are the last two chat widgets and are excluded from
the terminal scroll region. Everything above them is the content region.

**Nested transcript.** `TranscriptWidget` is a composite: one styled FIFO
(the only overflow producer) plus two embedded `StreamingWidget` children
(reasoning and live tool output). Its own min/desired/max heights sum the
FIFO and its children; after the parent allocates `H` rows, the transcript
re-runs the same allocate pass on its child slice and paints top-to-bottom
pre-reasoning FIFO head, reasoning, FIFO tool-call events, live tool output,
then streaming response (prompt → reasoning → tool-call events → live tool
output → response).

**Streaming widgets.** `StreamingWidget` owns a `LineBuffer` of completed
styled lines plus a trailing partial, an optional title row supplied as a
function of `RenderContext`, a scroll anchor (`scrolled`/`last_width`), a
per-line styler, and a row cap. It tail-anchors, trims its own head, collapses
to zero when idle, and never returns overflow. The reasoning instance uses a
four-space indent plus nickel italic text; the tool instance uses a `◆`
chrome title and four-space-indented, nickel italic, ANSI-stripped text. Both
constructors retain a sliding commit tail capped at the shared stream row
count so turn-end commits never depend on how short the live band was painted.

### 8.3 Layout and rendering model

Each frame runs the same recursive engine over an ordered slice of `&mut dyn
LiveAreaWidget`:

1. **Finalize** every widget at the parent-supplied width.
2. **Measure** min/desired/max from finalized content.
3. **Allocate** with priority-aware shrink/expand, honoring each max.
4. **Drain overflow** and order it by `overflow_priority`.
5. **Paint** each widget into its allocated region; chat commits the
   transcript's overflow to terminal scrollback first.

A container's `size_priority` and `overflow_priority` are the minimum of its
children. After allocation, the container re-runs the allocate pass on its
children constrained to `H` rows, feeding them the already-reduced inner
width. Only the transcript FIFO may overflow to terminal scrollback;
`StreamingWidget` returns no overflow and self-trims. Full-screen views never
commit scrollback.

### 8.4 Visual states

Each diagram shows the terminal's main screen — the `╔═══╗` box is the
**viewport** (what's visible on screen). Content above the `─── viewport top ───`
marker has scrolled off into the terminal's scrollback buffer (accessible via
tmux copy-mode, terminal scrollback, etc.). The input and info bars are always
pinned at the viewport bottom.

#### 8.4.1 Idle — several finished turns

```
                        ~~~ terminal scrollback ~~~
                        (older turns visible in
                        tmux copy-mode, etc.)
  > optimize query performance
    4 tools · 3.2K tokens · 64% cache
  > refactor middleware chain
    2 tools · 550 tokens · 91% cache
                     ─── viewport top ───
╔══════════════════════════════════════════════════════════════╗
║  > plan implement review                                   ║
║    3 tools · 1.2K tokens · 85% cache                       ║
║                                                              ║
║  > fix the login bug                                       ║
║    1 tool · 850 tokens · 92% cache                         ║
║                                                              ║
║  > add rate limiting                                       ║
║    5 tools · 2.1K tokens · 78% cache                       ║
║                                                              ║
║                                                              ║
║                                                              ║
║  > ▌                                                         ║
║  gremlins tui · 0 gremlins · project: gremlins              ║
╚══════════════════════════════════════════════════════════════╝
                     ─── viewport bottom ───
```

Each finished turn renders its full layout (prompt, reasoning tail, tool
list, response) plus the stats footer. Two blank lines separate adjacent
widgets. Older turns scroll off the top into terminal scrollback. The input
bar waits for the next message.

#### 8.4.2 Active turn — streaming

```
                        ~~~ terminal scrollback ~~~
                        (older turns visible in
                        tmux copy-mode, etc.)
  > optimize query performance
    4 tools · 3.2K tokens · 64% cache
                     ─── viewport top ───
╔══════════════════════════════════════════════════════════════╗
║  > plan implement review                                   ║
║    3 tools · 1.2K tokens · 85% cache                       ║
║                                                              ║
║  > fix the login bug                                       ║
║    1 tool · 850 tokens · 92% cache                         ║
║                                                              ║
║  > add rate limiting                                       ║
║  ┌────────────────────────────────────────────────────────┐ ║
║  │  reading auth.rs...                                    │ ║
║  │  running cargo test...                                 │ ║
║  │────────────────────────────────────────────────────────│ ║
║  │ Here's the rate limiting implementation:               │ ║
║  │                                                        │ ║
║  │ We'll add a token bucket in the middleware layer...    │ ║
║  └────────────────────────────────────────────────────────┘ ║
║  > ▌                                                         ║
║  gremlins tui · 0 gremlins · project: gremlins              ║
╚══════════════════════════════════════════════════════════════╝
                     ─── viewport bottom ───
```

The active turn sits at the bottom of the transcript area, just above the
input bar. It renders the prompt, tool-call lines between the prompt and the
response, a capped reasoning tail, and the accumulating response. The
widget grows as content arrives; older finished turns scroll off the top of
the viewport into terminal scrollback.

#### 8.4.3 Finished turn with stats footer

```
                        ~~~ terminal scrollback ~~~
                        (older turns visible in
                        tmux copy-mode, etc.)
  > optimize query performance
    4 tools · 3.2K tokens · 64% cache
  > update dependencies
    1 tool · 320 tokens · 95% cache
                     ─── viewport top ───
╔══════════════════════════════════════════════════════════════╗
║  > plan implement review                                   ║
║    3 tools · 1.2K tokens · 85% cache                       ║
║                                                              ║
║  > fix the login bug                                       ║
║    1 tool · 850 tokens · 92% cache                         ║
║                                                              ║
║  > add rate limiting                                       ║
║    5 tools · 2.1K tokens · 78% cache                       ║
║  ┌────────────────────────────────────────────────────────┐ ║
║  │  reading auth.rs...                                    │ ║
║  │  running cargo test...                                 │ ║
║  │────────────────────────────────────────────────────────│ ║
║  │ Here's the rate limiting implementation:               │ ║
║  │                                                        │ ║
║  │ We'll add a token bucket in the middleware layer.      │ ║
║  │ The implementation lives in src/middleware/rate.rs     │ ║
║  │                                                        │ ║
║  └────────────────────────────────────────────────────────┘ ║
║                                                              ║
║  > ▌                                                         ║
║  gremlins tui · 0 gremlins · project: gremlins              ║
╚══════════════════════════════════════════════════════════════╝
                     ─── viewport bottom ───
```

When the turn finishes, the retained reasoning tail is committed to the
transcript FIFO between the prompt and the tool blocks (prompt → reasoning →
tools → response), so it appears in the finished history like committed tool
output and may overflow into terminal scrollback. While the turn is still
streaming, the in-progress reasoning strand stays display-only and never
enters the FIFO or scrollback; cancel (Esc), clear, and new-chat paths discard
it without committing it.

#### 8.4.4 Gremlins watch overlay

```
                        ~~~ terminal scrollback ~~~
                        (older turns visible in
                        tmux copy-mode, etc.)
  > fix the login bug
    1 tool · 850 tokens · 92% cache
  > refactor middleware chain
    2 tools · 550 tokens · 91% cache
                     ─── viewport top ───
╔══════════════════════════════════════════════════════════════╗
║  ┌─ gremlins watch ───────────────────────────────────────┐ ║
║  │ ID          STATUS   STAGE        PROJECT               │ ║
║  │ abc123      running  implement    gremlins              │ ║
║  │ def456      running  review-code  gremlins              │ ║
║  │ ghi789      done     —            other-project         │ ║
║  │                                                        │ ║
║  │ 2 running · 1 done                                     │ ║
║  └────────────────────────────────────────────────────────┘ ║
║                                                              ║
║  > plan implement review                                   ║
║    3 tools · 1.2K tokens · 85% cache                       ║
║                                                              ║
║  > fix the login bug                                       ║
║    1 tool · 850 tokens · 92% cache                         ║
║                                                              ║
║  > ▌                                                         ║
║  gremlins tui · 3 gremlins · project: gremlins              ║
╚══════════════════════════════════════════════════════════════╝
                     ─── viewport bottom ───
```

The watch overlay occupies the upper portion. Transcript widgets flow below it;
older ones scroll into terminal scrollback. The input bar still accepts chat —
the overlay is non-blocking.

### 8.5 Widget lifecycle

```
                  user submits message
                         │
                         ▼
              ┌─────────────────────┐
              │ transcript widget   │  prompt + tool lines + response;
              │ (streaming)         │  grows with content
              └─────────┬───────────┘
              ┌─────────────────────┐
              │ reasoning widget    │  live tail (dim italic); 5-row cap
              │ (streaming)         │
              └─────────┬───────────┘
                        │ Done / Ended
                        ▼
              ┌─────────────────────┐
              │ transcript widget   │  same layout; tool lines, prompt and
              │ (finished)          │  response committed as history; reasoning
              │                     │  frozen in its own widget
              └─────────────────────┘
                        │
           session ends / cleared
                        │
                        ▼
                    discarded

              ┌─────────────────────┐
              │ reasoning widget    │  Error / Esc
              │ (streaming)         │
              └─────────┬───────────┘
                        │
              Esc / Error during an active turn
                        │
                        ▼
              ┌─────────────────────┐
              │ transcript widget   │  `clear_current_turn` truncates the FIFO
              │ (discarded)         │  back to the turn start: prompt, tool
              │                     │  lines, and in-flight response all dropped
              └─────────────────────┘

  The reasoning widget is ephemeral while the turn streams: it populates during
  the turn and is display-only, never entering the transcript FIFO or terminal
  scrollback until `Done`/`Ended`. At turn end its retained tail is committed
  into the FIFO between the prompt and the tool-call lines, so the finished
  reasoning block survives across turns as ordinary scrollback history.
  Tool-call lines are folded directly into the transcript FIFO between the
  prompt and the response as they complete, so they — like the prompt and
  response — survive across turns as ordinary scrollback history. Esc/Error
  discards the turn and its reasoning without committing it.

  system content (banner, events, command output) is pushed directly into the
  transcript widget. Overlay widgets exist outside this lifecycle — they are
  pushed/popped from the top of the widget stack, have no persistent state
  when hidden, and are dismissed by `Esc`.
```

### 8.6 Input routing

The input bar is the primary interaction surface. Key routing rules:

| State | Input bar behavior |
|-------|-------------------|
| Idle (no active turn, no overlay) | Accepts chat text and slash commands |
| Active turn (streaming) | Disabled — input is not accepted |
| Gremlins watch overlay visible | Accepts chat text and slash commands as normal |
| Single gremlin watch visible | Accepts slash commands only |
| Debug widget visible | Input bar disabled; debug prompt inside the overlay accepts debug commands |

`Esc` in order of priority:

1. If a blocking overlay is visible (single watch, debug), dismiss the overlay.
2. If a non-blocking overlay is visible (gremlins watch), dismiss the overlay.
3. If an active turn is streaming, abort the turn (transition widget to finished, discard partial response).
4. Otherwise, clear the input bar.

### 8.7 Summary

The TUI is a stack of self-contained widgets laid out top-to-bottom. The
transcript is the one elastic, overflow-producing widget; reasoning and tools
are bounded, transient widgets that appear only during a streaming turn; the
input and info bars are fixed and always pinned. Overlays are widgets pushed
onto the top of the stack that occlude the content beneath them. The user
never scrolls within the TUI — tmux and terminal features handle that. Widgets
describe their own content and lifecycle; `LiveArea` just lays them out,
commits overflow to scrollback, and diffs the result to the terminal.

## 9. Chat backend

A chat is a gremlin — a persistent one with an unbounded chain of Agent stages.
Each user message appends a new stage to the chain; each stage runs a fresh
agent loop (§3.1) to completion. The daemon owns all context management.
Front-ends are thin clients that send text into a gremlin and stream events
back out.

### 9.1 A chat is a gremlin owned by a session

A chat gremlin lives in the run map alongside pipeline gremlins, appears in
`gremlins ls`, and supports every daemon op a normal gremlin does — `stop`,
`debug`, `info`, `log`. The gremlin id is an internal daemon detail, however:
front-ends never address it directly. They address the **session** that owns
the gremlin, and the session dies (reaping the gremlin) when the front-end
disconnects or closes the session.

A chat gremlin's definition is built programmatically at creation time and
grows with every user message — each turn appends a new Agent stage to the
chain. The definition is never written to `.gremlins/`; it lives in the
ephemeral gremlin's state directory as `definition.yaml`.

### 9.2 Daemon owns context; front-ends are thin

The TUI (or any front-end) first creates a session:

```json
{"op": "create_session"}
```

The daemon replies with a session id and a session socket name. Conversation
turns are then sent on that session socket:

```json
{"op": "chat", "session_id": "sess-…", "text": "<user message>"}
```

No history, no transcript, no tool records, and no gremlin id — the
front-end holds no conversation state beyond the session id. The daemon
manages the entire context lifecycle internally. When a session dies, its chat
gremlin dies with it.

This makes the chat backend front-end-agnostic: the TUI, a CLI one-shot
(`gremlins chat <id> "message"`), a web dashboard, or an API all speak the
same protocol: create a session, then address it by session id.

### 9.3 Per-turn Agent stages with full context

Each turn is a complete Agent stage — a fresh agent loop with a fresh model
context, per §3.1. Context flows from one stage to the next through the
artifact registry, in the form of the full message array from the previous
stage.

After stage N completes:

1. The daemon serializes the agent loop's accumulated message array — system
   prompt, user messages, assistant responses, tool calls, and tool results —
   as a structured artifact (`artifact://conversation.json`).
2. Stage N+1 prepares its agent loop with this message array pre-populated.
   The new user prompt is appended as the next user message, and the loop
   proceeds from there.
3. The model sees the previous conversation with proper role attribution:
   its own assistant messages are in the assistant role, tool calls and
   results are in the tool/tool_result roles, not flattened into a system
   prompt transcript.

```
Stage N agent loop:                    Stage N+1 agent loop:
  [system]                                [system]
  [user: "fix the login bug"]             [user: "fix the login bug"]    ─┐
  [assistant: "I'll read auth.rs"]        [assistant: ...]               │
  [tool: Read auth.rs]                    [tool: Read auth.rs]           │ pre-populated
  [tool_result: (120 lines)]              [tool_result: ...]             │ from artifact
  [assistant: "The bug is in..."]         [assistant: ...]               │
  [tool: Edit auth.rs]                    [tool: Edit auth.rs]           │
  [tool_result: "ok"]                     [tool_result: "ok"]           ─┘
  [assistant: "Done."]                    [user: "add rate limiting"]    ← new
                                          [assistant: ...]               ← new turn
```

This is not API-level session resumption — there are no provider-specific
session IDs, no shared HTTP connections. It is content-level: the messages
from stage N are replayed as prefix messages in stage N+1's ChatCompletion
request. Every provider supports it, and prompt caching benefits naturally
from the stable prefix (system prompt + history).

### 9.4 Pause between stages

The gremlin pauses after each Agent stage completes, waiting for the next
user message. This is the same mechanism the interactive debug protocol uses:
the run loop finishes a stage, reaches the end of the (current) definition,
and blocks on an external signal. When a new message arrives, the daemon
appends a stage to the definition and resumes the run loop.

From the gremlin's perspective it is one continuous run — one `state.json`,
one stage cursor, one artifact registry, one worktree. It simply has an
unbounded definition that grows as the conversation progresses.

### 9.5 History rollback

Because each turn is a discrete stage, rolling back is truncating the stage
chain at any point. The daemon drops stages after the truncation point,
rewinds the artifact registry to the snapshot at that point, and sets the
stage cursor. The next user message resumes from there. No message editing,
no partial rewrites — the boundary is the stage boundary.

`/clear` (or `/new`) closes the session and opens a fresh one. The old
session's chat gremlin is torn down immediately — cancelled, reaped, and
removed from the run map — rather than remaining inspectable.

### 9.6 Relationship to the TUI

The TUI (§8) is one front-end for the chat backend. It creates a session,
sends `{"op": "chat", "session_id": "sess-…", "text": "..."}` on that
session socket, and renders stream events (reasoning chunks, tool results,
response text, turn completion) as transcript widgets. The TUI holds no
conversation history — the daemon is the authority.
