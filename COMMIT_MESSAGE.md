Fix session-chat replay ownership and report blocked migration gate

Own one replayable, bounded chat stream per gremlin, independent of enqueue
acceptance. Preserve wire turn/execution identity, recover by replay without
resubmitting requests, and show turnless discontinuities as standalone notices.
Retire forwarding and readers before replacement, /clear, and close.

Test retirement reasons:
- Delete session_chat_tests.rs: replace socket transport scenarios with fake-opener/session tests; frame-holding and pending-submission scenarios no longer apply.
- Delete session_chat_reservation_tests.rs: replace reservation socket scenarios with fake opener failures and controlled enqueue replies.
- Delete session_chat_live_warning_tests.rs: idle warnings now remain turnless standalone notices rather than being discarded or adopted by a prompt.
- Delete session_chat_review_tests.rs: gate/discard scenarios are obsolete; replay and recovery coverage now lives with the session stream.
- Delete session_chat_shutdown_tests.rs: replace pending-display socket shutdown with session recreation and close cancellation tests.

Serialize confirmed identity publication with session-owned replay setup, and
retain replay after failed reservations across overlapping submissions. Warn
once when a retry reader closes without restoring fresh delivery. Test active
retirement, malformed/retired acknowledgments, and request-recorded replay
without re-enqueue.

Keep one end-to-end TUI prompt rendering test; migrate other prompt/watch
socket scenarios to socket-free presentation tests or native-input fakes.

STOP: migration acceptance remains blocked against 30eb67f6b. Count all touched
text files, including Makefile, checker, docs, and this message; charge mixed
files wholly to non-test. Non-test: +1328/-921, net +407. Rust test-only:
+1738/-2879, net -1141. Python test-only: +118/-0, net +118.
Test savings cannot offset non-test growth; binary changes fail explicitly.
Seven checker tests pass; make check-session-chat-lines fails as expected.
No runtime implementation added for this accounting correction. Full Rust
validation was not rerun; see docs/session-chat-migration-gate.md.
