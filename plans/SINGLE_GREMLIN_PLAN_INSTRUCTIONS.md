# plans — how to write an implementation plan for a single gremlin

A single-gremlin implementation plan describes exactly what to change, in which files,
and how to verify the result. A gremlin reads the plan, does the work, and produces
a PR — no back-and-forth.

## Scope: one plan, one gremlin

A plan must be completable in one gremlin invocation. If the work is too large,
ask the operator for clarification before writing the plan.

## No design options

A plan states **what will be done and how**. It does not present alternatives,
trade-off matrices, or decision points. If the approach isn't obvious enough
to commit to in writing, the plan isn't ready — ask the operator to clarify before writing the plan.

## Plan anatomy

A plan should answer these questions in order:

### 1. Goal (one sentence)

What changes and why. If the motivation requires more than a sentence, give
it its own short section, but keep it tight.

### 2. Scope (what files are touched)

A crisp list of files or modules affected. This is the contract: the gremlin
should not touch anything outside this list unless the plan explicitly says
it's a "ripple" or "call-site" change with a justification.

### 3. Task decomposition — separate concerns, run in parallel

A plan isn't a linear script — it's a set of independent workstreams.
Before listing file-level changes, decompose the work into tasks that can
scout and implement in parallel via the `Task` tool. This isn't just about
speed; it's a design quality check. If you can't separate the work into
independent tasks, the design likely has tangled concerns that should be
untangled first.

Each task should:

- **Own one concern.** If a task touches three unrelated modules, split it.
- **Be independently verifiable.** A task's work should compile and pass its
own tests without waiting for another task to finish.
- **Self-contained.** A task knows exactly which code to read and what to
change. It doesn't need to coordinate with another task mid-flight;
the plan gives it everything it needs to complete its concern independently.

The plan should list tasks explicitly and flag dependencies:

```
Tasks:
  A. Move yaml_io helpers to _gremlins_core (no deps — can run immediately)
  B. Rewire Python call sites to import from new location (depends on A)
  C. Update Rust-side YAML error types (no deps — parallel with A)
```

If two tasks must be sequential, name the dependency and explain why.
If the reason is weak ("they touch the same file" is not a reason — let
the second task rebase), reconsider whether they're truly one task.

Common decomposition patterns:

| Pattern | When to use |
|---------|-------------|
| **Per-module** | Each task owns a file or module group |
| **Per-layer** | Core logic vs. CLI surface vs. test fixtures |
| **Per-concern** | Error handling, happy path, logging, types |

### 4. Changes (per-task detail)

For each task identified above, specify the concrete changes:

Each change specifies:

- **What file** to edit
- **What to do** in that file (delete a function, add a parameter, re-route an
  import, etc.)
- **Any non-obvious constraints** the implement agent must respect (e.g., "this
  function must handle None inputs," "the error type must match the existing
  pattern in convert.rs").

Describe the transformation, not the code. The implement agent reads the
current source and writes the edit — it does not need the plan to contain
the exact diff. An import-rewiring step is one sentence ("change all
`from gremlins.utils.yaml_io import ...` to
`from _gremlins_core.utils.yaml_io import ...`"), not a diff annotated
with line numbers.

Do not write implementation code in the plan. Providing exact source code
is counterproductive: it leaves the implement agent with nothing to design,
which causes it to fill the vacuum with endless verification loops. If a
change truly needs a code sketch, show only the interface or a minimal
snippet that communicates a constraint — never the full body.

### 5. Test impact

Which tests are affected and whether new tests are needed. If the plan says
"no test impact", say why (e.g., "existing integration tests cover this
through make_runner").

## What a plan is not

- **Not a design document.** Design documents explore trade-offs and justify
  architecture. Plans assume the design is settled and describe the
  implementation.
- **Not a backlog item.** Plans are ready to execute. If there are open
  questions, don't call it a plan.
- **Not a spec for a multi-gremlin chain.** A boss workflow spans multiple
  gremlins; a plan is one gremlin's worth of work. Chain-level coordination
  belongs in the chain spec, not here.
