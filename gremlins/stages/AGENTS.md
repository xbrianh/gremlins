# `gremlins/stages/`

Per-stage bodies — the work each pipeline stage actually does. Modules
here are called by orchestrators in `../orchestrators/`; they own no
sequencing logic of their own.

## Modules

- `loop.py` — `LoopStage(Stage)`. Iterates a `body: list[Stage]` (or raw `body_runners` callables) until a termination predicate fires or `max_iterations` is exhausted. Body stages execute in order; subsequent stages only run when a preceding stage returned `NeedsFix`, except on the final iteration where fix stages are skipped. Termination is controlled by an `until: UntilFn` predicate (default `head_stable`); `max_iters(n)` is a factory for count-bounded loops. `on_iteration_start` callback runs at the top of each iteration (used by `with_dict` to express `pr_stack` detach-to-prior-PR logic).
- `cmd.py` — `Cmd(Stage)`. Runs `options["cmds"]` joined with `&&`; writes output to disk on every invocation. On non-zero exit returns `NeedsFix(output)`. Supports `log_path` option with `{n}` interpolation for per-invocation naming (e.g. `verify-attempt-{n}.log`); falls back to `cmd.log`. Stage type `"cmd"`.
- `apply.py` — `Apply(Stage)`. Runs `options["cmds"]` sequentially in cwd; on non-zero writes output to apply.log and raises Bail. Then git add -A and commits (name or commit_message) iff staged diff non-empty; skips empty commit. For deterministic post-impl normalizers (e.g. ruff) before verify. Stage type `"apply"`.
- `parallel.py` — `ParallelStage(Stage)`. Constructed by the orchestrator with pre-built child runners; call `build_runtime_stages()` to get the three `(name, fn)` pairs (`<group>-fanout`, `<group>`, `<group>-fanin`) that implement fan-out/fan-in execution.
- `handoff.py` — `Handoff(Stage)` plus the full handoff agent implementation (`run`, `build_prompt`, `collect_git_context`, `sanitize_rolling_plan`, etc.). `Handoff` runs the agent once per boss-loop iteration: returns `Done()` on "chain-done" (loop exits via HEAD-stable), returns `NeedsFix(...)` on "next-plan" (writes child plan to `plan.md`, loop continues), raises `RuntimeError` on "bail". Preserves the original boss spec in `boss-spec.md` and restores it to `plan.md` on "chain-done" so post-loop stages see the original spec. Stage type `"handoff"`.
- `plan.py` — `Plan(Stage)`. Local pipeline only.
- `implement.py` — `Implement(Stage)`. Dual-mode (`kind='local'` / `kind='gh'`). For gh: enforces the empty-implementation invariant, classifies the outcome (`HeadAdvanced` / `EmptyImpl` / `DivergentHead`), creates the impl-handoff branch, and returns an `ImplStageResult` with the pre-impl state and classified outcome.
- `review_code.py` — `ReviewCode(Stage)` (type `"review-code"`): local pipeline only (single-detail-reviewer post-collapse). `GitHubReviewPullRequest(Stage)` (type `"github-review-pull-request"`): posts a PR review to GitHub.
- `address_code.py` — `AddressCode(Stage)` (type `"address-code"`): local pipeline only. `GitHubAddressPullRequestReviews(Stage)` (type `"github-address-pull-request-reviews"`): addresses PR review comments on GitHub.
- `github_request_copilot_review.py` — `GitHubRequestCopilotReview(Stage)`. Requests Copilot review by adding `copilot-pull-request-reviewer` to the PR's reviewer list.
- `verify.py` — `Verify(Stage)` + `VerifyFix(Stage)`. `Verify.run` builds `body=[Cmd, VerifyFix]` and delegates to `LoopStage`. `Cmd` runs the configured commands with per-attempt log naming (`verify-attempt-{n}.log`). `VerifyFix` reads the highest-numbered attempt log from `session_dir` and invokes the fix agent; agent streams to `stream-verify-{n}.jsonl`.
- `github_wait_ci.py` — `GitHubWaitCI(Stage)`. Gh pipeline (`ci-gate`). Polls PR CI checks via `utils.github`; re-invokes agent to fix failures; bails on `REVIEW_REQUIRED` or attempt exhaustion.
- `github_wait_copilot.py` — `GitHubWaitCopilot(Stage)`. Polls until Copilot posts a non-PENDING review.

## Conventions

- YAML stage entries are dispatched via `STAGE_TYPES` in `gremlins/pipeline/loader.py`. Each type string maps to a `Stage` subclass; `parse_stage` calls `StageCls.with_dict(d)` to construct the instance and the executor calls `stage.run(state)` to execute it.
- Every stage that talks to `claude` takes `client: ClaudeClient` and
  calls `client.run(...)`. **Never spawn `claude -p` directly** — that
  bypasses the test seam in `../clients/protocol.py`.
- Prompt-based claude stages join `self.prompts` (already-loaded text list) with `"\n\n"`.
  Bundled internal prompts are loaded via `load_bundled_prompt` / `render_bundled_prompt`
  from `gremlins.utils.yaml_io`. Bundled prompt files live under `gremlins/prompts/`. See
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
  is the firewall that keeps no-op runs out of `github-review-pull-request`.
  Don't soften it.
