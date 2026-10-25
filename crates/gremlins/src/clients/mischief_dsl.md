# Gremlins YAML DSL Reference

## File conventions

| Convention | Example |
|---|---|
| Definition file | `.gremlins/<name>.yaml` (e.g. `.gremlins/gh.yaml`) |
| Stage definition | `.gremlins/stages/<name>.yaml` (e.g. `.gremlins/stages/plan.yaml`) |

## Top-level keys

```yaml
stages:             # ordered sequence of stage mappings (required for execution)
prompt_dir:         # relative directory for prompt file resolution (string)
default_model:      # model reference: "provider/model", "profile/name", or concrete URI
default_task_models: # per-task-kind models: { Scout: provider/model, Implement: provider/model }
bootstrap:          # initialisation: source, launch_cmds, env, cli_out
land:               # final squash stage: interpolation, land_cmds, clean_cmds
stage-definitions:  # named recipes: either an inline stage mapping or a string naming a file under stages/
```

- `name` (optional) — defaults to the YAML file stem.

## Stage types

Every stage has a `type` field. The four built-in types:

### `agent`

An LLM-agent stage with a chat loop.

```yaml
name: my-agent              # string (optional — auto-named if missing)
type: agent
prompt:                     # string or list of strings (one or more prompt file .md names relative to prompt_dir)
model: provider/model       # per-stage model override (optional)
task-models:                # per-task-kind models for this stage (optional)
  Scout: provider/model
  Implement: provider/model
interpolation:
  inputs:
    var_name: content("artifact://path")   # read artifact content into {var_name}
    other: "artifact://path"               # bare URI: resolve to file path
  outputs:
    out_name: "artifact://path"            # bind artifact path (required)
    out_opt?: "artifact://path"            # ? suffix = optional artifact
options:
  …                          # arbitrary key/value pairs passed through
# Turn limits, idle timeout, and event capture are controlled via
# environment variables (GREMLINS_AGENT_MAX_TURNS, GREMLINS_STREAM_IDLE_TIMEOUT)
# and are not stage options.
```

### `exec`

A shell-command stage (no LLM).

```yaml
name: my-exec
type: exec
interpolation:               # same shape as agent
  inputs: { … }
  outputs: { … }
options:
  cmds:                      # required: list of bash -c command strings
    - echo "{my_var}"
  timeout: 300               # seconds (optional)
  …                          # arbitrary key/value pairs passed through
```

### `sequence`

Runs child stages in order. Can loop.

```yaml
name: my-sequence
type: sequence
body:                        # ordered list of child stage mappings
  - type: agent
    prompt: [stuff.md]
  - type: exec
    options: { cmds: [echo done] }
max-iterations: 3            # loop count (default 1)
skip_if_exists: "artifact://path"   # skip entire sequence if artifact exists
interval: 5.0                # seconds between iterations (optional)
model: provider/model        # propagated to children (optional)
task-models: { … }           # propagated to children (optional)
```

### `parallel`

Runs child stages concurrently.

```yaml
name: my-parallel
type: parallel
body:                        # list of child stage mappings
  - type: agent
    prompt: [review.md]
  - type: agent
    prompt: [review.md]
max_concurrent: 4            # max concurrent children (optional)
cancel_on_error: true        # stop all on first failure (default false)
error_policy: any            # "any" (default): fail if any child fails; "all": fail only if all fail
fork:
  cmds:                      # per-child: run before each child is launched
    - git worktree add --detach "$GREMLIN_FORK_WORKDIR" HEAD
join:
  cmds:                      # per-child: run after each child finishes
    # Substitution map is empty for join commands.
    - echo "all done"
```

## Stage definition references

A stage can reference a named definition instead of inlining its config.

The **call site** provides a definition name via `type` and supplies concrete
`prompt` and `options` values:

```yaml
# In a definition file (e.g. .gremlins/gh.yaml):
stages:
  - name: plan
    type: plan                        # references stages/plan.yaml
    prompt: [style_guide.md, plan.md] # concrete prompts
```

The **definition body** (e.g. `.gremlins/stages/plan.yaml`) uses templates to
inject the caller's values:

```yaml
# In .gremlins/stages/plan.yaml:
required-prompt: true
stages:
  - type: agent
    prompt:
      - "{{prompt}}"                  # expands to caller's prompt list
    options:
      timeout: "{{options.timeout | default(600)}}"
```

- `required-prompt: true` in a definition file marks that the caller must supply `prompt`.
- `required-options: [key1, key2]` marks required option keys.
- `{{prompt}}` expands to the caller's prompt value (string or list).
- `{{options.<key>}}` expands to an option from the caller.
- `{{options.<key> | default(<val>)}}` supplies a default if the caller doesn't provide the key.

### Stage definition lookup

- `type: my-def` with no sub-keys other than `prompt`/`options`:
  looks for `my-def` in the parent definition's `stage-definitions` map,
  then in `.gremlins/stages/my-def.yaml`.
- If the value in `stage-definitions` is a string rather than an inline mapping,
  it names the file under `stages/`.
- If the value is an inline mapping, it's used directly as the definition.

## Artifact URIs

| URI | Meaning |
|---|---|
| `artifact://<name>` | Required artifact path. Fails if not produced. |
| `artifact://<name>?` | Optional artifact. Resolves to empty string if missing. |
| `artifact://{loop_iter}/<name>` | Scoped artifact within a sequence loop iteration. |

## Interpolation shapes

### `inputs`

