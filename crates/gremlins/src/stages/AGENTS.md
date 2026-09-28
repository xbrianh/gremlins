# stages/ — pipeline stage definitions

Pipeline YAML stage definitions flow through the builder API into a typed
stage tree (`ParsedStage`). Each YAML `type` string dispatches to a
per-stage builder (`AgentBuilder`, `ExecBuilder`, `SequenceBuilder`,
`ParallelBuilder`), which validates constraints and produces a
`ParsedStage` variant.

## File map

| File | Role |
|---|---|
| `base.rs` | `Stage` trait — common interface every stage type implements. Also `substitute_vars` and `string_options` helpers used by the runtime. |
| `node.rs` | `ParsedStage` enum — the typed stage tree. Pure data: accessors (`name()`, `stage_type()`, `client()`, `skip_if_exists()`, `body()`, `set_name()`), serialization to YAML (`to_yaml`, `to_stage_node`), and `to_stage_entry` for name-filling. No parsing logic. |
| `agent.rs` | `Agent` struct. Leaf stage: prompt list, options, interpolation/bind maps. |
| `exec.rs` | `Exec` struct. Leaf stage: shell commands via `options.cmds`. |
| `sequence.rs` | `Sequence` struct. Composite: iterates its body up to `max_iterations` with an optional `interval`. |
| `parallel.rs` | `ParallelGroup` struct, `ErrorPolicy` enum, child-name validation. Composite: runs children concurrently. |
| `composite.rs` | Shared composite infrastructure: `StageAttrs` (name, type, path, `skip_if_exists`, `client_explicit`), `ClientSpec` newtype, `ChildParams` / `compute_child_params` for fan-out. |
| `constants.rs` | `FRAMEWORK_KEYS` — variable names (`name`, `model`, `cwd`, `base_ref`) reserved for runtime injection and excluded from interpolation maps. |
| `outcome.rs` | `Done` marker — unit type signaling a stage completed without bailing. |
| `builders/agent.rs` | `AgentBuilder` — builder pattern for `ParsedStage::Agent`. Validates interpolation syntax, framework-key collisions, bind/interpolation key collisions, and unused keys. |
| `builders/exec.rs` | `ExecBuilder` — builder pattern for `ParsedStage::Exec`. Same validations as `AgentBuilder`. |
| `builders/composite.rs` | `SequenceBuilder`, `ParallelBuilder` — builder patterns for composite stages. Validates empty-body rejection, max-iterations, nested-parallel rejection, and child-name uniqueness/validity. |
| `builders/definition.rs` | `DefinitionBuilder` — top-level YAML ingestion. `from_yaml` expands, then `stage_from_yaml` dispatches to the per-stage builders. Also `BootstrapBuilder` and `LandBuilder`. |

## Architecture

Parsing happens in two layers:

1. **Schema layer** (`schemas/loader::fill_names`). Runs first. Fills
   names across siblings, handles `_auto_name` from recipe expansion,
   and normalizes the YAML shape.

2. **Builder layer** (`builders/definition.rs::stage_from_yaml`). Runs
   on name-complete YAML. Matches `type` to the correct builder
   (`AgentBuilder`, `ExecBuilder`, `SequenceBuilder`, `ParallelBuilder`),
   calls `.build()` → `ParsedStage`, descends into composite bodies
   (recursing into `stage_from_yaml`), and validates cross-cutting rules
   like `max_concurrent`-on-non-parallel rejection.

Parallel groups use `type: parallel` with a `body:` list — there is no
bare `parallel:` sugar.

## Key invariants

- **`client` is a composite-only concern.** Leaf structs (`Agent`, `Exec`)
  never read the `client` key; it is stored on the `ParsedStage` variant
  by the per-stage builder. Composition resolves inheritance.
- **`skip_if_exists` is composite-only.** Set on `Sequence` and `Parallel`
  via the builder; silently ignored on leaves (returns `""`).
- **Composite bodies are fully parsed.** Children are `Vec<ParsedStage>`,
  not raw YAML. Descent happens in `stage_from_yaml` via `yaml_children`.
- **Framework-key filtering.** `cwd` and `base_ref` are stripped from
  `options` during `to_yaml` serialization — they're runtime-injected,
  not user-visible.
- **Nested `parallel` is rejected.** `ParallelBuilder::build` rejects any
  child that is itself a parallel stage.

## Canonical stage type strings

| User writes | `stage_type()` returns | ParsedStage variant |
|---|---|---|
| `type: agent` | `"agent"` | `ParsedStage::Agent` |
| `type: exec` | `"exec"` | `ParsedStage::Exec` |
| `type: sequence` | `"sequence"` | `ParsedStage::Sequence` |
| `type: parallel` | `"parallel"` | `ParsedStage::Parallel` |

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new stage type | Recipe below; `builders/agent.rs` for leaf, `builders/composite.rs` for composite |
| Understand parse flow | `builders/definition.rs` — `DefinitionBuilder::from_yaml` → `stage_from_yaml` → per-type builder → `ParsedStage` |
| Understand name-filling | `builders/definition.rs::fill_builder_names` → `schemas/loader::fill_names` |
| Change serialization shape | `node.rs` — `to_yaml` / `*_to_yaml` helpers |
| Add a composite-only attribute | `composite.rs` (struct), `node.rs` (serialize), `builders/composite.rs` (builder) |
| Understand variable substitution | `base.rs::substitute_vars` |

## Adding a new stage type

1. **Struct + builder.** Add a module (or extend an existing one) with a
   struct and a builder (e.g., `FooBuilder`) following the pattern in
   `builders/agent.rs` (leaf) or `builders/composite.rs` (composite).
2. **Implement `Stage`.** Implement the `Stage` trait from `base.rs` on
   the new struct — this is the common interface every stage type must
   provide.
3. **ParsedStage variant.** Add a variant to `ParsedStage` in `node.rs`.
   For composites, include `StageAttrs`, `client: Option<ClientSpec>`,
   and `body: Vec<ParsedStage>`.
4. **Wire into `stage_from_yaml`.** Add a match arm and a `foo_from_yaml`
   helper in `builders/definition.rs`.
5. **Serialization.** Add a `*_to_yaml` helper and a match arm in
   `ParsedStage::to_yaml`. Add a match arm in `to_stage_node` for
   bind/interpolation propagation.
6. **Accessors.** Add match arms to `name()`, `stage_type()`, `client()`,
   `skip_if_exists()`, `body()`, `set_name()`.
7. **Tests.** Add parse round-trip tests in `builders/definition.rs`'s
   test module.
