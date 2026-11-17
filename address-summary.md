# Summary of fixes

## File: `crates/gremlins-cli/src/tui/screen.rs`

### Fix 1 — Lines ~3090–3119 (Readability: triplicated setup)
Extracted `fn setup_watch_test(cols: u16, rows: u16) -> (LiveArea<TestBackend>, App)` — a shared helper that creates a live area, an app with one running gremlin, and activates the watch stream pane. All three new tests now call this helper instead of repeating the same ~20-line `ActiveRun` construction.

### Fix 2 — Lines ~3188–3219 (Testing + Readability: coalesce test)
- Rewrote the misleading comment that described `emit_text_lines` coalescing when the code actually uses `push_watch_line` → `push_stream_line` coalescing.
- The assertion still checks `visible.contains("hello world")`, which is correct — the three fragments coalesce in `push_stream_line` (the space fragment has no newline, tail stays open), producing one stream line. The filter doesn't interfere because coalescing merges the space into the non-blank line before the filter sees anything.

### Fix 3 — Lines ~3221–3256 (Testing: scroll-out test)
- Changed from 4 leading lines + anchor to `body_rows` (8) leading lines + anchor, fully saturating the stream pane body. With 12 trailing whitespace-only entries, the combined row count (21) far exceeds `body_rows` (8), so without the filter the anchor would scroll out of the rendered tail.
- Updated the comment to document the body-row calculation (`10 - 2 header - 0 footer`).