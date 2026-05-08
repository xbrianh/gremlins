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
