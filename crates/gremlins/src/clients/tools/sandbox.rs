use std::path::{Path, PathBuf};

use super::defaults::global_registry;
use super::runtime::AgentLoopRuntime;
use crate::clients::shell_parse::{
    shell_tokenize, split_commands, strip_heredocs, strip_subshells, ShellToken, SubCommandSep,
};
use crate::core::paths::{
    canonicalize_or_ancestor, expand_user, normalize_path, project_root, resolve, roots_suffix,
    within_worktree, within_worktree_lexical,
};

pub(crate) fn audit_log_path(raw_path: &Path) -> PathBuf {
    let stem = raw_path.file_stem().unwrap_or_default();
    let name = format!("{}.audit.jsonl", stem.to_string_lossy());
    match raw_path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Returns Some(error) if `cmd` is an `ln` invocation that requests
/// symbolic linking. Token-based: inspects argv[0] for `ln` and scans
/// subsequent tokens for `-s*`, `--symbolic`, or flags containing `s`.
///
/// Defense-in-depth only. `io_enforce` is the real containment boundary:
/// this misses `env ln -s`, `command ln -s`, `sh -c 'ln -s'`, and any `ln`
/// after `;`/`&&`/`|`, since it only inspects argv[0]. It exists to give the
/// model a clear early denial for the common `ln -s` case, not to be airtight.
fn check_ln_symlink(cmd: &str) -> Option<String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let first = tokens.first()?.trim_matches(|c| c == '\'' || c == '"');
    // Match "ln" or paths ending in "/ln" (e.g. /bin/ln).
    if first != "ln" && !first.ends_with("/ln") {
        return None;
    }
    for tok in &tokens[1..] {
        let stripped = tok.trim_matches(|c| c == '\'' || c == '"');
        if stripped == "--symbolic" || stripped.starts_with("-s") {
            return Some("Error: creating symlinks is not allowed".into());
        }
        // Catch short-flag bundles like -sf, -fs, -sn. Any short flag
        // containing 's' is rejected. This still has false positives
        // (-ns, --version), but those are harmless denials in practice.
        if stripped.starts_with('-') && stripped.len() > 1 && stripped.contains('s') {
            return Some("Error: creating symlinks is not allowed".into());
        }
    }
    None
}

/// Pre-flight static analysis for Bash commands. Rejects commands that
/// reference paths outside the sandbox roots.
///
/// Splits compound commands on `;`, `&&`, and `||`, then processes each
/// sub-command in order while tracking a simulated working directory
/// (`sim_cwd`). When a sub-command is `cd <arg>`, `sim_cwd` is updated so
/// that subsequent relative paths are resolved against the directory the
/// shell would actually be in; `cd` only affects *later* sub-commands, so
/// the operand of the `cd` itself still resolves from the pre-`cd` directory.
///
/// This is a heuristic, not shell emulation. `cd -`, bare `cd`, `cd` with
/// options (`-L`, `-P`, `--`), `cd` after `||`, and `cd` with a
/// variable/substitution target are not tracked. The real containment
/// boundary is `io_enforce`; this check provides early, clear denials for
/// obviously stray path arguments.
/// Validate every path-looking token against `canonical_roots` under
/// `sim_cwd`. Returns the first denial message, or `None` when all pass.
fn check_path_tokens(
    tokens: &[ShellToken],
    sim_cwd: &Path,
    canonical_roots: &[PathBuf],
    roots: &[PathBuf],
) -> Option<String> {
    for tok in tokens {
        if tok.value.is_empty() {
            continue;
        }
        let looks_like_path = tok.value.starts_with('/')
            || tok.value.starts_with('~')
            || tok.value.starts_with("..")
            || tok.value.contains('/');
        if !looks_like_path {
            continue;
        }
        let expanded = if tok.value.starts_with('~') {
            expand_user(&tok.value)
        } else {
            tok.value.clone()
        };
        let p = resolve(&expanded, Some(sim_cwd));
        // Lexical-leaf, not canonical: a final-component symlink whose node is
        // in-worktree (e.g. `.venv/bin/python`) is allowed. See
        // within_worktree_lexical for the rationale and the anti-regression note.
        if !within_worktree_lexical(&p, canonical_roots) {
            return Some(format!(
                "Error: path outside sandbox (from {}): {}{}",
                tok.raw,
                tok.value,
                roots_suffix(roots, Some(sim_cwd))
            ));
        }
    }
    None
}

