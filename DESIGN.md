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

    plan → implement → review-code → address-code → verify → commit-pr → ...

Each stage is one of two kinds:

- **Deterministic stages** are plain Python. They run shell commands, talk to
  `gh`, manage worktrees, parse JSON, wait on CI. They do not invoke a model.
  Examples: `verify`, `request-copilot`, `wait-copilot`, `wait-ci`,
  `commit-pr` (the mechanics; the message-writing is delegated).
- **Agentic stages** invoke `claude -p` via an injected `ClaudeClient`. They
  receive a prompt assembled from pipeline-declared prompt files, run to
  completion, and produce an artifact on disk (a markdown file, a commit, a
  PR comment). Examples: `plan`, `implement`, `review-code`, `address-code`,
  `ghreview`, `ghaddress`. Most agentic stages invoke the model exactly
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
  `review-code-detail-*.md`, the commit itself, the PR description.
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

Different gremlins share nothing. Different `gr_id`s have different
worktrees, different `state.json` files, different log directories. A boss
chain coordinates child gremlins by reading their `state.json` files, not
by sharing context with them. This is what lets a boss recover from a child
bail without inheriting any of the child's confusion.

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

### 4.1 Where the agency lives

The boss reuses the §2 dividing line, applied at a different scale:

- **Deterministic:** spawning children, polling for completion, landing
  PRs, writing `boss_state.json`, parsing children's `state.json`,
  deciding when the chain has structurally stalled.
- **Agentic, exactly once per step:** the **handoff agent**
  (`gremlins/handoff.py`). It reads the rolling plan, the chain spec, and
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

The cost knobs we have *considered and rejected*:

### 5.1 Why we reject session-resumption caching

`CompletedRun.session_id` is captured from the `claude -p` stream-json
output. At one point we thought about threading it back through
`claude --resume <session_id>` on the next stage, on the theory that
resuming a session would let Anthropic's prompt cache cover the prior
context for free.

We are not going to do this, for reasons that follow directly from §3:

1. **Stages don't share enough context to amortize.** The strongest
   candidate edge is `review-code → address-code`, where address would
   inherit review's file reads and findings. The rest of the pipeline
   either doesn't run back-to-back (verify, wait-copilot, wait-ci, CI
   gates) or doesn't share enough with the previous stage to matter
   (implement → review-code mutates the very files the previous stage
   read).

2. **The plan→implement edge is out-of-band in our workflow.** Plans are
   typically authored interactively in a separate session and dropped onto
   a gremlin via `--plan` or via an issue. The two stages are separated by
   minutes to hours, not seconds, so the prompt cache is cold by the time
   `implement` runs and the win evaporates.

3. **One edge isn't worth the wiring.** Threading `session_id` through
   `ClaudeClient.run`, the stage context, and resume semantics costs us
   permanent complexity in a layer we want to keep simple. The win on a
   single edge is small and only realized when the two stages run within
   the cache TTL — which is exactly the case where exploration cost is
   already small because the agent hasn't built up much state.

4. **It would couple stages we deliberately decoupled.** Resuming a
   session means the next stage inherits the prior stage's full
   message history. That is precisely the in-memory-context-sharing we
   ruled out in §3.1, smuggled in through a different door. Stages that
   communicate through resumed sessions are no longer independently
   testable, no longer cleanly resumable, and no longer reasoning about
   bounded prompts.

5. **Sessions are linear; some of our stages aren't.** Where we do run
   work in parallel (e.g. multiple review lenses, in pipelines that
   support it), a single `session_id` cannot be forked into N concurrent
   resumes. Any session-caching scheme would apply to a strict subset of
   stage edges and would have to coexist with the cold-start path
   anyway.

`CompletedRun.session_id` has since been removed. It was never load-bearing
and the overhead of carrying it through the protocol outweighed its
occasional debugging utility.

### 5.2 What we'd do instead, if cost became a problem

Before reaching for session caching we would:

1. Profile per-stage cost on real gremlins and find the actual hot stage.
   It is almost always `implement`, occasionally `review-code` on large
   diffs.
2. Trim that stage's prompt or split it. Prompt size is the cheapest
   variable to move.
3. Drop non-critical stages to a smaller model. `commit-pr`,
   `ghaddress`, and the chain-step `handoff` agent are good candidates;
   they're constrained tasks that don't need Sonnet's headroom.
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
