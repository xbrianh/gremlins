# clients/ — client backends and the agent loop

A definition names its model with a single string, `provider:model[:key=value,...]`.
This module owns that grammar, the lazy construction of backends from it, the
shared tool-calling agent loop used by every native (rig-based) backend, and
the concrete backend implementations themselves (`backends/`).

## File map

| File | Role |
|---|---|
| `mod.rs` | Declares the module structure — `pub mod backend`, `pub mod client`, `pub mod protocol`, etc. The rest of the crate imports directly from submodules (e.g. `clients::backend::Backend`, `clients::client::Client`). |
| `client.rs` | Owns the `provider:model[:key=value,...]` grammar (`parse_spec`), the provider allowlist (`is_known_provider`), and `Client` — the type that turns a spec into a live `Backend` lazily and memoises it (`get_or_build_backend`). |
| `backend.rs` | The `Backend` trait every provider implements, plus `RunParams` (everything a run needs: prompt, cwd, timeouts, retries, cancellation, interactive session, task-client overrides, …) and `ClientError`. |
| `protocol.rs` | `CompletedRun` (exit code, text result, events, cost, token usage) and `UsageStats` — the shared result shape every backend returns. |
| `agent_loop.rs` | `run_agent_loop` — the shared tool-calling loop used by every native agent backend (OpenAI, OpenRouter, xAI, Copilot, Anthropic, Azure OpenAI). Streams model output, dispatches tool calls, classifies errors for retry (`ErrorClassifier`), and exposes `CancelToken` for `gremlins stop`. |
| `openai_protocol.rs` | Shared state and run/reap logic (`OpenAiRunState`, `run_openai_compat`, `reap_openai_compat`) for backends that speak the OpenAI completions protocol. |
| `retry.rs` | `with_retry` backoff helper and the shared `STREAM_IDLE_BACKOFF` schedule, used by both native and `cmd` backends. |
| `tools.rs` | Implements the six native tools (`Bash`, `Edit`, `Read`, `Write`, `Grep`, `Glob`) plus `Task`/`Done`, with sandboxing (`allowed_roots`) and the `ToolContext` passed through the agent loop. |
| `interactive.rs` | The channel triplet connecting the supervisor to a running agent loop: `InteractiveHandle`/`InteractiveSession`, plus `PauseToken` for operator pause/resume. |
| `stream_json.rs` | Parses `stream-json` event lines (used by `cmd` backends) and extracts `StreamState` (cost, result text, error status). |
| `log_util.rs` | `trunc` — single-purpose string truncation helper for log lines. |
| `anthropic_bearer_http.rs` | HTTP client wrapper that rewrites `x-api-key` → `Authorization: Bearer` for Azure's Anthropic-compatible API (cognitive services with Entra ID tokens). Self-contained; delete when rig adds native bearer support. |
| `token_provider.rs` | `TokenProvider` trait — custom abstraction over Azure identity credential types (client secret, CLI, managed identity, chained default), with `TokenProviderError`. |
| `config.rs` | `agent_system_prompt` / `task_system_prompt` — the system prompts injected into top-level and nested (Task) agent invocations. Also owns `ProviderAuth`, `Providers`, and the `providers.yaml` loading/credential-resolution helpers (`api_key`, `base_url`, `auth_token`, `azure_auth_method`, `azure_auth_scope`). |
| `task.rs` | `TaskModelSelector`/`TaskModelFactory` — resolves `task-clients` overrides (exact + prefix match) so a `Task` tool call can run under a different model than its parent. |
| `backends/mod.rs` | The `backends!` macro: declares each backend's module and generates `registry()`, mapping provider name → `BuildFn`. Adding a provider is one line here. |
| `backends/openai.rs` | `OpenAiBackend` — wraps `OpenAiRunState` for `api.openai.com`, default model `gpt-4o`. |
| `backends/openrouter.rs` | `OpenRouterBackend` — wraps `OpenAiRunState` for `openrouter.ai`, with its own transient-error classifier for retry. |
| `backends/copilot.rs` | `CopilotBackend` — GitHub Copilot, with credential resolution from env vars (`GITHUB_COPILOT_API_KEY`, `COPILOT_API_KEY`) and `providers.yaml`. Uses its own `CopilotRunState` (not `OpenAiRunState`) and calls `run_agent_loop` directly. |
| `backends/xai.rs` | `XaiBackend` — wraps `OpenAiRunState` for `api.x.ai`, default model `grok-4`. |
| `backends/anthropic.rs` | `AnthropicBackend` — native Anthropic backend using rig's `Anthropic` provider with its own agent loop. Supports both direct API key and Azure Entra ID bearer-token auth via `anthropic_bearer_http.rs`. |
| `backends/azure_openai.rs` | `AzureOpenAiBackend` — Azure OpenAI backend using rig's `OpenAI` provider with `AZURE` wire format. Has its own `AzureOpenAiRunState` (not `OpenAiRunState`) and calls `run_agent_loop` directly. Supports Entra ID token credentials via `token_provider.rs`. |
| `backends/cmd.rs` | `CmdBackend` — drives an arbitrary external CLI (e.g. Claude Code) as a subprocess, parsing its `stream-json` output and tracking per-gremlin retry context. |

