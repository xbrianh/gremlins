# Claude Config Personal Setup

Phases 0–3 have landed. The sync script, skill structure, and initial gremlins package are all in place. The fleet manager and boss gremlin are merged and working.

## Context

The remaining work is to add the sanitize pass and test coverage for the handoff agent's rolling plan output.

## Approach

Add `build_sanitize_prompt` and `sanitize_rolling_plan` to `gremlins/handoff.py`. After the main agent writes the rolling plan, run a second `claude -p --model haiku` pass that enforces format rules mechanically and overwrites the file in-place. Failure is non-fatal.

## Tasks

- [ ] Task 5: Add `build_sanitize_prompt` and `sanitize_rolling_plan` helpers
- [ ] Task 6: Call `sanitize_rolling_plan` from `main()` for all exit states
- [ ] Task 7: Add tests and fixture files

## Open questions

(none)
