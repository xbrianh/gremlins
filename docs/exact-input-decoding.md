# Exact input decoding

`supervisor::input_wire` is a private synchronous leaf. Its only import is
`serde_json::Value`; its tests additionally import `serde_json::json`. It has no
path to runtime state, watch, client inboxes, receipts, or async I/O. Descriptors
and commands borrow the original strings. The caller supplies canonical ID
validation precisely between required ID and required invocation validation.

The runtime adapter still owns trusted session authorization, exact registration
resolution, nonce comparison, canonical controller claims, revocation, queueing,
and receipt ordering. Socket bind correlation remains outside shared binding.
Watch and chat retain their existing envelopes and wrap the shared retired-field
message in their existing error frames.

## Tree-wide measurement

Both runs used the checkout-root command:

```sh
gremlins-drag crates/gremlins/src --model chatgpt/gpt-6-luna
```

Raw reports are saved in `docs/exact-input-measurements/before.txt` and
`after.txt`. These are source-tree measurements, not isolated-file scores.
The raw reports are verbatim historical tool output. Legacy module-name matches
in them are measurement labels, not current session-chat terminology; they are
retained unchanged so the measurements remain accurate. The truncated historical
`tests::home_bu` label denotes today's session-chat bus tests
(`tests::session_chat_bus`); `tests::home_id` denotes session-chat identity tests
(`tests::session_chat_identity`). These labels are historical evidence, not
current navigation destinations.

| Row | Own tokens before → after | Footprint before → after | Comprehensibility before → after | Drag before → after |
|---|---:|---:|---:|---:|
| `supervisor::input` | 2,521 → 1,904 | 282,683 → 283,147 | 0.61 → 0.64 | 1,400.8 → 1,317.0 |
| `supervisor::input_wire` | absent → 1,077 | absent → 1,077 | absent → 0.77 | absent → 58.5 |
| Supervisor directory subtotal | — | — | — | 50,664.3 → 51,928.1 |

The directory subtotal reports maintainability rather than module
comprehensibility: 0.62 (0.56–0.68) before, 0.60 (0.57–0.63) after. The tool does
not report directory token or footprint subtotals.

The parser footprint is substantially smaller than the original adapter's.
This is dependency isolation, not a claimed net tree-wide saving: the adapter
still reaches runtime dependencies, its footprint grows by 464 tokens, combined
adapter/parser own tokens grow by 460, and the supervisor subtotal grows by
1,263.8 drag. Comprehensibility and maintainability are model estimates; their
variation also affects drag.

## Verification

- Six synchronous parser table tests cover descriptor forms, original strings,
  field allowlists, retired fields, error precedence, injected validator errors,
  padded messages, four controls, ignored extras, and selector bans.
- A synchronous adapter test supplies the actual canonical validator and verifies
  its diagnostic wins over absent `stage_run_id` for inline and nested invalid IDs.
- All 12 exact-input route tests pass, including shared chat controller claims,
  correlated rejection without delivery, and revocation without successor leakage.
- Stalled bind acknowledgment test passes: control is released without dispatch.
- All 11 targeted-chat tests and all seven socket-watch tests pass.
- `make check` passes.
- `make test` passes: 1,930 main-library tests and 49 additional workspace tests.

Existing integration tests remain in place; no runtime code or integration tests
were moved into the parser.
