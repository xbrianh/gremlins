# Input-rule test migration

Replace `tests/correction_chat.rs` and `tests/exact_input.rs` with exactly five socket smoke tests in `tests/input_wiring.rs`. Pure validation and receipt bookkeeping belong in `input_rules::tests`; execution inbox semantics remain covered by execution-input tests. No excluded watch, paused-chat, session-chat identity, or turn-runner tests are changed.

## New socket tests

- **a** `a_global_bound_queued_delivered`: global binding and consumer-confirmed delivery.
- **b** `b_owned_session_targeted_chat_queued_delivered`: trusted session ownership and targeted-chat queued/delivered adapter framing.
- **c** `c_competing_claim_rebind_after_disconnect`: exclusive claim and explicit reacquisition after EOF.
- **d** `d_held_receipt_disconnect_synthesizes_no_command`: disconnect with an unsettled receipt releases control without injecting a control command.
- **e** `e_revoke_held_dropped_ended_successor_has_no_input`: held input revocation, dropped-before-ended framing, and successor isolation.

## Deleted `tests/correction_chat.rs` tests

| Deleted test | Replacement or redundancy reason |
|---|---|
| `second_prompt_is_gated_and_each_start_matches_live_snapshot_then_becomes_stale` | Session-chat setup/start identity is independently covered by unchanged `session_chat_bus` and `session_chat_identity` tests; stale binding is covered by `binding_rejection_table_and_precedence`. |
| `reregistered_same_nonce_rejects_old_execution_and_invalid_targets_without_prompting` | `binding_rejection_table_and_precedence`, `descriptor_validation_table_and_canonical_id_precedence`; inbox registration identity remains an execution-input concern. |
| `retired_target_keys_are_rejected_before_resolution_without_delivery` | `retired_aliases_on_every_route`, `chat_preflight_preserves_precedence_even_without_target`. |
| `correction_rejects_malformed_targets_without_starting_chat` | `chat_preflight_preserves_precedence_even_without_target`, `descriptor_validation_table_and_canonical_id_precedence`. |
| `aliased_corrections_echo_client_identity_but_claim_canonical_execution` | `successful_routes_use_resolved_identity_and_optional_nonce` and `chat_alias_frames_echo_client_selector_not_canonical_invocation` check canonical binding and byte-compatible alias framing; b checks chat adapter delivery. |
| `held_correction_releases_claim_and_keeps_main_dispatcher_responsive` | `input_flow::tests::held_correction_allows_second_claim_before_settlement` uses fake RAII claim state and fake held correction/receipt channels through the production enqueue helper, acquires a second claim before either receipt settles, then settles both independently. It does not exercise session actors or prove session responsiveness. |
| `revocation_drops_queued_unconsumed_correction_without_successor_leakage` | `duplicate_request_ids_settle_independently_and_revoke_does_not_retarget`; queued revocation is also an execution-input concern; e retains socket isolation coverage. |
| `revocation_drops_polled_held_correction_without_successor_leakage` | e plus `duplicate_request_ids_settle_independently_and_revoke_does_not_retarget`; b retains chat-specific delivery framing. |
| `full_execution_inbox_rejects_correction_without_queued_ack` | Full inbox rejection belongs to execution-input tests, not pure receipt bookkeeping; b is the chat adapter smoke test. |
| `main_disconnect_with_held_correction_leaves_independent_target_bindable` | d retains held-receipt EOF/claim-release coverage; unchanged session teardown tests cover main-session lifecycle. |

## Deleted `tests/exact_input.rs` tests