## Architecture

**Spec grammar.** `client.rs::parse_spec` splits `provider:model[:key=value,...]`
into `(provider, model, extra_params)`. The trailing parameter list is only
recognised for non-`cmd` providers — a `cmd` backend's "model" is a shell
command, so anything after the first colon belongs to it verbatim. For other
providers, a colon suffix that doesn't parse as `key=value,...` is kept as
part of the model (OpenRouter routes on suffixes like `:free`, `:online`), so
`openrouter:some/model:free` names the model `some/model:free`.
`is_known_provider` just checks membership in `backends::registry()`.

**Lazy, memoised construction.** `Client::get_or_build_backend` builds the
`Arc<dyn Backend>` for a spec on first use and caches it behind a
`OnceLock`/`Mutex`. This means parsing and validating a pipeline definition
never touches credentials or the network — only the process that actually
runs a stage pays the cost of resolving an API key, and repeated runs reuse
the same backend instance.

**Shared OpenAI-protocol state.** `openai_protocol.rs::OpenAiRunState` holds
everything duplicated between OpenAI-compatible backends: the
`openai::OpenAI`, model name, tool filter, client params,
in-flight cancel tokens, and the next request id. `backends/openai.rs`,
`backends/openrouter.rs`, and `backends/xai.rs`
each just construct an `OpenAiRunState` and delegate `run`/`reap` to
`run_openai_compat` / `reap_openai_compat`. OpenAI and xAI call
`run_openai_compat` with no custom classifier (`None`), relying on its
default retry behavior; only OpenRouter passes its own `ErrorClassifier`,
which additionally retries on a list of transient-error substrings
(capacity, rate limits, gateway errors, etc.).

**Native (non-OpenAI-protocol) backends.** `backends/anthropic.rs`,
`backends/azure_openai.rs`, `backends/copilot.rs`, and
`backends/cmd.rs` do not use `OpenAiRunState`. Anthropic uses rig's native
`Anthropic` provider and runs its own agent loop via `run_agent_loop`.
Azure OpenAI uses rig's `OpenAI` provider with the `AZURE` wire format
but has its own `AzureOpenAiRunState` and also calls `run_agent_loop`
directly (it cannot reuse `OpenAiRunState` because it needs dynamic
per-attempt token acquisition for Entra ID auth).
Copilot uses rig's native `Copilot` provider with its own `CopilotRunState`
and also calls `run_agent_loop` directly.
`CmdBackend` shells out to an external agent CLI and parses its own JSON
event stream (`stream_json.rs`).

**The agent loop.** `agent_loop.rs::run_agent_loop` is the single
tool-calling loop shared by every native backend: it streams the model's
response, executes any tool calls via `tools.rs`, feeds results back, and
repeats until the model calls `Done` or the loop is cancelled. `CmdBackend`
does not use this loop — it shells out to an external agent CLI and parses
its own JSON event stream instead (`stream_json.rs`).

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new client provider | `backends/mod.rs` (register in the `backends!` macro), then a new `backends/<name>.rs` implementing `Backend`. If it's OpenAI-protocol-compatible, wrap `OpenAiRunState` like `backends/openai.rs`/`backends/xai.rs`. If it's a native rig provider (like Anthropic, Copilot, or Azure OpenAI), use `run_agent_loop` directly. |
| Understand the spec grammar | `client.rs::parse_spec`, `is_known_provider` |
| Change how backends are cached/built | `client.rs::Client::get_or_build_backend` |
| Add or change a native tool | `tools.rs` |
| Change retry/backoff behavior | `retry.rs` (`with_retry`, `STREAM_IDLE_BACKOFF`), or a backend's `ErrorClassifier` |
| Change the system prompt | `config.rs` |
| Understand `task-clients` overrides | `task.rs::TaskModelSelector` |
| Understand supervisor pause/cancel/resume | `interactive.rs`, `agent_loop.rs::CancelToken` |
| Add a field to a run's result | `protocol.rs::CompletedRun` / `UsageStats` |
