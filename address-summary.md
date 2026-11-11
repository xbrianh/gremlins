# Fixes Applied

## 1. `crates/gremlins/src/clients/tools/tool_specs.rs`, line 34
**Category**: Readability (typo)
**Fix**: Changed "byte-for-identical" → "byte-for-byte identical" — restored the original phrasing that was mangled during the move.

## 2. `crates/gremlins/src/clients/tools/tool_specs.rs`, lines 31-32 (removed)
**Category**: Duplication / Layering
**Fix**: Removed `chat_only_always_available()` function entirely. It hard-coded `name == "Mischief" || name == "WebSearch"`, duplicating the `chat_only()` trait hook. Added `chat_only: bool` field to `ToolSpec` instead, so `definitions.rs` can query the flag without enumerating tool names.

## 3. `crates/gremlins/src/clients/tools/tool_specs.rs`, struct `ToolSpec`
**Category**: Layering
**Fix**: Added `chat_only: bool` field. Set `true` for WebSearch, Mischief, and LoadSkill specs; `false` for all others. This makes the chat-only policy declarative on the spec table rather than scattered across name-string comparisons in `definitions.rs` and `chat_only_always_available()`.

## 4. `crates/gremlins/src/clients/tools/registry.rs`, line ~101 (new hook)
**Category**: Layering
**Fix**: Added `bypass_filter() -> bool` trait hook (default `false`) to `ToolHandler`. This replaces the `name != "LoadSkill"` special case in `dispatch.rs`.

## 5. `crates/gremlins/src/clients/tools/handlers/skills.rs`, line 12
**Category**: Layering
**Fix**: Implemented `bypass_filter() -> true` on `LoadSkillTool`.

## 6. `crates/gremlins/src/clients/tools/dispatch.rs`, lines 20-42
**Category**: Layering / Correctness
**Fix**: Refactored `invoke_with_conversation`:
- Look up the handler once at the top, reuse in all three spots (gate, repair_args, invoke).
- Replaced `name != "LoadSkill"` with `handler.bypass_filter()`.
- Replaced `chat_only_always_available(name, chat)` with `chat && handler.chat_only()`.
- Removed import of `chat_only_always_available`.
Dispatch no longer contains any tool-name comparisons.

## 7. `crates/gremlins/src/clients/tools/definitions.rs`, lines 78-117
**Category**: Layering
**Fix**: Replaced hard-coded name comparisons (`spec.name == "LoadSkill" || spec.name == "WebSearch"`, `t.name == "LoadSkill"`) with `spec.chat_only` flag lookups. The filter now finds the original ToolSpec by name to check `chat_only`, keeping the flag the single source of truth. Removed import of `chat_only_always_available`.

## 8. `crates/gremlins/src/clients/tools/schema.rs`, lines 60-62, 126-128, 149-153
**Category**: Readability
**Fix**: Restored explanatory comments lost during the move from `tool_specs.rs`: the "file_path nested hint" rationale, the "'replacement' is removed — reject it explicitly" rationale, and the "start_line/end_line clamping semantics" documentation.

## 9. `crates/gremlins/src/clients/tools/tests.rs`, line 828
**Category**: Correctness (test update)
**Fix**: Updated `tool_definitions_filter` test: the headless tool count changed from 10 to 9 because `Mischief` (now `chat_only: true` on ToolSpec) is now consistently excluded from headless `tool_definitions`, matching the dispatch-time exclusion. Added a comment documenting the 9-tool count.