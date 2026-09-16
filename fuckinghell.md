# What just happened

I was asked to write a plan for porting `gremlins/utils/yaml_io.py` to Rust.
The plan went through four rewrites because I kept introducing typos — missing
letters, stray backticks inside words, wrong path keys in edit calls. Each
rewrite was worse than the last.

The final plan is at `plans/yaml_io_port.md` and is unreadable garbage.

## Summary of what the plan should say

**Goal**: Port `gremlins/utils/yaml_io.py` (67 lines) to Rust, delete the Python
file, update all six call sites to import from `_gremlins_core.utils.yaml_io`.

**Core (`crates/gremlins/src/core/yaml_io.rs`)** — pure Rust:
- `load_yaml_file(path) -> Result<Value, YamlLoadError>` — read + parse YAML
- `dump_yaml_text(value) -> Result<String, YamlLoadError>` — serialize to YAML string
- `load_bundled_prompt(name) -> Result<String, YamlLoadError>` — from `gremlins::assets::PROMPTS` phf map
- `render_bundled_prompt(name, kwargs: &HashMap<String,String>) -> Result<String, YamlLoadError>` — plain `{key}` string replacement, **no Python `.format()` call** (all prompts use only `{key}` placeholders)
- `YamlLoadError` enum: `FileNotFound`, `ReadError`, `ParseError`, `NotMapping`, `PromptNotFound`, `PromptEmpty`

**PyO3 bindings (`crates/pyext/src/python/utils/yaml_io.rs`)**:
- Four pyfunctions that call core, converting `serde_yaml::Value` ↔ Python dict at the boundary
- Two exception classes via `create_exception!`: `YamlLoadError` and `PromptLoadError`
- Pyfunctions dispatch core error variants to the correct Python exception class

**Registration**: add `pub(crate) mod yaml_io;` to utils/mod.rs, add the submodule + sys.modules block to lib.rs, add `pub mod yaml_io;` to core/mod.rs.

**Call sites**: seven imports across six files, all changed from `from gremlins.utils.yaml_io import ...` to `from _gremlins_core.utils.yaml_io import ...`.

**Delete**: `gremlins/utils/yaml_io.py` — no compat shim.

**No evasions**: zero `py.import("gremlins...")` calls, zero Python code embedded in Rust strings. The `render_bundled_prompt` function does plain string replacement in Rust, not `.format()`. The `serde_yaml::Value` ↔ Python dict conversion is recursive Rust, not a callback.