| Deleted test | Replacement or redundancy reason |
|---|---|
| `sibling_exact_lookup_routes_independently_of_snapshot_and_receipts` | a; `successful_routes_use_resolved_identity_and_optional_nonce`, `command_decoding_and_errors_are_wire_exact`, `duplicate_request_ids_settle_independently_and_revoke_does_not_retarget`; sibling lookup remains an execution-input concern. |
| `chat_descriptor_and_invocation_share_canonical_controller_claim` | `successful_routes_use_resolved_identity_and_optional_nonce` plus c; alias resolution belongs to execution-input tests. |
| `chat_requires_trusted_owning_session_not_request_session_id` | `binding_rejection_table_and_precedence` covers missing/closed/foreign ownership; b covers a trusted owning session adapter. |
| `no_watch_global_and_session_contention_cannot_be_bypassed_by_chat` | `binding_rejection_table_and_precedence` covers controller-busy precedence across input/chat; c covers actual claim acquisition/release. |
| `racing_global_and_session_binds_have_one_winner_and_no_automatic_handoff` | c; atomic exclusivity is independently covered by registry controller-claim tests. No duplicated route/race matrix. |
| `parent_and_review_watches_survive_writer_contention_and_reacquisition` | Redundant with unchanged watch routing/lifecycle tests and c's controller smoke test; display is not input authority. |
| `full_inbox_rejects_without_queued_and_connection_remains_usable` | Execution-input full-capacity tests own enqueue rejection; pure rules own command validation and receipt capacity (`pending_limit_precedes_command_validation_and_settlement_frees_capacity`). |
| `revocation_drops_queued_and_held_then_ends_without_successor_leakage` | e plus `duplicate_request_ids_settle_independently_and_revoke_does_not_retarget`; queued/held inbox revocation belongs to execution-input tests. |
| `malformed_selectors_and_bound_retargeting_never_enqueue` | `descriptor_validation_table_and_canonical_id_precedence`, `retired_aliases_on_every_route`, `command_decoding_and_errors_are_wire_exact`. |
| `rejection_matrix_is_reasoned_correlated_and_never_enqueues_on_either_route` | `binding_rejection_table_and_precedence`, `descriptor_validation_table_and_canonical_id_precedence`; route duplication removed. |
| `idle_controller_does_not_block_own_or_sibling_progress_and_closes_before_successor` | Agent progress is not a pure binding rule; unchanged agent/execution tests cover progress, while e covers exact successor isolation. Large mock-agent/socket cross-product removed. |
| `eof_releases_control_without_synthesizing_commands` | c and d. |

## Removed `input.rs::tests` inventory

These adapter tests were removed during integration; their replacements are listed below.

| Deleted test | Replacement or redundancy reason |
|---|---|
| `canonical_id_error_precedes_missing_stage_run_id` | `descriptor_validation_table_and_canonical_id_precedence`. |
| `message_receipt_requires_consumer_confirmation` | a and b; execution-input tests own consumer confirmation itself. |
| `controls_have_receipts_and_revocation_drops_them` | `command_decoding_and_errors_are_wire_exact`, `duplicate_request_ids_settle_independently_and_revoke_does_not_retarget`; execution-input tests own control incorporation. |
| `bound_commands_reject_retargeting_and_retired_fields` | `command_decoding_and_errors_are_wire_exact`, `retired_aliases_on_every_route`. |
| `inline_text_is_only_allowed_on_chat_envelopes` | Descriptor validation remains delegated to `input_wire`; `successful_routes_use_resolved_identity_and_optional_nonce` covers chat envelopes. Existing wire parser tests cover unsupported fields. |
| `bind_rejections_echo_request_id_and_require_it` | `binding_rejection_table_and_precedence` and pure rejection framing; request-id validity remains delegated to the wire parser. |
| `failed_bind_ack_releases_claim_while_reader_is_open` | `input_flow::tests::failed_ack_releases_claim_without_pipelined_dispatch` injects an immediately failed ACK outcome into the production ownership gate, proves fake RAII claim release/reacquisition without EOF, and verifies no fake pipelined dispatch. |
| `stalled_ack_times_out_without_dispatching_pipelined_bail` | `input_flow::tests::timed_out_ack_releases_claim_without_pipelined_dispatch` injects an immediate fake timed-out ACK outcome into the production ownership gate and proves claim release/reacquisition with no fake pipelined dispatch. No writer is stalled and no real timeout elapses. |
| `pending_limit_rejects_excess_without_losing_held_receipts` | `pending_limit_precedes_command_validation_and_settlement_frees_capacity`, `duplicate_request_ids_settle_independently_and_revoke_does_not_retarget`. |
| `blocked_ack_does_not_block_revocation_or_claim_release` | Implementation-specific oversized-ACK stress removed; e retains real revocation completion and c retains claim release. |
| `exact_descriptors_validate_sibling_nonce_and_reject_conflicts` | `successful_routes_use_resolved_identity_and_optional_nonce`, `binding_rejection_table_and_precedence`, `descriptor_validation_table_and_canonical_id_precedence`, `retired_aliases_on_every_route`; b retains targeted-chat wiring. |
| `stalled_post_enqueue_writer_does_not_block_execution_or_disconnect` | Oversized-response stress removed from the reduced suite; d retains disconnect while a receipt is held. Pure rules do not claim to test socket backpressure. |
| `stalled_post_enqueue_writer_times_out_and_releases_claim` | Implementation-specific writer-timeout stress removed; c/d retain ordinary EOF claim-release coverage. |
| `exact_selector_fields_must_be_nonempty_strings` | Descriptor validation remains delegated to the existing wire parser tests; `descriptor_validation_table_and_canonical_id_precedence` covers rule-layer validation. |

