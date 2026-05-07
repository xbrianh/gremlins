# `gremlins/clients/`

Agent backends behind the `ClaudeClient` Protocol. Stages talk to one of
these via `client.run(...)` and never spawn `claude -p` (or `copilot -p`)
directly — the protocol is the seam tests swap out.

## Modules

- `protocol.py` — `ClaudeClient` Protocol and the `CompletedRun` dataclass.
  The single contract every backend implements; stages depend on this, not
  on a concrete class.
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
- `stream.py` — `stream_events` reader plus the `STREAM_IDLE_TIMEOUT`
  constant. Parses the `--output-format stream-json` line stream into the
  formatted log lines stages emit (`text:` / `think:` / `tool:` / `result:`
  / `final:`). Used by both subprocess clients.
- `resolve.py` — `ClientSpec` (`provider:model`), the package default
  (`claude:sonnet`), and the helpers (`collect_stage_specs`,
  `resolve_stage_client`, `require_stage_spec`,
  `load_stage_specs_from_state`, `validate_stage_specs`) that decide which
  client each stage gets and persist that decision to `state.json`.
- `__init__.py` — registers the `claude` and `copilot` factories with
  `gremlins.stages.registry.CLIENT_FACTORIES` at import time and exposes
  `to_client(spec)` for the orchestrator. Importing the package is what
  wires the providers up.

## Conventions

- New backends implement the `ClaudeClient` Protocol from `protocol.py` and
  register a factory via `register_client_factory(provider, factory)` in
  this package's `__init__.py`. The factory takes a model string (or
  `None`) and returns a client instance.
- The `label=` kwarg on `run(...)` is the stream-event prefix in logs and
  the `FakeClaudeClient` lookup key. Stages that re-enter the same logical
  step within one process must use distinct labels per phase so the fake's
  lookup doesn't collide.
- Subprocess clients track their live children under a lock and expose
  `reap_all()` for shutdown. New subprocess-based backends should follow
  the same pattern so SIGINT/SIGTERM cleanup stays uniform.
- Never spawn the underlying CLI directly from a stage — go through
  `client.run(...)` so tests can substitute `FakeClaudeClient`.

## Load-bearing invariants

- `STREAM_IDLE_TIMEOUT` in `stream.py` bounds how long a subprocess client
  will wait between stream events before raising `StreamTimeoutError`. The
  retry loop in `claude.py` depends on this — don't remove it without
  updating the retry logic.
- `ClientSpec.parse` enforces `provider:model` shape and rejects unknown
  providers by consulting `CLIENT_FACTORIES`. Adding a provider means
  registering it in `__init__.py`; otherwise YAMLs that name it fail at
  parse time, which is the desired behavior.
