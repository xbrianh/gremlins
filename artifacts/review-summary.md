# Review Fixes Summary — PR #2231

## Changes made

### Comment 4528371283 — `dispatch()` receiving resolved absolute paths

**Problem:** The Enter handler called `dispatch(&input)` with the resolved (absolute-path) text. A completed relative path like `Cargo.toml` became `/abs/path/Cargo.toml`, which dispatch treated as an unknown slash command.

**Fix:** `dispatch()` now receives `display_input` (the operator-visible form). The resolved `input` still feeds `send_chat_input()` (for the model) and `push_history()` (for recall). The transcript `> {input}` block also shows `display_input` for consistency.

- **File:** `crates/gremlins-cli/src/tui/mod.rs`
  - Line 1820: `dispatch(&display_input)` instead of `dispatch(&input)`
  - Line 1849: `"> {display_input}"` instead of `"> {input}"`
  - Line 1810: `.clone()` needed since `display_input` is used after `push_history`

### Comment 4528371356 — History search discards path substitutions

**Problem:** `enter_search()` only saved `self.input.text().to_string()` (the display string), then cleared the buffer. On cancel (`Esc`) or no-match fallback, only the display text was restored — substitutions were lost.

**Fix:** `HistorySearch::saved_input` changed from `String` to `InputBuffer`. `enter_search()` snapshots the entire buffer via `std::mem::take`, and both `cancel_search()` and `accept_search()` restore via `self.input = search.saved_input`.

- **File:** `crates/gremlins-cli/src/tui/app.rs`
  - `HistorySearch::saved_input`: `String` → `InputBuffer`
  - `enter_search()`: `std::mem::take(&mut self.input)` instead of `self.input.text().to_string()`
  - `cancel_search()`: `self.input = search.saved_input` instead of `self.input.replace_all(&search.saved_input)`
  - `accept_search()` no-match fallback: same restoration pattern

### Comment 4528371448 — Bare `~` drops tilde from display candidates

**Problem:** `display_path_word()` used `word.rfind('/')` for the prefix, which returned `None` for bare `~`, producing `Documents/` instead of `~/Documents/`. The next Tab then completed under the working directory instead of `~`.

**Fix:** Special-case `word == "~"` to use `"~/"` as the prefix. For `~/...` paths, `rfind('/')` already returns the correct position (the `/` after `~`).

- **File:** `crates/gremlins-cli/src/tui/completion.rs`
  - `display_path_word()`: added `if word == "~" { "~/" }` branch before the `rfind` fallback