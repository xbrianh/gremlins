<!-- placeholders: plan, instructions -->
You are creating an implementation plan for a single gremlin.

## Where to write

Write your final plan to the file at `{plan}`. This file does not exist yet — you are creating it.

## Instructions

{instructions}

## Gather context

Read any relevant source files to understand the existing code and identify what needs to change.

## Plan anatomy

Write the plan following the structure below. The gremlin explores the repo, reads the code, and figures out the specific files and edits — the plan gives it the destination, not driving directions.

### 1. Goal (one sentence)

What changes and why. If the motivation requires more than a sentence, give it its own short section, but keep it tight.

### 2. Changes

Describe the changes at the level of crates, packages, and modules — not individual files, line numbers, or diffs. The repo's own AGENTS.md files already document what each piece is for; the plan says what happens to them.

Each change names a crate, package, or module and describes the general transformation:

- **Good:** "Add a `yaml_io` module to the `_gremlins_core` Rust crate. All Python call sites that currently import from `gremlins.utils.yaml_io` switch to `_gremlins_core.utils.yaml_io`. Delete the Python `gremlins/utils/yaml_io.py` module."
- **Avoid:** "Edit `gremlins/executor/gremlin.py` line 31: change import from `gremlins.utils.yaml_io` to `_gremlins_core.utils.yaml_io`."

The gremlin reads the code and discovers which specific files need editing. Giving it a file-by-file script is both wasteful and counterproductive.

### 3. Constraints and guardrails

Non-obvious rules the gremlin must respect — gotchas that wouldn't be apparent from reading the code or the project's AGENTS.md files:

- "Must handle None inputs" (when the type system won't catch it)
- "No backward-compatibility shims" (project convention)
- "The error type must follow the pattern in convert.rs" (consistency)
- "Don't touch cmd_backend.rs or openrouter_backend.rs" (intentional exclusion)

Omit constraints that are already obvious from the code or project conventions.

### 4. Verification

How to know the work is done, in behavioral terms. The project already has `make test` and `make check`; describe what correctness looks like:

- What behavior should hold when the work is done
- Whether new tests are needed and what invariants they should assert
- Any non-obvious verification step that `make test` won't catch

Don't list test files or test commands — describe what "correct" means, not how to run the suite.

## Rules

- **No design options.** State what will be done. If the approach isn't obvious enough to commit to in writing, the plan isn't ready.
- **One gremlin's worth.** The plan must be completable in one gremlin invocation. If the work is too large, say so.
- **Not a change script.** Do not list files, line numbers, or diffs. Describe at the level of crates, packages, and modules.

Start the plan with a `# Title` H1 header that concisely summarizes the work.