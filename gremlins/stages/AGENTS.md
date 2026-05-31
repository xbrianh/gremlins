# `gremlins/stages/`

Per-stage bodies — the work each pipeline stage actually does. Modules
here are called by orchestrators in `../orchestrators/`; they own no
sequencing logic of their own.

## Modules

- `agent.py` — `Agent(Stage)` (type `"agent"`). The generic agentic primitive. Declared in YAML with `in:` / `out:` maps:
  - `in: {var_name: registry_key}` — resolves each `registry_key` from `state.artifacts`, converts to string, and substitutes `{var_name}` into the rendered prompt. Dotted paths like `pr.branch` walk attributes on the resolved value before stringifying. Raises `MissingArtifact` if a key is not bound.
  - `out: {registry_key: uri_string}` — binds each URI in `state.artifacts` before the agent runs, then calls `resolver.verify_produced(uri)` post-run to assert the artifact was written.
  - Invokes the agent via `agent_runner.run_agent`; the `label` is the stage name.
  - Prompt template is `"\n\n".join(self.prompts)`; if `in:` produces substitution vars, `str.format(**subs)` is applied.
  - `state.artifacts` is required only when `in:` or `out:` maps are non-empty; stages with empty maps (e.g. `implement`) can delegate to `Agent` without a registry.
- `agent_runner.py` — `run_agent`, `_check_bail`. Single chokepoint for agentic execution: delegates to `state.client.run()`, then raises `Bail` if the agent's final message ends with a `BAIL: <class>: <detail>` sentinel line. Import from here (not from `agent.py`) in any stage that needs `run_agent`.
- `loop.py` — `LoopStage(Stage)`. Iterates a `body: list[Stage]` (or raw `body_runners` callables) until a termination predicate fires or `max_iterations` is exhausted. Body stages execute in order; subsequent stages only run when a preceding stage set the `status=needs_fix` marker artifact (a file at `artifact_dir/status` containing `needs_fix`, bound under key `"status"`), except on the final iteration where fix stages are skipped. Termination is controlled by an `until: UntilFn` predicate (default `head_stable`); `max_iters(n)` is a factory for count-bounded loops. The marker is unbound at the start of each iteration.
- `cmd.py` — `Cmd(Stage)`. Runs `options["cmds"]` joined with `&&`; writes output to disk on every invocation. On non-zero exit raises `Bail`. Supports `log_path` option with `{n}` interpolation for per-invocation naming (e.g. `verify-attempt-{n}.log`); falls back to `cmd.log`. Stage type `"cmd"`.
- `exec.py` — `Exec(Stage)`. Runs `options["cmds"]` joined with `&&`; writes combined stdout+stderr to `exec-{name}.log`. On non-zero exit: if `"status"` is in the `out:` map, writes the `needs_fix` marker and returns `Done()` (loop will gate subsequent fix stages); otherwise raises `Bail`. Supports `in:`/`out:` artifact substitution and `git://range` out-URIs. Stage type `"exec"`.
- `parallel.py` — `ParallelStage(Stage)`. Constructed by the orchestrator with pre-built child runners; call `build_runtime_stages()` to get the three `(name, fn)` pairs (`<group>-fanout`, `<group>`, `<group>-fanin`) that implement fan-out/fan-in execution.

## Recipes

Bundled stage recipes live under `gremlins/recipes/stages/`. Each recipe is a multi-primitive YAML pipeline fragment that expands in-place wherever its `gremlins:<name>` type is referenced.