ACK ownership gating and chat enqueue/release are tested through small generic production helpers in `input_flow`, using only fake RAII claim state (no RunRegistry, InputRegistry, disk, or session actors). `successful_ack_precedes_dispatch_and_retains_claim` additionally checks ACK-before-dispatch and claim retention; `rejected_enqueue_releases_claim_before_return` covers enqueue rejection. Failed/timed-out outcomes are immediate fakes: these tests do not exercise transport writes, real timeout scheduling, or reader lifetime. Production `serve_bound_io` still selects revocation against the bounded ACK write before passing its outcome to the gate. Production `targeted_chat_io` enqueues through the release helper before awaiting delivery. Other removed oversized-write/timeout tests are deliberate reductions, not assertions that pure rules prove transport timeout behavior. The five retained socket tests are unchanged; they check adapter boundaries without rebuilding the old socket cross-product.

## Scope constraints

The plan's literal repository-wide socket/selector searches conflict with its explicit exclusions: unchanged `paused_chat.rs` and `session_chat_identity.rs` still construct input sockets, while watch tests and handlers retain their own socket and retired-selector coverage. Those excluded files were preserved. All input-scenario sockets within this pass's editable test scope now live in `input_wiring.rs`.

## Commit-message inventory correction

The follow-up to `06c26e470` now records these current replacements in its
commit message, superseding the obsolete `input::adapter_tests` references:

- `failed_bind_ack_releases_claim_while_reader_is_open` → `input_flow::tests::failed_ack_releases_claim_without_pipelined_dispatch`
- `stalled_ack_times_out_without_dispatching_pipelined_bail` → `input_flow::tests::timed_out_ack_releases_claim_without_pipelined_dispatch`
- `held_correction_releases_claim_and_keeps_main_dispatcher_responsive` → `input_flow::tests::held_correction_allows_second_claim_before_settlement`

The other deleted-test mappings in `06c26e470` remain unchanged. The corrected
message describes fake RAII claims and immediate fake ACK outcomes, without
claiming transport timeout, reader-lifetime, or session-responsiveness coverage.

## Verification

Commit-message correction verification:

- `cargo test -p gremlins --lib -- executor::supervisor::input_flow::tests`: all five registry-free lifecycle tests passed.

Step 4 targeted verification:

- `make fmt` passed.
- `cargo test -q -p gremlins --lib executor::supervisor::input`: 30 passed, including the five registry-free lifecycle tests and two retained adapter tests.
- `cargo test -q -p gremlins --lib executor::supervisor::tests::input_wiring`: all five unchanged socket wiring tests passed.

Final corrective verification:

- `make check`: formatting, all-target clippy, and workspace check passed.
- `make test`: 1,927 gremlins library tests and 49 landscape tests passed.
- `git diff --check` passed.

The accompanying corrective commit includes every deleted-test mapping below its summary; this document is not a substitute for the commit-message inventory. For Step 4, that inventory must name `input_flow::tests::failed_ack_releases_claim_without_pipelined_dispatch`, `input_flow::tests::timed_out_ack_releases_claim_without_pipelined_dispatch`, and `input_flow::tests::held_correction_allows_second_claim_before_settlement` as the replacements for the three removed Fixture-based adapter tests. Describe fake RAII state and immediate failed/timed-out ACK outcomes, not real registry rebinding, open readers, elapsed timeouts, or session responsiveness.
