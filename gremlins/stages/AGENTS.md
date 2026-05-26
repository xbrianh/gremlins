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
- `loop.py` — `LoopStage(Stage)`. Iterates a `body: list[Stage]` (or raw `body_runners` callables) until a termination predicate fires or `max_iterations` is exhausted. Body stages execute in order; subsequent stages only run when a preceding stage returned `NeedsFix`, except on the final iteration where fix stages are skipped. Termination is controlled by an `until: UntilFn` predicate (default `head_stable`); `max_iters(n)` is a factory for count-bounded loops. `on_iteration_start` callback runs at the top of each iteration (used by `with_dict` to express `pr_stack` detach-to-prior-PR logic).
- `cmd.py` — `Cmd(Stage)`. Runs `options["cmds"]` joined with `&&`; writes output to disk on every invocation. On non-zero exit returns `NeedsFix(output)`. Supports `log_path` option with `{n}` interpolation for per-invocation naming (e.g. `verify-attempt-{n}.log`); falls back to `cmd.log`. Stage type `"cmd"`.
- `apply.py` — `Apply(Stage)`. Runs `options["cmds"]` sequentially in cwd; on non-zero writes output to apply.log and raises Bail. Then git add -A and commits (name or commit_message) iff staged diff non-empty; skips empty commit. For deterministic post-impl normalizers (e.g. ruff) before verify. Stage type `"apply"`.
- `parallel.py` — `ParallelStage(Stage)`. Constructed by the orchestrator with pre-built child runners; call `build_runtime_stages()` to get the three `(name, fn)` pairs (`<group>-fanout`, `<group>`, `<group>-fanin`) that implement fan-out/fan-in execution.
- `handoff.py` — `Handoff(Stage)` plus the full handoff agent implementation (`run`, `build_prompt`, `collect_git_context`, `sanitize_rolling_plan`, etc.). `Handoff` runs the agent once per boss-loop iteration: returns `Done()` on "chain-done" (loop exits via HEAD-stable), returns `NeedsFix(...)` on "next-plan" (writes child plan to `plan.md`, loop continues), raises `RuntimeError` on "bail". Preserves the original boss spec in `boss-spec.md` and restores it to `plan.md` on "chain-done" so post-loop stages see the original spec. Stage type `"handoff"`.
- `plan.py` — `Plan(Stage)`. Wrapper over `Agent` for the local branch: renders the prompt with `plan_file` / `instructions`, builds `Agent(out_map={"plan": "file://session/plan.md"})`, and delegates. `verify_produced` enforces that the agent wrote a non-empty `plan.md`. The GH branch (`state.engine_ctx.repo` non-empty) keeps a direct `run_agent` call for now; it migrates when `gh://issue/...` artifacts and `extract_gh_url` are replaced in the state-data cutover chunk.
- `review-code` — Code review stage (recipe: `gremlins/recipes/stages/review_code.yaml`). Reads `plan` artifact, writes model-keyed review file `{name}-{model}.md` to the session directory.
- `github-request-copilot-review` — Requests Copilot review (recipe: `gremlins/recipes/stages/github_request_copilot_review.yaml`). Runs `gh pr edit` to add `copilot-pull-request-reviewer`.
- `verify` — Verify stage (recipe: `gremlins/recipes/stages/verify.yaml`). Runs configured commands; on failure captures git diff and invokes a fix agent; loops up to 3 times.
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
