# `gremlins/prompts/`

Bundled prompt fragments composed by stages at runtime. Stages load
them via `load_bundled_prompt` / `render_bundled_prompt` from
`gremlins.utils.yaml` and feed the concatenated text to the agent.

## Modules

- `__init__.py` — exposes `BUNDLED_PROMPT_DIR` (the absolute path to this
  directory). All bundled-prompt path resolution goes through
  `BUNDLED_PROMPT_DIR` — don't duplicate the path computation elsewhere.
- `README.md` — runtime placeholder reference. Every `{name}` token used
  in any fragment is documented there with its source stage. Update
  `README.md` when adding or removing a placeholder.

## Conventions

- Fragments are plain Markdown. Static text only — runtime values come in
  via Python `str.format(...)` in the calling stage.
- Literal braces that must survive `.format(...)` (e.g. shell command
  examples like `{{owner}}/{{repo}}`) are doubled in the source.
- Compose, don't inline. A YAML stage entry's `prompt:` list is the unit
  of reuse: `[gremlins:code_style.md, gremlins:plan_gh.md]` is preferred
  over copy-pasting shared text into a new file. Add a new fragment when
  text is reused across two or more stages.
- Bundled prompts are referenced from YAML with the `gremlins:` prefix
  (e.g. `gremlins:code_style.md`); bare names resolve from the pipeline's
  `prompt_dir`. The prefix makes user-authored YAMLs self-describing
  about which prompts ship with the package vs which must be provided
  locally.
- Filenames track the stage they belong to (`plan.md`, `plan_gh.md`,
  `implement_local.md`). Shared fragments use a topic name
  (`code_style.md`, `bail_section.md`).
- The `review/` subdirectory holds review-only fragments. YAMLs reference
  them as `review/detail.md`.
- An empty file is a load error, not silently-empty text. If a fragment
  has nothing to say in some context, the calling stage should pass an
  empty placeholder instead of including the file conditionally.

## Load-bearing invariants

- `BUNDLED_PROMPT_DIR` is the single source of truth for the bundled
  prompt path. The pipeline parser uses it to resolve YAML `prompt:`
  entries; tests depend on it. Don't compute the path ad-hoc.
- The placeholder set in `README.md` is the contract between stages and
  fragments. A stage that introduces a new `{name}` is responsible for
  adding it to the table; a fragment that uses an undocumented
  placeholder will raise `PromptLoadError` at format time.
- `load_bundled_prompt` rejects empty files on purpose — silent empty
  prompts produced confusing agent behavior in the past. Keep the check.
- Stages load bundled prompts via `load_bundled_prompt` / `render_bundled_prompt`
  from `gremlins.utils.yaml`. Direct `BUNDLED_PROMPT_DIR` access in stage
  modules is not permitted.
