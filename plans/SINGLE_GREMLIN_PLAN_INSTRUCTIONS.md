# plans — how to write an implementation plan for a single gremlin

A single-gremlin implementation plan describes the changes to make, at the
level of crates, packages, and modules. The gremlin explores the repo, reads
the code, and figures out the specific files and edits — the plan gives it
the destination, not driving directions.

## Scope: one plan, one gremlin

A plan must be completable in one gremlin invocation. If the work is too large,
ask the operator for clarification before writing the plan.

## No design options

A plan states **what will be done**. It does not present alternatives,
trade-off matrices, or decision points. If the approach isn't obvious enough
to commit to in writing, the plan isn't ready — ask the operator to clarify
before writing the plan.

## Plan anatomy

### 1. Goal (one sentence)

What changes and why. If the motivation requires more than a sentence, give
it its own short section, but keep it tight.

### 2. Changes

Describe the changes at the level of crates, packages, and modules — not
individual files, line numbers, or diffs. The repo's own AGENTS.md files
already document what each piece is for; the plan says what happens to them.

Each change names a crate, package, or module and describes the general
transformation:

- **Good:** "Add a `yaml_io` module to the `_gremlins_core` Rust crate.
  All Python call sites that currently import from `gremlins.utils.yaml_io`
  switch to `_gremlins_core.utils.yaml_io`. Delete the Python
  `gremlins/utils/yaml_io.py` module."
- **Avoid:** "Edit `gremlins/executor/gremlin.py` line 31: change import
  from `gremlins.utils.yaml_io` to `_gremlins_core.utils.yaml_io`."

The gremlin reads the code and discovers which specific files need editing.
Giving it a file-by-file script is both wasteful (it verifies every detail
anyway) and counterproductive (it fills the vacuum with endless verification
loops).

### 3. Constraints and guardrails

Non-obvious rules the gremlin must respect. These are the gotchas that
wouldn't be apparent from reading the code or the project's AGENTS.md files:

- "Must handle None inputs" (when the type system won't catch it)
- "No backward-compatibility shims" (project convention)
- "The error type must follow the pattern in convert.rs" (consistency)
- "Don't touch cmd_backend.rs or openrouter_backend.rs" (intentional exclusion)

Omit constraints that are already obvious from the code or project
conventions. Every constraint here should earn its place by preventing a
likely mistake.

### 4. Verification

How to know the work is done, in behavioral terms. The project already has
`make test` and `make check`; the gremlin knows to run them. This section
is about what correctness looks like:

- What behavior should hold when the work is done (e.g., "importing
  `load_yaml_file` from `_gremlins_core.utils.yaml_io` works; the old
  `gremlins.utils.yaml_io` module no longer exists")
- Whether new tests are needed and what invariants they should assert
- Any non-obvious verification step that `make test` won't catch
  (e.g., "confirm the old module is absent from `pip show` file listing")

Don't list test files or test commands. The gremlin uses the project's
standard tooling to verify its work. This section describes what "correct"
means, not how to run the suite.

## What a plan is not

- **Not a design document.** Design documents explore trade-offs and justify
  architecture. Plans assume the design is settled and describe the
  implementation.
- **Not a backlog item.** Plans are ready to execute. If there are open
  questions, don't call it a plan.
- **Not a change script.** The plan does not list files, line numbers, or
  diffs. The gremlin figures out the specifics by reading the code.
- **Not a spec for a multi-gremlin chain.** A boss workflow spans multiple
  gremlins; a plan is one gremlin's worth of work. Chain-level coordination
  belongs in the chain spec, not here.
- **Not an orientation document.** The repo's AGENTS.md files and README.md
  already describe what each crate, package, and module is for. The plan
  assumes the gremlin reads them. Don't repeat that information here.