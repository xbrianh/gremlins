# gremlins

Background orchestration pipelines for Claude Code. Given a goal or GitHub issue,
a gremlin runs the full plan → implement → review-code → address-code cycle
unattended, writing artifacts to `~/.local/state/claude-gremlins/` and optionally
opening a pull request. A fleet manager tracks running, stalled, and finished
gremlins and provides stop / rescue / land / close operations.

**Status:** pre-1.0, not published to PyPI. The copy at `~/.claude/gremlins/`
is still what Claude Code skills (`/localgremlin`, `/ghgremlin`, `/bossgremlin`)
consume today — this repo is the upstream source.

---

## Dev install

```sh
uv venv
source .venv/bin/activate  # or `.venv\Scripts\activate` on Windows
uv pip install -e ".[dev]"
```

## Make targets

| Target | What it runs |
|---|---|
| `make test` | `pytest` |
| `make lint` | `ruff check .` |
| `make format` | `ruff format --check .` (check only — does not rewrite files) |
| `make typecheck` | `pyright` |
| `make check` | lint + format + typecheck |

## CLI subcommands

Invoked as `python -m gremlins.cli <subcommand>` or `gremlins <subcommand>`
after install. The authoritative list and per-subcommand description lives in
the module docstring at the top of [`gremlins/cli.py`](gremlins/cli.py).

| Subcommand | Purpose |
|---|---|
| `launch local` | Full local pipeline: plan → implement → review-code → address-code |
| `launch gh` | GitHub issue-driven pipeline (plan → implement → PR → Copilot review → address) |
| `launch boss` | Chained serial workflow driven by a top-level spec |
| `review` | review-code stage only |
| `address` | address-code stage only |
| `resume` | Re-spawn an existing gremlin from its recorded stage |
| `stop` | Send SIGTERM to a running gremlin |
| `rescue` | Diagnose and resume a dead or stalled gremlin |
| `land` | Land a finished gremlin onto the current branch |
| `rm` | Delete a dead gremlin's state dir, worktree, and branch |
| `close` | Mark a dead gremlin as closed |
| `log` | Tail the gremlin's log file |

`_run-pipeline` is an internal spawn boundary; not for direct use.

### Launch flags

#### Common flags (all kinds)

| Flag | Default | Description |
|---|---|---|
| `--plan <path-or-ref>` | — | Path to a plan/spec file, or (for `gh`/`boss`) a GitHub issue ref (`42`, `#42`, `owner/repo#42`, or issue URL) |
| `--description <text>` | — | Human-readable description stored in state |
| `--parent <id>` | — | Parent gremlin ID (used by boss to track child ownership) |
| `--print-id` | false | Print the gremlin ID to stdout after launch |
| `-c`/`--instructions <text>` | — | Instructions string (mutually exclusive with `--plan`); not applicable to `launch boss` |
| `--base-ref <ref>` | `HEAD` | Git ref to branch the worktree from; ignored for `gh` (always anchors to origin default branch) |
| `--spec <path>` | — | Path to a coding-style spec file passed into stages; not applicable to `launch boss` |

#### `launch local` flags

| Flag | Default | Description |
|---|---|---|
| `<instructions>` | — | Positional instructions string (mutually exclusive with `--plan` and `-c`) |
| `-p <model>` | `sonnet` | Model for the plan stage |
| `-i <model>` | `sonnet` | Model for the implement stage |
| `-x <model>` | `sonnet` | Model for the address stage |
| `-b <model>` | `sonnet` | Model for the detail-review stage |
| `-t <model>` | `sonnet` | Model for the test-fix stage |
| `--resume-from <stage>` | — | Resume from the named stage instead of starting over |
| `--cmd <command>` | — | Verification command; may be repeated for multiple commands |
| `--test-max-attempts <n>` | `3` | Maximum test-fix retry attempts (must be ≥ 1) |
| `--pipeline <name-or-path>` | `local` | Pipeline to run (see Pipeline configuration) |
| `--client <provider:model>` | — | Override the pipeline-level default client (per-stage `client:` in YAML still takes precedence; e.g. `claude:sonnet`, `copilot:gpt-5.4`) |

#### `launch gh` flags

| Flag | Default | Description |
|---|---|---|
| `-r <ref>` | — | GitHub issue or PR reference (e.g. `42`, `#42`, `owner/repo#42`) |
| `--model <model>` | — | Override the default model for all stages |
| `--resume-from <stage>` | — | Resume from the named stage instead of starting over |
| `--pipeline <name-or-path>` | `gh` | Pipeline to run (see Pipeline configuration) |
| `--client <provider:model>` | — | Override the pipeline-level default client (per-stage `client:` in YAML still takes precedence; e.g. `claude:sonnet`, `copilot:gpt-5.4`) |

#### `launch boss` flags

