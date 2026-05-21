# `gremlins/clients/providers/`

Vendor-SDK backends. Every file here may import from third-party agent
frameworks (`agents`, `openai`, etc.). Nothing outside this subpackage
imports from it directly — callers go through `gremlins.clients.__init__`
and the factory registered there.

## Boundary contract

- **Import into**: only `gremlins/clients/__init__.py` (to register the
  factory).
- **Import from**: anything inside this subpackage plus
  `gremlins.clients.tools` (shared tool definitions) and the standard
  library.
- **Never import from**: `gremlins.stages`, `gremlins.pipeline`, or any
  other gremlins module not listed above — this keeps the provider slice
  independently testable.

Verify the boundary with:

```
grep -rn "from gremlins.clients.providers" gremlins/ | grep -v "clients/__init__.py"
```

No output means the boundary is intact.

## Modules

- `openai_agents.py` — `OpenAIAgentsClient`, a `ClaudeClient`-protocol
  implementation that drives prompts through the `openai-agents` SDK.
  Uses `GREMLINS_TOOLS` from `gremlins.clients.tools` as the tool list.
- `anthropic_sdk.py` — `AnthropicSdkClient`, a `ClaudeClient`-protocol
  implementation that drives prompts through the `claude-agent-sdk` Python
  package. **Auth**: requires `ANTHROPIC_API_KEY` in the environment; raises
  `RuntimeError` at construction time if the key is absent. **Hermetic**: all
  `CLAUDE_*` vars and all `ANTHROPIC_*` vars except `ANTHROPIC_API_KEY` are
  stripped from the child environment, and `setting_sources=[]` / no MCP /
  no hooks are passed to the SDK so no local operator config leaks in.
  **Cost**: extracts `input_tokens` + `output_tokens` from the SDK's
  `ResultMessage` and prices them against a module-level `_PRICING` table
  (Sonnet/Opus/Haiku IDs); unknown model IDs fall back to Sonnet rates.
  **Retry**: applies the same `STREAM_IDLE_TIMEOUT`, `STREAM_IDLE_BACKOFF`,
  and `is_transient_stream_error` classifier used by `openai_agents.py`.
  Transient SDK errors (rate limit, overloaded, etc.) are retried up to
  `max_retries` times; permanent errors propagate immediately.
