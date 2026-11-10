## Fix summary: Bind Ctrl+W to open the watch screen in the TUI

### 1. `mod.rs:1522–1528` — Missing `ensure_watch_selection()` on Chat→Runs entry (Correctness — both reviews)

**Issue:** The `Screen::Chat` arm called `app.show_runs()` / `live_area.show_runs()` / `fetch_run_statuses()` but never `app.ensure_watch_selection()`, despite the plan's constraints explicitly listing it among the required entry-point methods. The run table could render with no watch selection set (bug: selections slide to first run in display but `ensure_watch_selection` guards against clobbering existing valid selections, and it's the canonical path the `/watch` slash-command handler takes via `show_runs()`).

**Fix:** Added `app.ensure_watch_selection()` after `live_area.show_runs()` in the `Screen::Chat` arm, matching what `App::show_runs()` itself does internally.

### 2. `mod.rs:1529–1548` — `Ctrl+W` on Runs screen never opens a pane (Correctness — both reviews)

**Issue:** The `Screen::Runs` `else` branch (no pane open) called `close_watch_follow` (redundant, since `watch_following` was `None`) and then exited to chat via `app.exit_screen()` / `live_area.exit_screen_view()`. This contradicted the plan's requirement that `Ctrl+W` "behave like `Ctrl+O` on the selected gremlin: toggle the inline watch pane" — the binding should open the pane when closed and close it when open, only exiting to chat on a subsequent press when no pane is open.

**Fix:** Replaced the `else` branch with a three-way match:
- `watch_following.is_some()` → close the pane (existing behavior, unchanged).
- `watch_selection` exists → open the pane via `start_watch_follow(...)`, mirroring `Ctrl+O`.
- Neither → exit to chat (fallback when the table is empty or nothing is selectable).

### 3. `mod.rs:1517` — Misleading comment about prompt guard (Readability — review two)

**Issue:** The comment "Inert on Debug (prompt owns input)" described the `has_prompt()` check as Debug-specific, but `Screen::has_prompt()` returns `true` for `Debug` only, and the guard runs for every screen — the comment was confusing.

**Fix:** Reworded to "Inert while any prompt owns the input bar."

### 4. `commands.rs:316` — Help text overstated behavior (Readability — both reviews)

**Issue:** The help line "enter/exit the watch screen; in /watch: toggle live log" claimed `Ctrl+W` could "exit" the watch screen directly, but the original implementation only exited when no pane was open — and after the fix it only exits when nothing is selectable. The "toggle live log" wording was also imprecise.

**Fix:** Updated to "enter the watch screen; in /watch: toggle the selected gremlin's live log".

### 5. `DESIGN.md:885–888` — Outdated and contradictory documentation (Readability — both reviews)

**Issue:** The DESIGN.md prose claimed `Ctrl+W` "opens a tail-anchored inline log pane for the selected run" and "exits back to chat when no pane is open", which (a) overstated the original code (never opened), and (b) didn't describe the new toggle semantics.

**Fix:** Reworded to document the full toggle behavior: opens the pane, closing it on second press, and exiting to chat only when there is no selection left to follow.

### 6. `chat_system.md:71` — Description didn't match toggle semantics (Readability — both reviews)

**Issue:** The help text said "the first press closes the inline pane and the next returns to chat", assuming the user already had a pane open (e.g. via `Ctrl+O`). This didn't describe the general toggle behavior.

**Fix:** Updated to "enter the watch screen from chat; inside `/watch`, toggle the selected gremlin's live log (open it, or close an open pane); exits to chat when there is no selection left to follow."