# `gremlins/clients/`

Agent backends behind the `Client` protocol. Stages talk to one of these via
`client.run(...)` and never spawn `claude -p` (or `copilot -p`) directly —
the `Client` class is the seam tests swap out.

## Modules

- `protocol.py` — `CompletedRun` dataclass. The return type for all backend
  `run(...)` calls.
- `client.py` — `Client` class: parses `provider:model` specifiers
  (`Client.parse`), dispatches to the registered factory, and provides a sync
  `run(...)` wrapper around the backend impl. Also defines `PACKAGE_DEFAULT`
  (`claude:sonnet`).
- `claude.py` — `SubprocessClaudeClient`, the production backend. Spawns
  `claude -p ... --output-format stream-json` and consumes events via
  `stream.stream_events`. Owns its child list so `reap_all()` (called from
  the runner's signal handlers) can terminate every concurrent subprocess
  before the orchestrator exits.
- `copilot.py` — `SubprocessCopilotClient`. Same protocol, delegates to
  `copilot -p`. Selectable per-stage via pipeline YAML `provider: copilot`.
  Strips Copilot's `⏺ Cost: …` footer so `text_result` contains only the
  response.
- `fake.py` — `FakeClaudeClient`, the recording test double. Looks up
  scripted responses by `label=` passed into `client.run(...)`.
- `config.py` — cross-backend retry/timeout constants (`STREAM_IDLE_TIMEOUT`,
  `STREAM_IDLE_BACKOFF`) and `validate_max_retries`. The single source of
  truth for retry policy; both `claude.py` and `providers/openai_agents.py`
  import from here.
- `stream.py` — `stream_events` reader and `trunc` helper. Parses the
  `--output-format stream-json` line stream into the formatted log lines
  stages emit (`text:` / `think:` / `tool:` / `result:` / `final:`).
  `stream_events` is used by `claude.py` and `fleet/rescue.py`; `trunc`
  is used by `providers/openai_agents.py`.
- `__init__.py` — registers the `claude`, `copilot`, `openai`, `xai`, and
  `anthropic` factories with `CLIENT_FACTORIES` at import time. Importing the
  package is what wires the providers up.
- `tools.py` — `GREMLINS_TOOLS`, the list of `openai-agents` `FunctionTool`
  objects (Read, Edit, Bash, Write, Grep, Glob) that back the OpenAI
  provider's agent loop.
- `providers/` — vendor-SDK backends. All `agents`/`openai` SDK imports live
  here; nothing outside `__init__.py` imports from this subpackage. See
  `providers/AGENTS.md` for the full boundary contract.

## Conventions

- New backends implement the duck-typed interface expected by `Client` in
  `client.py` (`run(...)`, `reap_all()`, `total_cost_usd`) and register a
  factory via `register_client_factory(provider, factory)` in this package's
  `__init__.py`. The factory takes a model string and returns a backend
  instance.
- Registered providers: `claude` → `SubprocessClaudeClient`; `copilot` →
  `SubprocessCopilotClient`; `openai` and `xai` → `OpenAIAgentsClient` in
  `providers/openai_agents.py` (both share the same backend, keyed by
  provider); `anthropic` → `AnthropicSdkClient` in
  `providers/anthropic_sdk.py`.
- The `label=` kwarg on `run(...)` is the stream-event prefix in logs and
  the `FakeClaudeClient` lookup key. Stages that re-enter the same logical
  step within one process must use distinct labels per phase so the fake's
  lookup doesn't collide.
- Subprocess clients track their live children under a lock and expose
  `reap_all()` for shutdown. New subprocess-based backends should follow
  the same pattern so SIGINT/SIGTERM cleanup stays uniform.
- Never spawn the underlying CLI directly from a stage — go through
  `client.run(...)` so tests can substitute `FakeClaudeClient`.

## `claude:` vs `anthropic:` — two backends, not migration phases

Both backends are fully supported and both stay; neither replaces the other.

| | `claude:` | `anthropic:` |
|---|---|---|
| **Mechanism** | Subprocess (`claude -p --output-format stream-json`) | Python SDK (`claude-agent-sdk`) |
| **Auth** | Claude subscription / operator config | `ANTHROPIC_API_KEY` environment variable |
| **Operator config** | Inherited (reads local Claude settings, MCP, hooks) | Hermetic — `setting_sources=[]`, no MCP, no hooks |
| **Cost reporting** | From SDK `result` event `total_cost_usd` | From token counts + pricing table; fallback to SDK `total_cost_usd` |
| **When to use** | Default; matches what an operator-configured Claude session would do | Isolated, API-key-driven runs where hermetic behavior is required |

Pipelines opt in to `anthropic:` by setting `default_client: anthropic:<model-id>` or a per-stage `client:` field.

### How `claude:` picks up config

The `claude:` backend is a thin wrapper around `claude -p`. It does *not*
materialize a per-gremlin config dir, and it does *not* set
`CLAUDE_CONFIG_DIR` for the subprocess. Whatever the operator has configured
for their interactive Claude session is exactly what the subprocess sees:

- **Settings** — `~/.claude/settings.json` (plus any project-level
  `.claude/settings.json` the CLI discovers) is read by the CLI directly.
  The gremlins-layer `allowed_tools` / `disallowed_tools` block has no
  effect on `claude:` runs; configure tool permissions via the user's own
  Claude settings or use the `anthropic:` backend.
- **MCP servers and hooks** — inherited from the user's Claude config.
- **Auth** — subscription auth follows `~/.claude/.credentials.json` (or the
  macOS keychain) exactly as it would for an interactive session.
- **Permission mode** — the only thing the wrapper still controls per call:
  `--permission-mode bypassPermissions` when `bypass=True`, otherwise
  `default`.

This is a deliberate simplification (#823): the prior hack symlinked the
user's credentials into a redirected `CLAUDE_CONFIG_DIR` so per-stage
settings could be injected, which depended on undocumented CLI behavior.

### True process isolation: use an SDK backend

If you need per-gremlin tool allow-lists, hermetic config, or a clean
separation between gremlins and the operator's interactive Claude session,
use one of the SDK-backed providers instead:

- `anthropic:<model-id>` — `claude-agent-sdk` with `setting_sources=[]` (no
  ambient settings, no MCP, no hooks). Requires `ANTHROPIC_API_KEY`.
  `allowed_tools` from the native block is enforced by the SDK.
- `openai:<model-id>` / `xai:<model-id>` — `openai-agents` SDK with the
  in-tree `GREMLINS_TOOLS` list. Per-gremlin `allowed_tools` filters that
  list. Requires `OPENAI_API_KEY` / `XAI_API_KEY`.

Set via pipeline YAML:

```yaml
default_client: anthropic:claude-sonnet-4-6
# or per-stage:
stages:
  - name: implement
    client: anthropic:claude-sonnet-4-6
```

Subscription auth is not available on the SDK backends — that is Anthropic
policy, not a gremlins limitation.

## Copilot permission surface

GitHub Copilot Agent CLI (`copilot -p`) exposes a thin permission surface:

| Scenario | Flags added |
|---|---|
| `bypass=True` | `--allow-all` (grants file-path + URL access beyond `--allow-all-tools`) |
| `bypass=False`, any `native_block` | *(none)* |

The `allowed_tools` list in `gremlins/permissions/defaults/copilot.yaml` names
gremlins-layer tools (Read, Edit, Bash, …). Copilot's CLI has no per-tool flags
that accept these names, so the block cannot be expressed as argv. The minimum
safe default for non-bypass runs is no extra flags. If Copilot's CLI grows a
per-tool allow-list flag in the future, translate `allowed_tools` there.

## Load-bearing invariants

- `STREAM_IDLE_TIMEOUT` and `STREAM_IDLE_BACKOFF` in `config.py` are the
  single source of truth for retry/timeout policy across all backends. Both
  `claude.py` and `providers/openai_agents.py` import and use
  `validate_max_retries` from there; overrun semantics must stay uniform.
- `Client.parse` enforces `provider:model` shape and rejects unknown providers
  by consulting `CLIENT_FACTORIES`. Adding a provider means registering it in
  `__init__.py`; otherwise specifiers that name it fail at parse time, which
  is the desired behavior.
