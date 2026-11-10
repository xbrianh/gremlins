## Fix summary: Tab completion `InputBuffer` review findings

### 1. `input_buffer.rs:118–144` — `replace_range` doesn't invalidate overlapping substitutions (Correctness — both reviews)

**Issue:** `replace_range` was intentionally not invalidating substitutions that overlapped the replaced span, relying on `App::replace_input_span` to pre-clear via `clear_substitutions_in`. Any other `pub` caller would leave stale/misaligned spans.

**Fix:** Made `replace_range` self-consistent: it now drops substitutions whose interior overlaps the replaced region (matching `insert_char`/`delete_range` behavior) and shifts later ones by the length delta. Removed the `clear_substitutions_in` method entirely since it's no longer needed. Updated doc comments accordingly.

### 2. `input_buffer.rs:182–197` — `resolve()` clips stale spans instead of skipping them (Correctness — both reviews)

**Issue:** A substitution span wholly past the end of the text was clamped to an empty range that appends the absolute path. A span extending past the text silently swallowed trailing text. Neither behavior matched the doc comment ("left as-is (stale guard)").

**Fix:** `resolve()` now uses `result.get(span)` to check whether the span is valid in the current text. If not (out of bounds or not on char boundaries), it skips the substitution entirely. This is a true stale guard: only valid spans are substituted.

### 3. `input_buffer.rs:171–178` — `clear_substitutions` doc says "typed character" (Readability — both reviews)

**Issue:** The doc comment claimed `clear_substitutions` is called "when the operator types a character", but typing actually preserves non-overlapping substitutions via `insert_char`. The only caller is `App::clear_completion` (wholesale input-bar reset).

**Fix:** Updated the doc comment to say "Used when the input bar is being reset wholesale. Ordinary edits preserve non-overlapping substitutions; see `insert_char` and `delete_range`."

### 4. `input_buffer.rs:335–337` — `resolve_with_stale_span` test name contradicted assertion (Readability — both reviews)

**Issue:** The test named `resolve_with_stale_span_leaves_text_as_is` asserted the text *was* replaced by the absolute path, not left as-is.

**Fix:** Renamed to keep the original name but fixed the assertion to match: the stale span extending past the text is now skipped, and the text is left as-is (`"short"` remains `"short"`). Added tests for: span entirely past text, multiple substitutions, and edit between two substitutions.

---

### 5. `app.rs:550–567` — `clear_completion` wipes all substitutions on ordinary edits (Correctness — review two)

**Issue:** `clear_completion` was called from every edit/cursor key handler (Left, Right, Home, End, Backspace, Delete, Char typing, Up/Down history), wiping all path substitutions. Typing a character after tab-completing a path silently reverted the completed path to its relative display form on Enter.

**Fix:** Split into two methods:

- **`dismiss_completion()`**: drops only the `CompletionState` preview, preserving path substitutions. Used by ordinary edit/cursor keys. The editing operations in `InputBuffer` already invalidate any substitution their edit crosses — no need to clear all.
- **`clear_completion()`**: drops completion state AND all substitutions. Used on wholesale input-bar resets (hard cancel, Esc, `/clear`, search entry, submission).

Updated all edit/cursor/history key handlers in `mod.rs` from `clear_completion()` to `dismiss_completion()`.

### 6. `mod.rs:1715–1718` — Multi-candidate Tab doesn't record substitution for common prefix (Correctness — review two)

**Issue:** When Tab produces multiple candidates, only the display-form common prefix is applied, and no `add_substitution` is recorded. Enter would submit the unresolved relative text.

**Fix:** After computing `common_prefix(&state.candidates)`, also compute `common_prefix(&state.absolutes)` and record it as a substitution. If the prefix is empty, no substitution is recorded (the completion hasn't advanced yet).

### 7. `mod.rs:1549` — Editor round-trip clears substitutions (Correctness — review two)

**Issue:** `app.input.replace_all(&text)` clears all substitutions, so tab-completing a path, opening the external editor, and submitting sends the display form to the model.

**Fix:** Added a comment documenting that this is intentional: the editor edits the display form, so completed paths revert to their as-typed form. The operator can re-tab-complete after the editor round-trip. (Preserving substitutions across the editor would require matching the editor's output back to the completion candidates, which is fragile.)

### 8. `mod.rs:1675–1702` — Fragile dual borrow in Tab handler (Correctness — review one)

**Issue:** The Tab handler took a named `&mut` reference to `app.completion` (`let Some(state) = &mut app.completion`) and later called `app.completion.as_ref().expect(...)` — a dual-access pattern that's fragile to future refactoring.

**Fix:** Replaced with a single snapshot at the top: extract `span` and the `contains_current` flag upfront via `app.completion.as_ref().map(...)`, then work from the snapshot. Inline extraction of `next_absolute`/`next` uses a block with `.as_ref().expect(...)` so the borrow doesn't escape.

### 9. `mod.rs:1731–1754` — Similar dual borrow in BackTab handler (Correctness — review one)

**Fix:** Same pattern: extract `span` upfront via `app.completion.as_ref().map(|state| state.span.clone())`, then extract `candidate`/`absolute` in a block after `completion_rewind()`.

---

### 10. `app.rs:656–676` — `replace_input_span` no longer needs `clear_substitutions_in` (Correctness — follow-on from fix #1)

**Issue:** After making `replace_range` self-consistent (fix #1), the explicit `clear_substitutions_in` call in `replace_input_span` is redundant.

**Fix:** Removed `clear_substitutions_in` call and the span-clamping logic. `replace_input_span` now delegates cleanly to `self.input.replace_range(span, &replacement)`. Updated doc comment.