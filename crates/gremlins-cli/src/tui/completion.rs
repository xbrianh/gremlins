//! Filepath tab-completion for the main input bar.
//!
//! This module owns all filesystem access for completion. The event loop and
//! [`App`](crate::tui::app::App) stay I/O-free: they only call
//! [`complete`] and keep the returned inert [`CompletionState`].

use std::path::{Path, PathBuf};

/// The result of one completion step.
///
/// The module exposes a pure entry point ([`complete`]) that performs the
/// filesystem listing, and callers store this inert value so subsequent
/// Tab / Shift+Tab presses can cycle through candidates without touching the
/// filesystem again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionState {
    /// All matching entries, returned as full path words (directory and file
    /// entries alike).
    pub candidates: Vec<String>,
    /// Index of the currently selected candidate.
    pub index: usize,
    /// Byte range in the pre-completion input that the selected candidate
    /// replaces. `start` is the first byte of the path word being completed
    /// and `end` is the cursor offset that produced the completion.
    pub span: std::ops::Range<usize>,
}

impl CompletionState {
    /// The candidate currently selected.
    pub fn current(&self) -> &str {
        &self.candidates[self.index]
    }

    /// Advance to the next candidate, wrapping around at the end.
    pub fn advance(&mut self) {
        if !self.candidates.is_empty() {
            self.index = (self.index + 1) % self.candidates.len();
        }
    }

    /// Rewind to the previous candidate, wrapping around at the start.
    pub fn rewind(&mut self) {
        if !self.candidates.is_empty() {
            self.index = (self.index + self.candidates.len() - 1) % self.candidates.len();
        }
    }
}

/// Extract the path word being completed.
///
/// The word runs from the byte after the last whitespace boundary before
/// `cursor` through `cursor`. Returns `None` when there is nothing to
/// complete (for example an empty word or a cursor that is not on a
/// `char` boundary).
pub fn path_word(input: &str, cursor: usize) -> Option<&str> {
    if cursor == 0 || cursor > input.len() || !input.is_char_boundary(cursor) {
        return None;
    }
    let before = &input[..cursor];
    let start = before
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = &input[start..cursor];
    if word.is_empty() {
        return None;
    }
    Some(word)
}

/// Compute the longest common byte prefix of the candidate set.
pub fn common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut end = first.len();
    for candidate in &candidates[1..] {
        end = end.min(candidate.len());
        let mut n = 0;
        let a = first.as_bytes();
        let b = candidate.as_bytes();
        while n < end && a[n] == b[n] {
            n += 1;
        }
        end = n;
        if end == 0 {
            break;
        }
    }
    first[..end].to_string()
}

/// Expand a leading `~` in `word` to the operator's home directory.
///
/// Returns the expanded path as owned data when expansion succeeded, and
/// `None` when the word does not start with `~` or the home directory is
/// unavailable.
fn expand_home(word: &str) -> Option<PathBuf> {
    let rest = word.strip_prefix('~')?;
    // Only a bare `~` (or `~/...`) is expanded. `~user` is left to the
    // shell; attempting to list it would produce a noisy candidate path.
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(if rest.is_empty() {
        home
    } else {
        home.join(rest.trim_start_matches('/'))
    })
}

/// Normalize a path word for directory listing.
///
/// Returns the directory to list and the partial component that must match
/// entry names. A path with no trailing separator keeps the final component
/// as the partial; a path ending in `/` (or with no component at all) lists
/// inside that directory with an empty partial.
fn split_prefix(word: &str, expanded: PathBuf) -> (PathBuf, String) {
    if word.ends_with('/') {
        (expanded, String::new())
    } else {
        match (expanded.parent(), expanded.file_name()) {
            (Some(dir), Some(file)) if !dir.as_os_str().is_empty() => {
                (dir.to_path_buf(), file.to_string_lossy().into_owned())
            }
            // The expanded path has no parent (e.g. a relative filename).
            (_, Some(file)) => (PathBuf::from("."), file.to_string_lossy().into_owned()),
            (Some(dir), None) => (dir.to_path_buf(), String::new()),
            (None, None) => (PathBuf::from("."), String::new()),
        }
    }
}

/// Build the full path word for a directory entry.
fn entry_path_word(dir: &Path, name: &str, is_dir: bool) -> String {
    let joined = dir.join(name);
    let mut out = joined.to_string_lossy().into_owned();
    if is_dir && !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// Enumerate filesystem entries matching the expanded `word`.
///
/// The partial component after the final `/` is matched case-sensitively
/// against entry names. Directory entries receive a trailing `/` so a later
/// Tab completes into that directory. Failures are silent and yield an empty
/// candidate list.
pub fn complete(input: &str, cursor: usize) -> Option<CompletionState> {
    let word = path_word(input, cursor)?;
    let expanded = expand_home(word).unwrap_or_else(|| PathBuf::from(word));
    let (dir, partial) = split_prefix(word, expanded);

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return None,
    };

    let mut candidates: Vec<String> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&partial) {
            continue;
        }
        // We don't require `entry.file_type()` to append `/`; use the
        // file-name component only. A trailing slash is added for known
        // directories below.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        candidates.push(entry_path_word(&dir, &name, is_dir));
    }

    if candidates.is_empty() {
        return None;
    }

    candidates.sort();
    candidates.dedup();

    Some(CompletionState {
        candidates,
        index: 0,
        span: word_start(input, cursor)..cursor,
    })
}

