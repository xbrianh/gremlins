# gremlins configuration

This project uses [gremlins](https://github.com/amorphous-industries/gremlins) for AI-driven background pipelines. Configuration lives in `.gremlins/`.

## `.gremlins/pipelines/*.yaml`

Each YAML file defines a pipeline. The pipeline's name is the filename stem (e.g. `my-pipeline.yaml` → `my-pipeline`). Key fields:

```yaml
clients:
  <alias>: { provider: claude, model: sonnet }   # model: sonnet | opus | haiku

prompt_dir: ../prompts            # directory bare-name `prompt:` paths resolve against (relative to this YAML; default = YAML dir)

stages:
  - name: <stage-name>
    type: <stage-type>          # plan | implement | verify | review-code | agent | github-wait-ci | …
    client: <alias>             # omit for stages that don't call Claude
    prompt: [gremlins:foo.md, foo.md]   # `gremlins:NAME` -> bundled package prompts; bare NAME -> prompt_dir
    options:                    # stage-specific knobs
      check_cmd: "make check"   # verify: command run as lint/type-check gate
      test_cmd:  "make test"    # verify: command run as test gate
```

Stages run in order. A stage can be wrapped in a `parallel:` group to run concurrently.

To change which model a stage uses, set the appropriate stage option (`plan_model`, `impl_model`, `address_model`, `fix_model`) in the stage's `options:` block, or pass the corresponding CLI flag to `gremlins launch`. The `model` field in the `clients:` block is not used by the built-in `claude` provider.

## `.gremlins/prompts/*.md`

Markdown prompt templates injected into Claude's system prompt for the stage that references them. Edit in place — no re-scaffolding needed. Templates may use subdirectories (e.g. `review/detail.md`).
Bundled defaults for these files live under `gremlins/prompts/` in the package.
