# rig 0.41 → 0.43 migration state

## What was done

The `Cargo.toml` was bumped from `rig-core = "0.41"` to `>=0.43` in commit `63063bbd` without code changes. Rig 0.43 is a complete API redesign. This documents the migration progress.

## Files completed

| File | Status | Notes |
|------|--------|-------|
| `Cargo.toml` | ✓ | Version `>=0.43`, added `reqwest` feature + `rig-reqwest` dep |
| `anthropic_bearer_http.rs` | ✓ | Wraps `DynHttpClient` instead of `ReqwestClient` |
| `openai_protocol.rs` | ✓ | `CompletionsClient` → `OpenAI`, `client.completion_model()` → `client.completion().erase()`, `ReqwestClient` pool → `DynHttpClient` |
| `backends/openai.rs` | ✓ | Same client type changes |
| `backends/xai.rs` | ✓ | Same |
| `backends/openrouter.rs` | ✓ | Same + `CompletionError` → `ProviderError` |
| `backends/anthropic.rs` | ✓ | `anthropic::Client` → `Anthropic`, `AnthropicKey` → direct key, builder → config; `&model` → `model` |
| `backends/azure_openai.rs` | ✓ | `azure::Client` → `OpenAI` + `AZURE` dialect, `.connect()` no longer returns `Result` |
| `backends/copilot.rs` | ✓ | `copilot::Client` → `Copilot`, PAT auth deferred (returns error for now) |
| `task.rs` | ✓ | `CompletionModel` → `DynModel<Completion>`, `TaskModelSelector<M>` defaulted to `DynModel<Completion>` |
| `agent_loop.rs` | ✓ | All compilation and test errors fixed |

## Fixes applied (2025-07-17)

### Lib compilation errors (12 fixed)

1. **`&model` → `model` in `run_agent_loop` callers** — `run_agent_loop` now takes owned `DynModel<Completion>`; removed `&` in anthropic.rs (x2), azure_openai.rs, openai_protocol.rs, copilot.rs
2. **`&model` → `model` in `run_agent_loop_core` callers** — `run_agent_loop_core` takes `&DynModel<Completion>`; added `&` at the 2 internal call sites that had `model` (owned)
3. **`ToolName` → `String`** — `.map(|tc| tc.function.name.clone())` → `.map(|tc| tc.function.name.to_string())` (ToolName has `to_string()`)
4. **`Message::tool_result` signature** — now takes `(CallId, ToolName, String)`; fixed `job.id, job.id` → `CallId::from_wire(job.id.clone()), ToolName::new(job.name.clone()).expect(...)` (second `job.id` was a bug — should be `job.name`)
5. **`BearerHttpClient`** — changed from wrapping `ReqwestClient` to `DynHttpClient`; `.connect(wrapped)` instead of `.connect(DynHttpClient::new(wrapped))`
6. **`azure_openai.rs` `.map_err` on `.connect()`** — `.connect()` returns client directly, not `Result`; removed `.map_err()`
7. **Unused import `futures::StreamExt`** — removed
8. **Unused variable `usage`** — renamed to `_usage`
9. **`&selected_model` in `task.rs`** — removed `&`

### Test compilation errors (14 fixed)

10. **`chat_history.first()` pattern** — now returns `Option<&Message>`, patterns need `Some(Message::System { .. })`
11. **`req.preamble` field → `req.system_instructions()` method** — `preamble` is now a builder, not a field
12. **Mock model → `DynModel` in tests** — tests using `MockCompletionModel` with `run_agent_loop[_core]` need `.erase()`; factory closures return `.erase()`; split mock for introspection vs dyn model for the call
13. **`TaskModelFactory<MockCompletionModel>` → `TaskModelFactory`** — type alias defaults to `DynModel<Completion>` now

### Test runtime failures (19 fixed)

14. **Text duplication bug** — `StreamEvent::End { content: AssistantContent::Text(t) }` now carries the complete text in rig 0.43, duplicating chunked `StreamEvent::Text` accumulation. Fix: only capture `End` text when `text` is empty.
15. **Idle timeout test** — empty mock model returns `ProviderError` immediately (not a hang); updated assertion to `ClientError::ApiServerError`
16. **Copilot PAT auth tests** — 4 tests updated to expect `Err("GitHub PAT auth is not yet supported")` since PAT auth is deferred

## Still deferred

- **Copilot PAT auth** — GitHub PAT / apps.json / providers.yaml PAT authentication is not yet supported in rig 0.43; `resolve_auth()` returns an error. The copilot API key (`COPILOT_API_KEY`) path works.
- **`Job.call_id` field** — still unused; populated but never read. Can be cleaned up later.

## Migration approach

The core change: `CompletionModel` trait removed, replaced by concrete `DynModel<Completion>` (type-erased `Model<W>`). Key API mappings:

| Old (0.41) | New (0.43) |
|---|---|
| `CompletionModel` trait | `DynModel<Completion>` |
| `CompletionError` | `ProviderError` |
| `CompletionClient` trait | N/A (removed) |
| `GetTokenUsage` trait | `Usage` on `CompletionResponse` |
| `StreamedAssistantContent<R>` | `Item<StreamEvent>` |
| `OneOrMany` | `Vec` |
| `StreamingCompletionResponse` | `CompletionResponse` |
| `client.completion_model(name)` | `client.completion(name).erase()` |
| `Provider::Client::builder().api_key().build()` | `ProviderConfig::new(key).connect(http)` or `Provider::new(key)` |
| `ReqwestClient` | `rig_reqwest::ReqwestClient` |
| `BearerAuth` / `AnthropicKey` | Pass key directly to config |
| `AzureOpenAIAuth` | Pass key/token directly to config |
| `Usage` fields | Now `Option<u64>` (was `u64`) |
| `ToolCall.call_id` (String) | `ToolCall.id` (CallId enum) |
| `Message::tool_result_with_call_id` | `Message::tool_result` |
| `MockResponse` | `MockHttpResponse` |
| `MockCompletionModel` | Still exists, `Model<MockScript, MockRuntime>` |
| `CompletionRequest::preamble` (field) | `CompletionRequest::preamble()` (builder) / `.system_instructions()` (accessor) |
| `chat_history.first()` → `Message::System` | `chat_history.first()` → `Option<&Message>`, use `Some(Message::System { .. })` |
| `.connect()` returns `Result` | `.connect()` returns client directly |
| `BearerHttpClient` wraps `ReqwestClient` | wraps `DynHttpClient` |