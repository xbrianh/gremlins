# stages/ — pipeline stage definitions

Parses pipeline YAML stage definitions into a typed stage tree. Each YAML `type` string maps to a `ParsedStage` variant, backed by a dedicated struct with its own `from_dict` parser.

## File map

| File | Role |
|---|---|
| `base.rs` | `Stage` trait — common interface every stage type implements. Also `substitute_vars` and `string_options` helpers used by the runtime. |
| `node.rs` | `ParsedStage` enum — the typed stage tree. `parse_stages` / `parse` entry points, serialization to YAML (`to_yaml`, `to_stage_node`). |
| `agent.rs` | `Agent` struct + `from_dict`. Leaf stage: prompt list, options, interpolation/bind maps. |
| `exec.rs` | `Exec` struct + `from_dict`. Leaf stage: shell commands via `options.cmds`. |
| `sequence.rs` | `Sequence` struct + `with_dict`. Composite: iterates its body up to `max_iterations` with an optional `interval`. |
| `parallel.rs` | `ParallelGroup` struct + `with_dict`, `ErrorPolicy` enum, child-name validation. Composite: runs children concurrently. |
| `composite.rs` | Shared composite infrastructure: `StageAttrs` (name, type, path, `skip_if_exists`, `client_explicit`), `ClientSpec` newtype, `get_client_from_dict`, `ChildParams` / `compute_child_params` for fan-out. |
| `constants.rs` | `FRAMEWORK_KEYS` — variable names (`name`, `model`, `cwd`, `base_ref`) reserved for runtime injection and excluded from interpolation maps. |
| `outcome.rs` | `Done` marker — unit type signaling a stage completed without bailing. |

## Two-parser architecture

Parsing happens in two layers, both visible in `node.rs`:

1. **Schema layer** (`schemas/loader`). Runs first. Fills names across siblings (`fill_names`), handles `_auto_name` from recipe expansion, and normalizes the YAML shape. This is the same code shared with the Python loader path.

2. **Stage layer** (`ParsedStage::parse`). Runs second on name-complete YAML. Matches `type` to the correct variant, calls each struct's `from_dict` / `with_dict`, descends into composite bodies (recursing into `parse_stages`), and validates cross-cutting rules like `max_concurrent`-on-non-parallel rejection.

The `parallel:` sugar (bare `parallel:` key without `type`) is normalized to `type: parallel` early in `ParsedStage::parse` so both spellings converge on the same `parse_parallel` path.

## Key invariants

- **`client` is a composite-only concern.** Leaf structs (`Agent`, `Exec`) never read the `client` key; `get_client_from_dict` is called in `node.rs` and stored on the `ParsedStage` variant. Composition resolves inheritance.
- **`skip_if_exists` is composite-only.** Parsed in `node.rs` for `sequence` and `parallel`; silently ignored on leaves (returns `""`).
- **Composite bodies are fully parsed.** Children are `Vec<ParsedStage>`, not raw YAML. Descent happens in `node.rs` via `parse_body`.
- **Framework-key filtering.** `cwd` and `base_ref` are stripped from `options` during `to_yaml` serialization — they're runtime-injected, not user-visible.
- **Nested `parallel` is rejected.** Depth tracking in `parse_parallel` prevents parallel-inside-parallel.

## Canonical stage type strings

| User writes | `stage_type()` returns | ParsedStage variant |
|---|---|---|
| `type: agent` | `"agent"` | `ParsedStage::Agent` |
| `type: exec` | `"exec"` | `ParsedStage::Exec` |
| `type: sequence` | `"sequence"` | `ParsedStage::Sequence` |
| `type: parallel` | `"parallel"` | `ParsedStage::Parallel` |
| `parallel:` (sugar) | `"parallel"` | `ParsedStage::Parallel` |

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new stage type | Recipe below; `agent.rs` for leaf, `sequence.rs` for composite |
| Understand parse flow | `node.rs` — `parse_stages` → `fill_names` → `ParsedStage::parse` |
| Understand name-filling | `node.rs::fill_names` → `schemas/loader::fill_names` |
| Change serialization shape | `node.rs` — `*_to_yaml` helpers |
| Add a composite-only attribute | `composite.rs` (struct), `node.rs` (parse + serialize) |
| Understand variable substitution | `base.rs::substitute_vars` |

## Adding a new stage type

1. **Struct + parser.** Add a module (or extend an existing one) with a struct and a `from_dict` / `with_dict` that consumes `&HashMap<String, serde_json::Value>`.
2. **Implement `Stage`.** Implement the `Stage` trait from `base.rs` on the new struct — this is the common interface every stage type must provide.
3. **ParsedStage variant.** Add a variant to `ParsedStage` in `node.rs`. For composites, include `StageAttrs`, `client: Option<ClientSpec>`, and `body: Vec<ParsedStage>`.
4. **Wire into `ParsedStage::parse`.** Add a match arm under the stage type string.
5. **Serialization.** Add a `*_to_yaml` helper and a match arm in `ParsedStage::to_yaml`. Add a match arm in `to_stage_node` for bind/interpolation propagation.
6. **Accessors.** Add match arms to `name()`, `stage_type()`, `client()`, `skip_if_exists()`, `body()`, `set_name()`.
7. **Tests.** Add parse round-trip tests in `node.rs`'s test module.

