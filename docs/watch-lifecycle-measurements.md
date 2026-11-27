# Watch lifecycle simplification

Both measurements ran from the checkout root with the same configuration and command:

```sh
gremlins-drag crates/gremlins/src --model chatgpt/gpt-6-luna
```

No drag configuration changed between runs. The before measurement used the original production watch path.

| Measurement | Attachment tokens | Footprint | Comprehensibility | Module drag | Supervisor subtotal |
|---|---:|---:|---:|---:|---:|
| Branch reference `536ba0c9f` (provided) | 4,285 | 284,447 | 0.61 | 1,405.2 | — |
| Before | 4,206 | 284,234 | 0.59 | 1,495.2 | 52,198.6 |
| After | 3,992 | 284,020 | 0.64 | 1,319.0 | 49,694.5 |

The measured module drag decreased by 176.2 (11.8%). The supervisor subtotal decreased by 2,504.1. These are survey results, not a deterministic attribution of the entire improvement to this refactor.

The deterministic structural changes are:

- One `watch_effect` classification supplies parent lifecycle admission and replay/live termination.
- No single-variant stream dispatcher or replacement trait remains.
- Replay consumes named effects, without rematching event kinds to decode a boolean.
- Restart clears only the run-ended flag; invocation closure survives it.
- Rendering stays separate from lifecycle policy; invisible endings still close live delivery.

## Verification

- All 15 `executor::supervisor::attachment::routing_tests` passed, including the five requested tests and both restart histories.
- `make check` passed.
- `make test` passed: 1,934 gremlins library tests and 49 landscape tests.

Selector admission, boundary identity, checkpoint ordering, lag handling, and invisible parent endings retain their behavioral assertions. Changes are confined to the supervisor watch implementation and its existing inline tests; wire schema, registration capacity, atomic snapshot/live registration, metadata prepass, command rejection, and subscription guard cleanup remain unchanged.
