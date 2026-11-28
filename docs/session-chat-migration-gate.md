# Session-chat migration: blocked line-count gate

STOP: migration acceptance remains blocked. Non-test net lines must be strictly
negative; test-only savings cannot offset non-test growth.

Baseline: `30eb67f6b`. Endpoint: current working tree plus all non-ignored
untracked files. `make check-session-chat-lines` counts every touched text file,
including Makefile, checker, documentation, and this status report. Tracked
changes include staged and unstaged content once; renames count as delete/add.
Blank lines and comments count. Binary changes fail explicitly, not silently skip.

Rust `*_tests.rs`, `tests.rs`, and Rust files under `tests/` are test-only.
Python `test_*.py` files form a separate test-only category. Everything else,
including mixed production/inline-test files, is charged wholly to non-test.
Moving tests out of mixed files changes classification, not implementation size.

| Category | Added | Deleted | Net physical lines |
|---|---:|---:|---:|
| Non-test (all file types) | 1328 | 921 | +407 |
| Rust test-only | 1738 | 2879 | -1141 |
| Python test-only | 118 | 0 | +118 |

Counts include this document and COMMIT_MESSAGE.md; rerun after any tree change.
Validation: seven checker tests pass; `make check-session-chat-lines` fails as
expected on the non-test limit. Tests cover all-file scope, separate categories,
strict negativity, mixed files, staged/untracked/deleted files, and binary errors.
No runtime implementation was added for this accounting correction. Full Rust
validation was not rerun here; earlier pass claims do not certify this tree.
