# Fixes Applied

## crates/gremlins-cli/src/tui/mod.rs, line 607
**Category**: Correctness — unconditional `env.insert()` clobbered user-set env var
**Fix**: Changed to `env.entry(...).or_insert_with(...)` so an operator-exported `GREMLINS_COMPACTION_THRESHOLD_TOKENS` takes precedence over the TUI's default pin.

## crates/gremlins-cli/src/tui/commands.rs, line 312
**Category**: Readability — help text showed Rust literal `u64::MAX` instead of its numeric value
**Fix**: Replaced `u64::MAX` with `18446744073709551615` and added `<tokens>` placeholder to the opt-in instruction ("set GREMLINS_COMPACTION_THRESHOLD_TOKENS=<tokens> to enable it").

## crates/gremlins/src/conversation/standard.rs, lines 248–259
**Category**: Correctness — `last_prompt_tokens` was not reset after compaction, risking double-compaction on retry paths. Readability — `unwrap_or_default()` inside a `is_some_and` guard was dead code.
**Fix**: Restructured to `if let Some(tokens)` + inner `if tokens > threshold`, eliminating the dead fallback. Added `self.last_prompt_tokens = None;` immediately after `self.compact(...)` so one stale reading triggers at most one compaction.

## crates/gremlins/src/config.rs, lines 1184–1201
**Category**: Correctness — env var was subordinate to settings.yaml, so a TUI-loaded settings.yaml could override the `u64::MAX` pin and trigger auto-compaction during interactive chat.
**Fix**: Swapped precedence in `compaction_threshold_tokens()` free function: env var now beats settings.yaml. Updated doc comment to document this. (The `Config` struct constructor retains its own independent precedence chain for its stored value; the free function is the runtime path called by `StandardConversation`.)

## crates/gremlins/src/config.rs, line 2337
**Category**: Testing — default test didn't assert thread-local slate was clean before testing the default path.
**Fix**: Added `assert!(TEST_COMPACTION_THRESHOLD_TOKENS.with(Cell::get).is_none())` before the equality assertion.