| Flag | Default | Description |
|---|---|---|
| `--chain-kind <kind>` | required | Kind of child gremlins to spawn: `local` or `gh` |
| `--model <model>` | `sonnet` | Model for the handoff decision agent |
| `--resume-from <stage>` | — | Ignored at the boss level (boss resumes from `boss_state.json`) |
| `--test <command>` | — | Test command forwarded to each child gremlin; only valid with `--chain-kind local` (rejected for `gh`) |
| `--test-max-attempts <n>` | `3` | Maximum test-fix retry attempts forwarded to each child |
| `-t <model>` | `sonnet` | Test-fix model forwarded to each child |

`boss` does not accept `--pipeline`; child pipeline args are built internally from `--test`/`--test-max-attempts`/`-t`.

## Pipeline configuration

Gremlins runs a sequence of stages defined in a YAML file. The bundled
pipelines work out of the box; a project-local YAML can override any of them.

### Discovery order

`--pipeline <name|path>` resolves as follows:

1. A value with a `.yaml` suffix or more than one path component is loaded
   directly as a filesystem path.
2. Otherwise `./.gremlins/pipelines/<name>.yaml` is checked first
   (project-local override).
3. Then `gremlins/pipelines/<name>.yaml` (bundled) is checked.

Defaults: `launch local` → `local`, `launch gh` → `gh`.

### Selecting a pipeline

```sh
gremlins launch local                                          # bundled local.yaml
gremlins launch local --pipeline my-pipeline                   # .gremlins/pipelines/my-pipeline.yaml
gremlins launch local --pipeline .gremlins/pipelines/foo.yaml  # direct path
gremlins launch gh --pipeline gh                               # bundled gh.yaml
```

### Schema reference

**Top-level keys:**

```yaml
name: my-pipeline         # optional; defaults to the file stem

default_client: claude:sonnet   # optional; provider:model string

stages:
  - name: plan
    type: plan
    client: copilot:gpt-5.4     # optional; overrides default_client for this stage
    prompt: prompts/plan.md
    options: {}
```

| Key | Description |
|---|---|
| `name` | Pipeline display name; defaults to the file stem |
| `default_client` | `provider:model` string used for stages without an explicit `client:` |
| `stages` | Ordered list of stage entries or parallel groups |

**Per-stage keys:**

