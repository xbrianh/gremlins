# clients/ — client backends and the agent loop

A definition names its model with a single string using the URI grammar:
`model://provider/model[?key=value,...]`, or the scheme-omitted stub
`provider/model[?key=value,...]`. This module owns that grammar, the lazy
construction of backends from it, the shared tool-calling agent loop used by
every native (rig-based) backend, and the concrete backend implementations
themselves (`backends/`).

## File map

| File | Role |
|---|---|
| `mod.rs` | Declares the module structure — `pub mod backend`, `pub mod client`, `pub mod protocol`, etc. The rest of the crate imports directly from submodules (e.g. `clients::backend::Backend`, `clients::client::Client`). |
| `client.rs` | Owns the `model://provider/model[?key=value,...]` / `provider/model[?key=value,...]` grammar (`ModelURI::parse`), the provider allowlist (`is_known_provider`), and `Client` — the runtime handle that turns a `ModelURI` into a live `Backend` lazily and memoises it (`get_or_build_backend`). |
| `backend.rs` | The `Backend` trait every provider implements, plus `RunParams` (everything a run needs: prompt, cwd, timeouts, retries, cancellation, interactive session, task-model overrides, …) and `ClientError`. |
| `protocol.rs` | `CompletedRun` (exit code, text result, events, cost, token usage, message history) and `UsageStats` — the shared result shape every backend returns. |
| `agent_loop.rs` | `run_agent_loop` — the shared tool-calling loop used by every native agent backend (OpenAI, OpenRouter, xAI, Copilot, Anthropic, Azure OpenAI), plus `run_agent_loop_nested` for the `Task` tool. Streams model output, dispatches tool calls, classifies errors for retry (`ErrorClassifier`), and exposes `CancelToken` for `gremlins stop`. Reads its `turn-retry` and `timeout-retry` backoff schedules from `settings.yaml` via `crate::config`. |
| `openai_protocol.rs` | Shared state and run/reap logic (`OpenAiRunState`, `run_openai_compat`, `reap_openai_compat`) for backends that speak the OpenAI completions protocol. |
| `tools.rs` | Implements the seven native tools (`Bash`, `Edit`, `Read`, `Write`, `Grep`, `Glob`, `Landscape`) plus `Task`/`Done`, with sandboxing (`allowed_roots`) and the `AgentLoopRuntime` passed through the agent loop. |
| `interactive.rs` | The channel triplet connecting the supervisor to a running agent loop: `InteractiveHandle`/`InteractiveSession`, plus `PauseToken` for operator pause/resume. `StreamEventSender` wraps the two stream paths — `Broadcast` for debug/interactive (lag-tolerant) and `Mpsc` for chat (bounded, backpressured, lossless). |
| `stream_json.rs` | Parses `stream-json` event lines (used by `cmd` backends) and extracts `StreamState` (cost, result text, error status). |
| `log_util.rs` | `trunc` — single-purpose string truncation helper for log lines. |
| `anthropic_bearer_http.rs` | `BearerHttpClient` — eager HTTP client wrapper that rewrites `x-api-key` → `Authorization: Bearer` for Azure's Anthropic-compatible API (cognitive services with Entra ID tokens). Used by `backends/anthropic.rs` for the per-attempt dynamic-auth path; the lazy variant used by `make_model` lives in `lazy_auth_http.rs`. |
| `azure_openai_responses_http.rs` | `AzureResponsesHttpClient` — removable HTTP shim that rewrites Azure OpenAI Responses requests (`/responses`) into the account-level v1 form (`{endpoint}/openai/v1/responses?api-version=…`) while preserving the top-level `model` body field. Used by `backends/azure_openai.rs`; delete when rig's Responses wire learns Azure v1 routing. |
| `lazy_auth_http.rs` | Lazy-auth HTTP client wrappers for dynamic-auth backends. `LazyBearerHttpClient` (Anthropic) and `LazyApiKeyHttpClient` (Azure OpenAI) hold a `TokenProvider` + scope and acquire a token on the first outgoing request, caching it for the model's lifetime. |
| `token_provider.rs` | `TokenProvider` trait — custom abstraction over Azure identity credential types (client secret, CLI, managed identity, chained default), with `TokenProviderError`. |
| `config.rs` | `agent_system_prompt` / `task_system_prompt` — the system prompts injected into top-level and nested (Task) agent invocations. Also owns `ProviderAuth`, `Providers`, and the `providers.yaml` loading/credential-resolution helpers (`api_key`, `base_url`, `auth_token`, `endpoint`, `api_version`, `azure_auth_method`, `azure_auth_scope`). |
| `task.rs` | `TaskModelSelector`/`TaskModelFactory` — resolves `task-models` overrides (exact `TaskKind` lookup) so a `Task` tool call can run under a different model than its parent. |
| `backends/mod.rs` | The `backends!` macro: declares each backend's module and generates `registry()`, mapping provider name → `BuildFn`. Adding a provider is one line here. |
| `backends/openai.rs` | `OpenAiBackend` — wraps `OpenAiRunState` for `api.openai.com`, default model `gpt-4o`. |
| `backends/openrouter.rs` | `OpenRouterBackend` — wraps `OpenAiRunState` for `openrouter.ai`, with its own transient-error classifier for retry. |
| `backends/copilot.rs` | `CopilotBackend` — GitHub Copilot, with credential resolution from env vars (`GITHUB_COPILOT_API_KEY`, `COPILOT_API_KEY`) and `providers.yaml`. Uses its own `CopilotRunState` (not `OpenAiRunState`) and calls `run_agent_loop` directly. |
| `backends/xai.rs` | `XaiBackend` — wraps `OpenAiRunState` for `api.x.ai`, default model `grok-4`. |
| `backends/anthropic.rs` | `AnthropicBackend` — native Anthropic backend using rig's `Anthropic` provider with its own agent loop. Default model `claude-sonnet-4-6`. Supports direct API key/token auth, plus Azure Entra ID identity auth (client secret, CLI, managed identity, default chain); dynamic auth uses `anthropic_bearer_http.rs` per attempt and `lazy_auth_http.rs` for `make_model`. |
| `backends/azure_openai.rs` | `AzureOpenAiBackend` — Azure OpenAI backend using rig's `OpenAI` provider with `AZURE` wire format and the Responses route (`POST /responses`). Has its own `AzureOpenAiRunState` (not `OpenAiRunState`) and calls `run_agent_loop` directly. Credentials come from env vars / `providers.yaml` (`azure-openai.*`); supports API key/token plus Entra ID identity auth via `token_provider.rs`, with `lazy_auth_http.rs` for `make_model` and `azure_openai_responses_http.rs` for Azure v1 routing. |
| `backends/cmd.rs` | `CmdBackend` — drives an arbitrary external CLI (e.g. Claude Code) as a subprocess, parsing its `stream-json` output and tracking per-gremlin retry context. |