- `plan` — local planning: `resolve-plan-input` → `plan` (agent) → `update-description`. Out: `plan: file://session/plan.md`. The `plan` agent stage has `skip_if_exists: plan`, so a resumed run skips the LLM if a non-empty `plan.md` already exists. Any pipeline using this recipe must declare `inputs: { PLAN: plan?, INSTRUCTIONS: instructions? }` — that block is the canonical source of the `--plan`/`--instructions` flags the recipe depends on (the launcher binds the raw arg as `plan_arg`, which `resolve-plan-input` consumes; `plan` is a distinct output key).
- `plan_gh` — GitHub planning: `resolve-plan-input` → `plan` (agent) → `publish-as-issue` → `update-description`. Out: `plan: gh://issue/{read:plan-issue-number}`. Same `skip_if_exists: plan` on the agent; `publish-as-issue` has `skip_if_exists: plan-issue-number` (idempotency guard against duplicate issues on resume). Same `inputs:` requirement as `plan`.
- `handoff` — boss-loop chain manager: `handoff-init` (exec, sets up `rolling-plan.md` and `boss-spec.md`) → `handoff` (agent, inspects worktree and decides exit state) → `translate-signal` (exec, routes `signal.json` exit_state to loop primitives: `next-plan`→`status=needs_fix`+copy child plan, `chain-done`→restore spec+exit 0, `bail`→write reason+exit 2) → `sanitize` (haiku agent, format-enforces the rolling plan) → `restore-rolling-plan` (exec, restores pre-sanitize copy if output is empty). Wraps all five stages in a `sequence` so the whole unit appears as `i=0` in the boss loop body, keeping gh fix stages (i=1+) conditional on `status=needs_fix`. Session files: `rolling-plan.md`, `boss-spec.md`, `child-plan.md`, `signal.json`.
- `implement` — implementation + progress guard.
- `verify` — run commands, fix loop, bail on exhaustion.
- `review-code` — code review agent, writes `{name}-{model}.md`.
- `github-open-pr` — compose PR title/body, push branch, open PR.
- `github-push-to-pr-branch` — push HEAD to existing PR branch.
- `github-request-copilot-review` — add Copilot as PR reviewer.
- `github-wait-copilot` — poll until Copilot posts a non-pending review.
- `github-wait-ci` — poll CI checks, fix loop, bail on `REVIEW_REQUIRED` or exhaustion.

## Conventions

- YAML stage entries are dispatched via `STAGE_TYPES` in `gremlins/pipeline/loader.py`. Each type string maps to a `Stage` subclass; `parse_stage` calls `StageCls.with_dict(d)` to construct the instance and the executor calls `stage.run(state)` to execute it.
- Every stage that talks to `claude` takes `client: ClaudeClient` and
  calls `client.run(...)`. **Never spawn `claude -p` directly** — that
  bypasses the test seam in `../clients/protocol.py`.
- Prompt-based claude stages join `self.prompts` (already-loaded text list) with `"\n\n"`.
  Bundled internal prompts are loaded via `load_bundled_prompt` / `render_bundled_prompt`
  from `gremlins.utils.yaml_io`. Bundled prompt files live under `gremlins/prompts/`. See
  `gremlins/prompts/README.md` for the runtime placeholder inventory.
- Stages that should respect a bail marker call `run_agent` from `agent_runner`, which parses the agent's final transcript message for a `BAIL: <class>: <detail>` sentinel line and raises `Bail` if found.
- Most stages return `None`.
- The `label=` argument passed to `client.run(...)` is the stream-event
  prefix and the `FakeClaudeClient` fixture key. Stages that re-enter the
  same logical step within one process (e.g. resumed implement) must use
  distinct labels per phase so the fake's lookup doesn't collide.

## Import constraint for new stages

Any new `gremlins/stages/introspect.py` (planned for #258) must import only `inspect` and `gremlins.stages.base.Stage` — never any orchestrator module. This keeps the stages package free of upward dependencies so orchestrators can import stages without cycles.

## Load-bearing invariants

- The empty-implementation invariant is enforced by the `require-impl-progress`
  exec stage in the `implement` stage-definition (gh.yaml). It runs two shell
  checks: HEAD must be a fast-forward from `base_sha`, and at least one commit
  must exist since `base_sha`. Either failure raises `Bail`. This is the
  firewall that keeps no-op runs out of the review stage. Don't soften it.