```yaml
inputs:
  my_content: content("artifact://plan.md")      # reads file content → {my_content}
  my_path: "artifact://plan.md"                   # bare URI → file path → {my_path}
  my_optional: "artifact://plan.md?"              # optional bare URI
  my_opt_content: content("artifact://plan.md")?  # optional content
```

- `content()` wraps an artifact URI to inline the artifact's contents rather than its path.
- Bare URIs (no `content()` wrapper) resolve to the file path.
- Optional artifacts (`?` suffix) resolve to empty string when missing.

### `outputs`

```yaml
outputs:
  required_artifact: "artifact://out.md"       # required — fails if not written
  optional_artifact?: "artifact://out.md"      # ? suffix — optional, no error if missing
```

- The `?` in the key name marks the artifact as optional.
- All output values are artifact URIs (they name where the stage writes its output).

### Interpolation in command strings

In `exec` stages and `bootstrap.launch_cmds`, interpolation vars from
`inputs` appear as `{var_name}` tokens. Runtime normalises hyphens ↔
underscores: `{my-var}` and `{my_var}` are equivalent.

Fork commands receive only fixed child-metadata placeholders (`{child_key}`,
`{child_name}`, `{child_id}`, `{parent_id}`, `{group_name}`) plus
environment variables `GREMLIN_WORKDIR` and `GREMLIN_FORK_WORKDIR`.
Join commands receive an empty substitution map — artifact and input
placeholders are not available in join hooks.

## Bootstrap

```yaml
bootstrap:
  source:
    require_one_of: [[instructions, plan]]     # at least one of each group must be non-empty
    instructions:
      type: [filepath, string]                  # accepts either
      optional: true
    plan:
      type: filepath
      optional: true
    base_ref:
      type: string
      optional: true
  launch_cmds:                                  # shell commands run at bootstrap (file setup)
    - gremlins:bind_artifact("artifact://plan.md", plan)
    - gremlins:bind_artifact("artifact://instructions.md", instructions)
    - gremlins:bind_artifact("artifact://base_ref", base_ref)
    - 'BASE_REF={base_ref} && SHA=$(git -C "$GREMLINS_PROJECT_ROOT" rev-parse --verify "${BASE_REF:-HEAD}") && git -C "$GREMLINS_PROJECT_ROOT" worktree add --detach "$GREMLIN_WORKDIR" "$SHA" && echo "$SHA" > "$GREMLINS_STATE_DIR/artifacts/base_sha"'
  env: |                                        # shell script sourced into environment
    export CARGO_INCREMENTAL=0
  cli_out:                                       # artifact registrations at bootstrap time
    base_sha: "artifact://base_sha"
```

- `source` declares input parameters. `type` can be `string`, `filepath`, or a list of both.
- `require_one_of` groups: each inner list names sources; at least one per group must be provided.
- `launch_cmds` run after source values are available. `gremlins:bind_artifact(uri, key)` binds
  a source value to an artifact for downstream stages.
- `env` is a shell script sourced before every stage.
- `cli_out` registers artifacts into the store during bootstrap by binding files that were
  written by `launch_cmds`. It does not emit output on its own — it is a mechanical
  registration step that happens before any pipeline stage runs.

## Land

```yaml
land:
  interpolation:
    inputs:
      message: artifact://commit_message.txt
  land_cmds:                                     # commands to execute (e.g. merge worktree, push)
    - WORKTREE_HEAD=$(git -C "$GREMLIN_WORKDIR" rev-parse HEAD)
    - git merge --squash "$WORKTREE_HEAD"
    - 'git diff --cached --quiet || git commit -F "{message}"'
  clean_cmds:                                    # cleanup commands (run on both success and failure)
    - git worktree remove --force "$GREMLIN_WORKDIR" || true
    - git worktree prune
```

- `land` is a final exec stage that runs **explicitly** via `gremlins land <id>`, not
  automatically after pipeline execution.
- `land.inputs` uses bare artifact URIs (not `content()`): `git commit -F` expects
  a file path, not inline text. See `.gremlins/local.yaml` for a working example.
- `clean_cmds` run separately during `Gremlin::clean` (triggered by `gremlins rm`
  or `gremlins clean`), not automatically after `land_cmds`.

## `skip_if_exists`

Available on `sequence` stages only (and definition references that expand to
sequences). Agent and exec stages do not read it, and `run_parallel` discards
it. Wrap non-sequence stages in a guarded sequence when a skip guard is needed.

```yaml
skip_if_exists: "artifact://plan.md"
```

- If the artifact already exists at stage start, the entire stage is skipped.
- The value is always an artifact URI string.

## Environment variables

These are set by the runtime and available in all commands:

| Variable | Meaning |
|---|---|
| `GREMLIN_WORKDIR` | Per-gremlin isolated worktree |
| `GREMLINS_PROJECT_ROOT` | The project root (parent of `.gremlins/`) |
| `GREMLINS_STATE_DIR` | Per-run state directory |
| `GREMLIN_FORK_WORKDIR` | Isolated worktree for a `parallel` fork child |

## Model references

Model references use the `provider/model` format, e.g. `openai/gpt-4o`,
`anthropic/claude-sonnet-4-20250514`. Profile references use `profile/name`.

- `default_model` — sets the model for all stages without an explicit `model`.
- `model` on a stage — overrides for that stage and its children (if composite).
- `task-models` / `default_task_models` — per-kind model overrides for Task tool workers.
  Valid task kinds: `Scout`, `Implement`. (`Fork` always inherits the parent model.)