| Key | Description |
|---|---|
| `name` | Unique stage identifier; used for `resume` targeting |
| `type` | Registered stage type (see [Available stage types](#available-stage-types)) |
| `client` | `provider:model` string; overrides `default_client` for this stage |
| `prompt` | Path or list of paths, relative to the YAML file |
| `options` | Free-form dict passed to the stage |

**`provider:model` format:**

Providers: `claude` (default), `copilot`. The model part is optional — `claude:` and `claude:sonnet` are both valid. Examples: `claude:sonnet`, `copilot:gpt-5.4`, `claude:`. Per-stage `client:` in YAML takes precedence over the CLI `--client` flag; `default_client:` at the pipeline level does not.

**Parallel-group form:**

```yaml
- name: reviews
  parallel:
    - name: review-detail
      type: review-code
      client: claude:sonnet
    - name: review-security
      type: review-code
      client: claude:sonnet
  max_concurrent: 2         # optional; defaults to all children at once
```

| Key | Description |
|---|---|
| `name` | Group identifier |
| `parallel` | List of child stage entries (no nesting allowed) |
| `max_concurrent` | Max simultaneously running children (optional) |

### Client specifiers

Clients are specified as `provider:model` inline strings, either at the pipeline level (`default_client:`) or per stage (`client:`). The model part is optional.

```yaml
default_client: claude:sonnet     # all stages default to this
stages:
  - name: plan
    type: plan
  - name: implement
    type: implement
    client: copilot:gpt-5.4       # this stage uses copilot instead
```

Providers: `claude`, `copilot`. The CLI `--client provider:model` flag overrides the pipeline-level `default_client:` but yields to per-stage `client:` settings.

### `prompt:` field

```yaml
prompt: prompts/plan.md                                  # single file
prompt: [prompts/code_style.md, prompts/plan.md]         # list — concatenated with \n\n
```

Paths are relative to the YAML file. Lists are joined with `\n\n` before
being passed to the stage.

By convention, project-local prompts live in `./.gremlins/prompts/` (a peer
of `./.gremlins/pipelines/`, not nested under it) and pipelines reference
them as `../prompts/<file>.md`. There is no search fallback — paths are
explicit. To reuse a bundled prompt, copy the file from
`gremlins/pipelines/prompts/` into `./.gremlins/prompts/`.

### `options:` field

A free-form dict passed verbatim to the stage. Selected options by stage
(see [`gremlins/stages/AGENTS.md`](gremlins/stages/AGENTS.md) for the full list):

**`verify`** — runs `check_cmd` then `test_cmd`, with an agent fix-loop:

```yaml
options:
  check_cmd: make check   # lint/typecheck command (optional)
  test_cmd: make test     # test command (optional)
  max_attempts: 3         # fix-loop retries (default: 3)
```

**`test`** — runs a single test command, with an agent fix-loop:

```yaml
options:
  test_cmd: pytest        # falls back to --test CLI flag; stage no-ops if unset in both
  max_attempts: 3         # fix-loop retries (default: 3)
```

For `local` stages, model options (`plan_model`, `impl_model`, `address_model`,
`test_fix_model`, `detail`) can also be set here to override the CLI defaults.

### Available stage types

| Type | Description |
|---|---|
| `plan` | Produces an implementation plan |
| `implement` | Applies the plan to the working tree |
| `review-code` | Runs a code review and writes findings to disk |
| `address-code` | Applies code-review findings |
| `verify` | Runs check and test commands with an agent fix-loop |
| `test` | Runs a single test command with an agent fix-loop |
| `commit-pr` | Commits changes and opens a pull request |
| `request-copilot` | Requests a Copilot review on the open PR |
| `ghreview` | Runs the `/ghreview` skill against the open PR |
| `wait-copilot` | Polls until Copilot posts its review |
| `ghaddress` | Runs the `/ghaddress` skill to address PR review comments |
| `wait-ci` | Polls PR CI checks until they pass or exhaust attempts |

### Parallel groups

Wrap sibling stages in a `parallel:` list to run them concurrently:

```yaml
default_client: claude:sonnet

stages:
  - name: plan
    type: plan

  - name: reviews
    parallel:
      - name: review-detail
        type: review-code
      - name: review-security
        type: review-code
    max_concurrent: 2

  - name: address-code
    type: address-code
```

If any child fails, the pipeline halts after the group finishes — siblings
are not cancelled mid-run. `gremlins resume` accepts both the group name
(`reviews`) and individual child names (`review-detail`).

### Worked example: project-local override

Create `.gremlins/pipelines/local.yaml` to override the bundled `local`
pipeline. This example uses Opus for plan/implement/address stages and adds
a `test` stage before `review-code`:

```yaml
name: local

stages:
  - { name: plan,         type: plan,         options: { plan_model: opus } }
  - { name: implement,    type: implement,    options: { impl_model: opus } }
  - { name: test,         type: test,         options: { test_cmd: pytest } }
  - { name: review-code,  type: review-code }
  - { name: address-code, type: address-code, options: { address_model: opus } }
```

Add a `prompt:` key to any stage to supply a custom prompt; paths are
relative to the YAML file.

### Worked example: parallel reviewers

Run two `review-code` passes in parallel, then address both:

```yaml
name: local

default_client: claude:sonnet

stages:
  - { name: plan,      type: plan }
  - { name: implement, type: implement }

  - name: reviews
    parallel:
      - name: review-detail
        type: review-code
      - name: review-security
        type: review-code
    max_concurrent: 2

  - { name: address-code, type: address-code }
```

Note: `review-code` does not currently support per-stage prompt overrides
via YAML — both passes use the built-in detail lens.

### Bundled pipelines

The canonical reference pipelines:

- [`gremlins/pipelines/local.yaml`](gremlins/pipelines/local.yaml) — default for `launch local`
- [`gremlins/pipelines/gh.yaml`](gremlins/pipelines/gh.yaml) — default for `launch gh`

### Local environment overrides

If `.gremlins/env` exists in the project root, gremlins sources it through
`bash` at startup and merges any new or changed variables into the process
environment before any stage runs. All subprocesses (plan, implement, verify,
review) inherit the result automatically.

> **Security warning:** because `.gremlins/env` is executed as a bash script,
> it can run arbitrary code. Do not run gremlins in a repository unless you
> have reviewed the contents of `.gremlins/env` and trust them.

The file is sourced via `bash`, so it can use command substitution,
conditionals, and anything bash supports:

```sh
export VIRTUAL_ENV=$(poetry env info --path)
export PATH="$VIRTUAL_ENV/bin:$PATH"
export TEST_DATABASE_URL=postgresql://localhost/mydb_test
```

`gremlins init` writes `.gremlins/.gitignore` with `env` so the file is
gitignored by default. Add it to your `~/.gitignore_global` or project
`.gitignore` if you don't use `gremlins init`.

### Loader API

`gremlins/pipeline.py` exposes:

- `load_pipeline(path)` → `Pipeline` — parses a YAML file, resolves `clients`
  via `CLIENT_FACTORIES`, and validates every stage `type` against
  `STAGE_REGISTRY` (populated by importing `gremlins.stages.all`).
- `resolve_pipeline_path(name_or_path, base_dir)` — resolves a name or path
  using the discovery order above.

Dataclasses: `Pipeline`, `StageEntry` (parallel groups have `type="parallel"`
internally and carry a `children` list and optional `max_concurrent`).

## Internals docs

- [`gremlins/AGENTS.md`](gremlins/AGENTS.md) — module layout, entry points,
  testability seam, byte-stable strings
- [`gremlins/fleet/AGENTS.md`](gremlins/fleet/AGENTS.md) — fleet manager internals
- [`gremlins/orchestrators/AGENTS.md`](gremlins/orchestrators/AGENTS.md) — orchestrator internals
- [`gremlins/stages/AGENTS.md`](gremlins/stages/AGENTS.md) — stage internals

## Planned: `gremlins install`

A future `gremlins install` subcommand will bootstrap the Claude Code skill
layer — syncing this package into `~/.claude/gremlins/` and wiring up the
hook and skill definitions. **This command does not exist yet.**
