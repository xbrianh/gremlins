# `gremlins/stages/`

Per-stage bodies — the work each pipeline stage actually does. Modules
here are called by orchestrators in `../orchestrators/`; they own no
sequencing logic of their own.

## Modules

- `loop.py` — `LoopStage(Stage)`. Iterates pre-built body runner callables until HEAD is stable or `max_iterations` is exhausted. Body runners are called in order; `RunCmdFailed` is caught and noted (subsequent runners still execute so a fix agent can act), except on the final iteration where fix runners are skipped and the stage bails. `LoopStage.from_runners([...], max_iterations=N)` constructs from closures without needing a YAML body. Also exports `RunCmdFailed` (sentinel raised by command-check runners) and `LoopExhausted` (raised on exhaustion so callers can translate the message).
- `run_cmd.py` — `RunCmd(Stage)`. Runs `options["cmds"]` joined with `&&`; writes output to `run-cmd.log` and raises `RunCmdFailed` on non-zero exit. Stage type `"run-cmd"`.
- `parallel.py` — `ParallelStage(Stage)`. Constructed by the orchestrator with pre-built child runners; call `build_runtime_stages()` to get the three `(name, fn)` pairs (`<group>-fanout`, `<group>`, `<group>-fanin`) that implement fan-out/fan-in execution.
- `handoff.py` — `Handoff(Stage)` plus the full handoff agent implementation (`run`, `build_prompt`, `collect_git_context`, `sanitize_rolling_plan`, etc.). `Handoff` runs the agent once per boss-loop iteration: returns normally on "chain-done" (loop exits via HEAD-stable), raises `RunCmdFailed` on "next-plan" (writes child plan to `plan.md`, loop continues), raises `RuntimeError` on "bail". Preserves the original boss spec in `boss-spec.md` and restores it to `plan.md` on "chain-done" so post-loop stages see the original spec. Stage type `"handoff"`.
- `plan.py` — `Plan(Stage)`. Local pipeline only.
- `implement.py` — `Implement(Stage)`. Dual-mode (`kind='local'` / `kind='gh'`). For gh: enforces the empty-implementation invariant, classifies the outcome (`HeadAdvanced` / `EmptyImpl` / `DivergentHead`), creates the impl-handoff branch, and returns an `ImplStageResult` with the pre-impl state and classified outcome.
- `review_code.py` — `ReviewCode(Stage)`. Local pipeline only (single-detail-reviewer post-collapse). Also registered as `"ghreview"` type alias.
- `address_code.py` — `AddressCode(Stage)`. Local pipeline only. Also registered as `"ghaddress"` type alias.
- `github_request_copilot_review.py` — `GitHubRequestCopilotReview(Stage)`. Requests Copilot review by adding `copilot-pull-request-reviewer` to the PR's reviewer list.
- `verify.py` — `Verify(Stage)`. Constructs a `LoopStage` from two closures (`_run_cmd`, `_run_fix`) and delegates the retry loop to it. `_run_cmd` runs `cmds` joined with `&&` and raises `RunCmdFailed` on failure; `_run_fix` formats the fix prompt and invokes the agent. `LoopExhausted` from `LoopStage` is translated to `RuntimeError("verify stage exhausted N attempts")`.
- `github_wait_ci.py` — `GitHubWaitCI(Stage)`. Gh pipeline (`ci-gate`). Polls PR CI checks via `utils.github`; re-invokes agent to fix failures; bails on `REVIEW_REQUIRED` or attempt exhaustion.
- `github_wait_copilot.py` — `GitHubWaitCopilot(Stage)`. Polls until Copilot posts a non-PENDING review.

## Conventions

- YAML stage entries are dispatched via `STAGE_TYPES` in `gremlins/pipeline/loader.py`. Each type string maps to a `Stage` subclass; `parse_stage` calls `StageCls.with_dict(d)` to construct the instance and the executor calls `stage.run(state)` to execute it.
- Every stage that talks to `claude` takes `client: ClaudeClient` and
  calls `client.run(...)`. **Never spawn `claude -p` directly** — that
  bypasses the test seam in `../clients/protocol.py`.
- Prompt-based claude stages join `self.prompts` (already-loaded text list) with `"\n\n"`.
  Bundled internal prompts are loaded via `load_bundled_prompt` / `render_bundled_prompt`
  from `gremlins.utils.yaml`. Bundled prompt files live under `gremlins/prompts/`. See
  `gremlins/prompts/README.md` for the runtime placeholder inventory.
- Stages that should respect a bail marker (written by the agent as
  `bail_$GREMLIN_ATTEMPT.json` in `$GREMLIN_STATE_DIR`) call `check_bail(<phase-name>)`
  from `..state` after the claude run. The runner inspects the bail and
  halts the pipeline.
- Most stages return `None`. Stages that produce information the
  orchestrator needs (`implement.py` → `ImplStageResult`) return it; the orchestrator threads
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
  is the firewall that keeps no-op runs out of `ghreview`.
  Don't soften it.