/// Byte start of the word being completed (kept in sync with [`path_word`]).
fn word_start(input: &str, cursor: usize) -> usize {
    let before = &input[..cursor];
    before
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_word_extracts_after_last_whitespace() {
        let input = "read file na";
        assert_eq!(path_word(input, input.len()), Some("na"));
        assert_eq!(path_word("na", 2), Some("na"));
    }

    #[test]
    fn path_word_at_multibyte_cursor_boundary() {
        let input = "héllo/wörld";
        let cursor = "héllo/wör".len();
        assert_eq!(path_word(input, cursor), Some("héllo/wör"));
        assert!(input.is_char_boundary(cursor));
    }

    #[test]
    fn path_word_rejects_bad_offsets() {
        assert_eq!(path_word("abc", 2), Some("ab"));
        assert_eq!(path_word("aé", 1), Some("a"));
        assert_eq!(path_word("aé", 3), Some("aé"));
        assert_eq!(path_word("aé", 2), None); // inside é
        assert_eq!(path_word("", 0), None);
        assert_eq!(path_word("abc", 4), None);
    }

    #[test]
    fn path_word_honors_cursor_not_input_end() {
        assert_eq!(path_word("hello world x", 13), Some("x"));
        assert_eq!(path_word("hello world x", 12), None); // cursor on the space
    }

    #[test]
    fn common_prefix_is_longest_byte_prefix() {
        let c = vec![
            "src/app.rs".to_string(),
            "src/auto".to_string(),
            "src/ast.rs".to_string(),
        ];
        assert_eq!(common_prefix(&c), "src/a");
        assert_eq!(common_prefix(&["a".to_string()]), "a");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn common_prefix_handles_multibyte() {
        let c = vec!["héllo world".to_string(), "héllo there".to_string()];
        assert_eq!(common_prefix(&c), "héllo ");
    }

    #[test]
    fn advance_and_rewind_wrap() {
        let mut state = CompletionState {
            candidates: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            index: 2,
            span: 0..1,
        };
        assert_eq!(state.current(), "c");
        state.advance();
        assert_eq!(state.current(), "a");
        state.rewind();
        assert_eq!(state.current(), "c");
        state.rewind();
        assert_eq!(state.current(), "b");
    }

    #[test]
    fn tilde_expands_to_home_directory() {
        let home = std::env::var_os("HOME").unwrap();
        let expanded = expand_home("~").unwrap();
        assert_eq!(expanded, PathBuf::from(&home));
        let expanded = expand_home("~/src").unwrap();
        assert_eq!(expanded, PathBuf::from(&home).join("src"));
    }

    #[test]
    fn tilde_user_is_not_expanded() {
        assert!(expand_home("~nobody").is_none());
    }

    #[test]
    fn split_prefix_handles_trailing_slash() {
        let (dir, partial) = split_prefix("src/", PathBuf::from("src"));
        assert_eq!(dir, PathBuf::from("src"));
        assert_eq!(partial, "");
    }

    #[test]
    fn split_prefix_handles_plain_filename() {
        let (dir, partial) = split_prefix("Ca", PathBuf::from("Ca"));
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(partial, "Ca");
    }

    #[test]
    fn entry_path_word_terminates_dirs_with_slash() {
        assert_eq!(
            entry_path_word(Path::new("src"), "main.rs", false),
            "src/main.rs"
        );
        assert_eq!(entry_path_word(Path::new("src"), "sub", true), "src/sub/");
    }

    #[test]
    fn completion_in_temp_dir_returns_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("apple.txt"), "x").unwrap();
        std::fs::write(dir.path().join("apricot.txt"), "x").unwrap();
        std::fs::write(dir.path().join("banana.txt"), "x").unwrap();

        let input = format!("{}/ap", dir.path().display());
        let state = complete(&input, input.len()).unwrap();
        let names: Vec<String> = state.candidates.clone();
        assert_eq!(names.len(), 2);
        assert!(names
            .iter()
            .all(|c| c.ends_with("apple.txt") || c.ends_with("apricot.txt")));
        assert_eq!(state.span, 0..input.len());
    }

    #[test]
    fn completion_single_candidate_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("unique.txt"), "x").unwrap();

        let input = format!("{}/uniq", dir.path().display());
        let state = complete(&input, input.len()).unwrap();
        assert_eq!(state.candidates.len(), 1);
        assert_eq!(
            state.current(),
            format!("{}/unique.txt", dir.path().display())
        );
    }

    #[test]
    fn completion_failure_is_silent() {
        assert!(complete("/definitely/not/here/pa", 24).is_none());
    }
}
