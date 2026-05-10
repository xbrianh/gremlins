# `gremlins/stages/`

Per-stage bodies — the work each pipeline stage actually does. Modules
here are called by orchestrators in `../orchestrators/`; they own no
sequencing logic of their own.

## Modules

- `registry.py` — `STAGE_REGISTRY` and `CLIENT_FACTORIES` dicts + `register_stage` / `register_client_factory`. All stage type lookups go through here.
- `compound.py` — `CompoundStage(Stage)` base class for stages that own a list of child stage entries (`body: list[StageEntry]`). Shared by `ParallelStage` and `LoopStage`.
- `loop.py` — `LoopStage(CompoundStage)`. Iterates pre-built body runner callables until HEAD is stable or `max_iterations` is exhausted. Body runners are called in order; `RunCmdFailed` is caught and noted (subsequent runners still execute so a fix agent can act), except on the final iteration where fix runners are skipped and the stage bails. `LoopStage.from_runners([...], max_iterations=N)` constructs from closures without needing a YAML body. Also exports `RunCmdFailed` (sentinel raised by command-check runners) and `LoopExhausted` (raised on exhaustion so callers can translate the message).
- `run_cmd.py` — `RunCmd(Stage)`. Runs `options["cmds"]` joined with `&&`; writes output to `run-cmd.log` and raises `RunCmdFailed` on non-zero exit. Registers as stage type `"run-cmd"`.
- `claude_prompt.py` — `ClaudePrompt(Stage)`. Loads `prompt_paths` and runs the agent; calls `check_bail` afterwards. Generic: no stage-specific template filling. Registers as stage type `"claude-prompt"`.
- `parallel.py` — `ParallelStage(CompoundStage)`. Constructed by the orchestrator with pre-built child runners; call `build_runtime_stages()` to get the three `(name, fn)` pairs (`<group>-fanout`, `<group>`, `<group>-fanin`) that implement fan-out/fan-in execution.
- `all.py` — importing triggers side-effect registration of all stage modules. `pipeline.py` imports it automatically via `_ensure_registered()`; no manual import needed.
- `handoff.py` — `Handoff(Stage)` plus the full handoff agent implementation (`run`, `build_prompt`, `collect_git_context`, `sanitize_rolling_plan`, etc.). `Handoff` runs the agent once per boss-loop iteration: returns normally on "chain-done" (loop exits via HEAD-stable), raises `RunCmdFailed` on "next-plan" (writes child plan to `plan.md`, loop continues), raises `RuntimeError` on "bail". Preserves the original boss spec in `boss-spec.md` and restores it to `plan.md` on "chain-done" so post-loop stages see the original spec. Registers as stage type `"handoff"`.
- `plan.py` — `run(ctx, PlanOptions)`. Local pipeline only.
- `implement.py` — `run(ctx, ImplementOptions)`. Dual-mode (`kind='local'` /
  `kind='gh'`). For gh: enforces the empty-implementation invariant,
  classifies the outcome (`HeadAdvanced` / `EmptyImpl` /
  `DivergentHead`), creates the impl-handoff branch, and returns an
  `ImplStageResult` with the pre-impl state and classified outcome.
- `review_code.py` — `run(ctx, ReviewCodeOptions)`. Local pipeline only
  (single-detail-reviewer post-collapse).
- `address_code.py` — `run(ctx, AddressCodeOptions)`. Local pipeline only.
- `commit_pr.py` — `run(ctx, CommitPrOptions)`. Gh pipeline. Opens a fresh
  claude session against the impl-handoff branch diff; no session_id
  dependency, so `--resume-from commit-pr` works cleanly.
- `ghreview.py` — `run(ctx, GhreviewOptions)`. Thin wrapper around `/ghreview
  <pr_url>` plus a `check_bail` call.
- `ghaddress.py` — `run(ctx, GhaddressOptions)`. Thin wrapper around `/ghaddress
  <pr_url>`.
- `request_copilot.py` — `run`. Requests Copilot review by adding
  `copilot-pull-request-reviewer` to the PR's reviewer list.
- `verify.py` — `Verify(Stage)`. Constructs a `LoopStage` from two closures (`_run_cmd`, `_run_fix`) and delegates the retry loop to it. `_run_cmd` runs `cmds` joined with `&&` and raises `RunCmdFailed` on failure; `_run_fix` formats the fix prompt and invokes the agent. `LoopExhausted` from `LoopStage` is translated to `RuntimeError("verify stage exhausted N attempts")`. Takes `is_git` (controls diff capture) and `commit_after_fix` (controls commit instruction). Registers as stage type `"verify"`.
- `wait_ci.py` — `run(ctx, WaitCiOptions)`. Gh pipeline (`ci-gate`). Polls PR CI checks via `gh_utils`; re-invokes agent to fix failures; bails on `REVIEW_REQUIRED` or attempt exhaustion. Registers as stage type `"wait-ci"`.
- `wait_copilot.py` — `run`. Polls until Copilot posts a non-PENDING review.

## Conventions

- Stages registered via `register_stage` receive `(ctx: StageContext, options: XxxOptions)` as positional args.
- Stage modules expose `run(ctx: StageContext, options: XxxOptions)` as the orchestrator entry point. Orchestrators call `module.run(ctx, options)` directly.
- Every stage that talks to `claude` takes `client: ClaudeClient` and
  calls `client.run(...)`. **Never spawn `claude -p` directly** — that
  bypasses the test seam in `../clients/protocol.py`.
- Prompt-based claude stages compose `load_prompts(self.prompt_paths)` from the YAML `prompt:`
  list; bundled prompt files live under `gremlins/prompts/`. See
  `gremlins/prompts/README.md` for the runtime placeholder inventory.
- Stages that should respect a bail marker (set by the agent via
  `python -m gremlins.bail`) call `check_bail(<phase-name>)` from
  `..state` after the claude run. The runner inspects the bail and
  halts the pipeline.
- Most stages return `None`. Stages that produce information the
  orchestrator needs (`implement.py` → `ImplStageResult`,
  `commit_pr.py` → PR URL string) return it; the orchestrator threads
  it into later stages.
- The `label=` argument passed to `client.run(...)` is the stream-event
  prefix and the `FakeClaudeClient` fixture key. Stages that re-enter the
  same logical step within one process (e.g. resumed implement) must use
  distinct labels per phase so the fake's lookup doesn't collide.

## Import constraint for new stages

Any new `gremlins/stages/introspect.py` (planned for #258) must import only `inspect` and `gremlins.stages.base.Stage` — never any orchestrator module. This keeps the stages package free of upward dependencies so orchestrators can import stages without cycles.

## Load-bearing invariants

- `implement.py` enforces the empty-implementation invariant: an empty
  impl in the gh pipeline raises `EmptyImpl` and the runner bails. This
  is the firewall that keeps no-op runs out of `commit-pr` / `ghreview`.
  Don't soften it.
- `commit_pr.py` selects its action clause based on the `ImplOutcome`
  classification from the implement stage. The shape
  (`HeadAdvanced`) — keep them aligned.