pub(crate) fn bash_check(roots: &[PathBuf], cmd: &str, cwd: Option<&Path>) -> Option<String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    // Guard against symlink creation via ln.
    if let Some(err) = check_ln_symlink(cmd) {
        return Some(err);
    }
    // Strip heredoc bodies and subshells up front. Neither contains literal
    // path arguments from the agent: heredoc bodies are file content, and
    // subshell paths are runtime-expanded by the shell. Stripping first keeps
    // `;`, `&&`, and `||` inside those regions from being treated as
    // sub-command boundaries.
    let cleaned = strip_subshells(&strip_heredocs(cmd));
    // Canonicalize roots once — avoid re-resolving per token.
    let canonical_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| normalize_path(r, cwd))
        .collect();
    if canonical_roots.is_empty() {
        return Some("Error: invalid sandbox root".into());
    }
    // Simulated working directory — updated when we see a resolvable cd.
    // Canonicalized to match `resolve`, which canonicalizes the base before
    // joining every relative path.
    let mut sim_cwd = match cwd {
        Some(c) => canonicalize_or_ancestor(c),
        None => canonicalize_or_ancestor(&project_root()),
    };

    let sub_commands = split_commands(&cleaned);
    for (sep, sub_cmd) in &sub_commands {
        let tokens = shell_tokenize(sub_cmd);
        if tokens.is_empty() {
            continue;
        }

        // Check all path tokens against the *current* sim_cwd. A `cd` in this
        // sub-command only changes the directory for subsequent commands, so
        // its operand (and any redirection on the same line) resolves from the
        // pre-`cd` directory. Resolving it against the updated directory would
        // falsely deny `cd ..` from a child of the sandbox root.
        if let Some(err) = check_path_tokens(&tokens, &sim_cwd, &canonical_roots, roots) {
            return Some(err);
        }

        // Check for a `cd` command — update sim_cwd for subsequent sub-commands.
        //
        // When the sub-command follows `||` (OR list), we cannot know
        // statically whether it executes, so conservatively skip tracking.
        // When it follows `&&` or `;` (or is the first sub-command), we
        // optimistically track: most compound commands succeed, and
        // over-tracking produces safe false denials rather than dangerous
        // false approvals.
        if *sep == Some(SubCommandSep::OrOr) {
            continue;
        }
        let first = &tokens[0];
        if (first.value == "cd" || first.value.ends_with("/cd")) && tokens.len() >= 2 {
            let arg = &tokens[1];
            // Unresolvable cd targets — skip without touching sim_cwd:
            //   - `cd` with no arg (goes HOME)
            //   - `cd -` (previous dir)
            //   - `cd` with a variable/substitution target ($DIR, `...`, $(...))
            //   - `cd` with options (`cd -- dir`, `cd -P dir`, `cd -L dir`)
            //   - `cd` target replaced by a subshell placeholder (\x01)
            let unresolvable = arg.value == "-"
                || arg.value.contains('$')
                || arg.value.contains('`')
                || arg.value.starts_with('-')
                || arg.value.contains('\x01');
            if !unresolvable {
                let expanded = expand_user(&arg.value);
                let p = resolve(&expanded, Some(&sim_cwd));
                // Validate cd target with canonical containment.
                if !within_worktree(&p, roots, Some(&sim_cwd)) {
                    return Some(format!(
                        "Error: cd target outside sandbox: {}{}",
                        arg.value,
                        roots_suffix(roots, Some(&sim_cwd))
                    ));
                }
                // Update sim_cwd to the fully-normalized target.
                if let Some(normalized) = normalize_path(&p, None) {
                    sim_cwd = normalized;
                }
            }
        }
    }
    None
}

pub(crate) fn audit_key_arg(args_json: &str) -> String {
    let Ok(d) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return String::new();
    };
    if let Some(obj) = d.as_object() {
        for k in ["file_path", "command", "pattern", "path", "kind"] {
            if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    String::new()
}

/// Seconds-resolution UTC timestamp for audit JSONL entries.
fn audit_ts() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // civil_from_days inline
    let z = (days as i64) + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

pub(crate) fn audit(
    log: Option<&Path>,
    lock: Option<&std::sync::Mutex<()>>,
    tool: &str,
    key_arg: &str,
    status: &str,
) {
    let Some(log) = log else {
        return;
    };
    let truncated: String = key_arg.chars().take(200).collect();
    let entry = serde_json::json!({
        "ts": audit_ts(),
        "tool": tool,
        "key_arg": truncated,
        "status": status,
    });
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    if let Some(lock) = lock {
        // audit is best-effort; skip write if the mutex is poisoned rather
        // than panicking and crashing the tool invocation.
        if let Ok(_guard) = lock.lock() {
            audit_write(log, &line);
        }
    } else {
        audit_write(log, &line);
    }
}

fn audit_write(log: &Path, line: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{line}")
        });
}

pub(crate) fn check_tool(
    name: &str,
    ctx: &AgentLoopRuntime,
    args: &serde_json::Value,
) -> Option<String> {
    // Per-tool sandbox policy lives on each handler. AllowAll is expressed as
    // `ctx.sandbox.roots() == None`, which each `sandbox_check` impl honors by
    // returning `None` (allowed) — the old early return moved into the tools.
    global_registry()
        .handlers
        .get(name)
        .and_then(|h| h.sandbox_check(ctx, args))
}