## Architecture

**Spec grammar.** `client.rs::ModelURI::parse` splits a model reference into
`(provider, model, extra_params)`. Both the canonical `model://provider/model`
form and the scheme-omitted stub `provider/model` form are accepted; `?` is the
only params introducer, so the portion after `?` is parsed with
`parse_params_list` and returned as `extra_params`. A `cmd` backend's "model"
is a shell command, so everything after `cmd/` belongs to it verbatim and
`cmd` never has params (`model://cmd/...` is rejected). Colons are no longer
structural: everything between the first path separator and the first `?` is
model content, so `openrouter/some/model:free` names the model
`some/model:free`. The provider name `profile` is reserved for load-time
model-profile references (`profile/<name>`): `ModelURI::parse` rejects both
`profile/...` and `model://profile/...` with a reserved-profile-reference-prefix
error, so a profile reference can never be misparsed as a literal
`provider/model` spec (profile resolution itself lives in `config.rs`, not
here). `is_known_provider` just checks membership in
`backends::registry()`; `Client::new`/`Client::from_uri` keep the allowlist
enforcement separate from the shape-only `ModelURI::parse`.

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
| Understand the spec grammar | `client.rs::ModelURI::parse`, `is_known_provider` |
| Change how backends are cached/built | `client.rs::Client::get_or_build_backend` |
| Add or change a native tool | `tools.rs` |
| Change retry/backoff behavior | `crates/gremlins/src/config.rs` (`backoff:` schedules in `settings.yaml`) and `agent_loop.rs` (retry semantics), or a backend's `ErrorClassifier` |
| Change the system prompt | `config.rs` |
| Understand `task-models` overrides | `task.rs::TaskModelSelector` |
| Understand supervisor pause/cancel/resume | `interactive.rs`, `agent_loop.rs::CancelToken` |
| Add a field to a run's result | `protocol.rs::CompletedRun` / `UsageStats` |
