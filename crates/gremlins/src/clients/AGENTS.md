# clients/ — client backends and the agent loop

A definition names its model with a single string, `provider:model[:key=value,...]`.
This module owns that grammar, the lazy construction of backends from it, the
shared tool-calling agent loop used by every native (rig-based) backend, and
the concrete backend implementations themselves (`backends/`).

## File map

| File | Role |
|---|---|
| `client.rs` | Owns the `provider:model[:key=value,...]` grammar (`parse_spec`), the provider allowlist (`is_known_provider`), and `Client` — the type that turns a spec into a live `Backend` lazily and memoises it (`get_or_build_backend`). |
| `backend.rs` | The `Backend` trait every provider implements, plus `RunParams` (everything a run needs: prompt, cwd, timeouts, retries, cancellation, interactive session, task-client overrides, …) and `ClientError`. |
| `protocol.rs` | `CompletedRun` (exit code, text result, events, cost, token usage) and `UsageStats` — the shared result shape every backend returns. |
| `agent_loop.rs` | `run_agent_loop` — the shared tool-calling loop used by every native agent backend (OpenAI, OpenRouter, xAI, Copilot). Streams model output, dispatches tool calls, classifies errors for retry (`ErrorClassifier`), and exposes `CancelToken` for `gremlins stop`. |
| `openai_protocol.rs` | Shared state and run/reap logic (`OpenAiRunState`, `run_openai_compat`, `reap_openai_compat`) for backends that speak the OpenAI completions protocol. |
| `retry.rs` | `with_retry` backoff helper and the shared `STREAM_IDLE_BACKOFF` schedule, used by both native and `cmd` backends. |
| `tools.rs` | Implements the six native tools (`Bash`, `Edit`, `Read`, `Write`, `Grep`, `Glob`) plus `Task`/`Done`, with sandboxing (`allowed_roots`) and the `ToolContext` passed through the agent loop. |
| `interactive.rs` | The channel triplet connecting the supervisor to a running agent loop: `InteractiveHandle`/`InteractiveSession`, plus `PauseToken` for operator pause/resume. |
| `stream_json.rs` | Parses `stream-json` event lines (used by `cmd` backends) and extracts `StreamState` (cost, result text, error status). |
| `log_util.rs` | `trunc` — single-purpose string truncation helper for log lines. |
| `config.rs` | `agent_system_prompt` / `task_system_prompt` — the system prompts injected into top-level and nested (Task) agent invocations. |
| `task.rs` | `TaskModelSelector`/`TaskModelFactory` — resolves `task-clients` overrides (exact + prefix match) so a `Task` tool call can run under a different model than its parent. |
| `backends/mod.rs` | The `backends!` macro: declares each backend's module and generates `registry()`, mapping provider name → `BuildFn`. Adding a provider is one line here. |
| `backends/openai.rs` | `OpenAiBackend` — wraps `OpenAiRunState` for `api.openai.com`, default model `gpt-4o`. |
| `backends/openrouter.rs` | `OpenRouterBackend` — wraps `OpenAiRunState` for `openrouter.ai`, with its own transient-error classifier for retry. |
| `backends/copilot.rs` | `CopilotBackend` — GitHub Copilot, with several credential sources (env vars, `providers.json`, auto-discovered `apps.json`). |
| `backends/xai.rs` | `XaiBackend` — wraps `OpenAiRunState` for `api.x.ai`, default model `grok-4`. |
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
`openai::CompletionsClient`, model name, tool filter, client params,
in-flight cancel tokens, and the next request id. `backends/openai.rs` and
`backends/openrouter.rs` (and `backends/xai.rs`) each just construct an
`OpenAiRunState` and delegate `run`/`reap` to `run_openai_compat` /
`reap_openai_compat`, passing their own `ErrorClassifier` and provider name —
OpenRouter's classifier additionally retries on a list of transient-error
substrings (capacity, rate limits, gateway errors, etc.) that OpenAI's
default classifier doesn't need to special-case.

**The agent loop.** `agent_loop.rs::run_agent_loop` is the single
tool-calling loop shared by every native backend: it streams the model's
response, executes any tool calls via `tools.rs`, feeds results back, and
repeats until the model calls `Done` or the loop is cancelled. `CmdBackend`
does not use this loop — it shells out to an external agent CLI and parses
its own JSON event stream instead (`stream_json.rs`).

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new client provider | `backends/mod.rs` (register in the `backends!` macro), then a new `backends/<name>.rs` implementing `Backend`. If it's OpenAI-protocol-compatible, wrap `OpenAiRunState` like `backends/openai.rs`/`backends/xai.rs` rather than reimplementing the run loop. |
| Understand the spec grammar | `client.rs::parse_spec`, `is_known_provider` |
| Change how backends are cached/built | `client.rs::Client::get_or_build_backend` |
| Add or change a native tool | `tools.rs` |
| Change retry/backoff behavior | `retry.rs` (`with_retry`, `STREAM_IDLE_BACKOFF`), or a backend's `ErrorClassifier` |
| Change the system prompt | `config.rs` |
| Understand `task-clients` overrides | `task.rs::TaskModelSelector` |
| Understand supervisor pause/cancel/resume | `interactive.rs`, `agent_loop.rs::CancelToken` |
| Add a field to a run's result | `protocol.rs::CompletedRun` / `UsageStats` |
