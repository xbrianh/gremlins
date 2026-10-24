//! Operator-defined chat skills (Claude Code `SKILL.md` format).
//!
//! Skills are markdown files with YAML frontmatter, placed under
//! `.gremlins/skills/<name>/SKILL.md` (project) or
//! `~/.config/gremlins/skills/<name>/SKILL.md` (user-global). Project skills shadow
//! user-global skills on name collision.
//!
//! The on-disk format is the Claude Code `SKILL.md` spec: a `---`-delimited
//! YAML block followed by a markdown body. Every field uses `#[serde(default)]`
//! so unknown keys are silently dropped for Claude Code compatibility.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Active skill entries
// ---------------------------------------------------------------------------

/// An active skill entry, carrying the skill name, shell-split positional
/// arguments, and the raw argument text (for `$ARGUMENTS` expansion).
#[derive(Debug, Clone)]
pub struct ActiveSkill {
    pub name: String,
    pub args: Vec<String>,
    /// Raw argument text as supplied by the model, preserved verbatim for
    /// `$ARGUMENTS` substitution.
    pub argument_text: String,
}

// ---------------------------------------------------------------------------
// Skill metadata (frontmatter)
// ---------------------------------------------------------------------------

/// Parsed frontmatter from a `SKILL.md` file.
///
/// Every field defaults. Unknown keys are silently dropped — this is the
/// Claude Code compatibility property.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Skill {
    /// Skill name. Defaults to the parent directory name when absent.
    #[serde(default)]
    pub name: String,

    /// Short description. Defaults to the first non-empty line of the
    /// markdown body when absent, truncated at 1,536 characters.
    #[serde(default)]
    pub description: String,

    /// Arbitrary version string (Claude Code compatibility).
    #[serde(default)]
    pub version: String,

    /// Author (Claude Code compatibility).
    #[serde(default)]
    pub author: String,

    /// Homepage URL (Claude Code compatibility).
    #[serde(default)]
    pub homepage: String,

    /// License identifier (Claude Code compatibility).
    #[serde(default)]
    pub license: String,

    /// When `true`, the skill is invisible to the model (omitted from the
    /// skill index injected into the system prompt). The operator can still
    /// activate it with `/skill`.
    #[serde(alias = "disable-model-invocation", deserialize_with = "deser_bool")]
    #[serde(default)]
    pub disable_model_invocation: bool,

    /// When `false`, `/skill <name>` and `/skills` hide this skill from the
    /// operator. Claude Code compatibility field.
    #[serde(alias = "user-invocable", deserialize_with = "deser_bool")]
    #[serde(default = "default_true")]
    pub user_invocable: bool,

    /// Tool allowlist. Parsed and validated but **not enforced** — tool
    /// filtering is out of scope for this pass.
    #[serde(alias = "allowed-tools", deserialize_with = "deser_allowed_tools")]
    #[serde(default)]
    pub allowed_tools: Vec<String>,

    /// Positional argument names. Maps `$name` placeholders to positions.
    /// `arguments: [issue, branch]` means `$issue` → $0, `$branch` → $1.
    #[serde(default)]
    pub arguments: Vec<String>,

    /// When-to-use guidance appended to the description in the index (Phase 3).
    /// Parsed now for forward compatibility.
    #[serde(default)]
    pub when_to_use: String,

    /// Raw markdown body (everything after the `---` fence).
    #[serde(skip)]
    pub body: String,

    /// Absolute path to the skill directory (the parent of `SKILL.md`).
    #[serde(skip)]
    pub dir: PathBuf,

    /// `"project"` or `"user"` — where the skill was discovered.
    #[serde(skip)]
    pub source: SkillSource,
}

fn default_true() -> bool {
    true
}

impl Default for Skill {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            version: String::new(),
            author: String::new(),
            homepage: String::new(),
            license: String::new(),
            disable_model_invocation: false,
            user_invocable: true,
            allowed_tools: Vec::new(),
            arguments: Vec::new(),
            when_to_use: String::new(),
            body: String::new(),
            dir: PathBuf::new(),
            source: SkillSource::Project,
        }
    }
}

/// Where a skill was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillSource {
    #[default]
    Project,
    User,
}

impl SkillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillSource::Project => "project",
            SkillSource::User => "user",
        }
    }
}

// ---------------------------------------------------------------------------
// Custom deserializers
// ---------------------------------------------------------------------------

/// Accept `true`/`false`/`yes`/`no`/`on`/`off`/`1`/`0` case-insensitively.
fn deser_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoolVisitor;

    impl<'de> serde::de::Visitor<'de> for BoolVisitor {
        type Value = bool;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a boolean or a truthy string (yes/no, on/off, 1/0)")
        }

        fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<bool, E> {
            match v.to_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => Ok(true),
                "false" | "no" | "off" | "0" => Ok(false),
                other => Err(E::custom(format!("invalid truthy value: {other:?}"))),
            }
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<bool, E> {
            self.visit_str(&v)
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<bool, E> {
            Ok(v != 0)
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<bool, E> {
            Ok(v != 0)
        }

        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<bool, E> {
            Ok(v != 0.0)
        }
    }

    deserializer.deserialize_any(BoolVisitor)
}

/// Accept space-separated, comma-separated, and YAML list formats.
fn deser_allowed_tools<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ToolsVisitor;

    impl<'de> serde::de::Visitor<'de> for ToolsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or list of tool names")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(split_tool_list(v))
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Vec<String>, E> {
            Ok(split_tool_list(&v))
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Vec<String>, A::Error> {
            let mut tools = Vec::new();
            while let Some(elem) = seq.next_element::<String>()? {
                tools.push(elem);
            }
            Ok(tools)
        }
    }

    deserializer.deserialize_any(ToolsVisitor)
}

/// Split a string on whitespace and commas.
fn split_tool_list(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Error returned by skill-loading functions. Always non-fatal at the call
/// site — the caller logs a warning or returns an error string.
#[derive(Debug)]
pub enum SkillError {
    /// The directory does not contain a `SKILL.md`.
    Missing(String),
    /// The `SKILL.md` file could not be read.
    Io(std::io::Error),
    /// The YAML frontmatter could not be parsed.
    Yaml(String),
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::Missing(path) => write!(f, "no SKILL.md in {path}"),
            SkillError::Io(e) => write!(f, "I/O error reading SKILL.md: {e}"),
            SkillError::Yaml(msg) => write!(f, "YAML parse error in SKILL.md: {msg}"),
        }
    }
}

/// Parse a `SKILL.md` file.
///
/// If the opening `---` is not on line 1, the whole file is treated as body
/// with no metadata (all fields at defaults). If YAML doesn't parse, all
/// fields are loaded at defaults with a warning logged.
pub fn parse_skill_file(path: &Path, dir: &Path, source: SkillSource) -> Result<Skill, SkillError> {
    let content = std::fs::read_to_string(path).map_err(SkillError::Io)?;

    let mut skill = parse_skill_text(&content).unwrap_or_else(|e| {
        log::warn!("skills: {e} — loading with empty metadata");
        // Re-parse as body-only (the YAML block becomes part of the body).
        Skill {
            body: content.clone(),
            ..Default::default()
        }
    });

    skill.dir = dir.to_path_buf();
    skill.source = source;

    // Default name → parent directory name.
    if skill.name.is_empty() {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            skill.name = name.to_string();
        }
    }

    // Default description → first non-empty body line, truncated at 1,536.
    if skill.description.is_empty() {
        skill.description = first_body_line(&skill.body, 1536);
    }

    Ok(skill)
}

/// Parse skill text (the file contents) into a Skill, without path defaults.
fn parse_skill_text(text: &str) -> Result<Skill, SkillError> {
    // The opening `---` must be on the very first line (after an optional
    // BOM). Leading blank lines or whitespace → treat the whole file as body.
    let content = text.strip_prefix('\u{feff}').unwrap_or(text);

    // Frontmatter must start on line 1.
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        // No opening `---` on line 1 — treat entire file as body.
        return Ok(Skill {
            body: text.to_string(),
            ..Default::default()
        });
    }

    // Find the closing `---`.
    let after_open = &content[3..]; // skip "---"
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
    let after_open = after_open.strip_prefix("\r\n").unwrap_or(after_open);

    // Empty frontmatter: the closing `---` immediately follows the opening.
    if after_open.starts_with("---\n") || after_open.starts_with("---\r\n") || after_open == "---" {
        // Empty YAML block — parse with all defaults.
        let mut skill: Skill = Skill::default();
        let body_start = if after_open.as_bytes().get(3) == Some(&b'\n') {
            4
        } else if after_open.as_bytes().get(3) == Some(&b'\r')
            && after_open.as_bytes().get(4) == Some(&b'\n')
        {
            5
        } else {
            3
        };
        skill.body = after_open[body_start..].to_string();
        return Ok(skill);
    }

    let Some(close_idx) = find_yaml_close(after_open) else {
        // No closing `---` — treat entire file as body.
        return Ok(Skill {
            body: text.to_string(),
            ..Default::default()
        });
    };

    let yaml_block = &after_open[..close_idx];
    let body_start = close_idx + 3; // skip "---"
                                    // skip the newline after closing `---`
    let body_start = if after_open.as_bytes().get(body_start) == Some(&b'\n') {
        body_start + 1
    } else if after_open.as_bytes().get(body_start) == Some(&b'\r')
        && after_open.as_bytes().get(body_start + 1) == Some(&b'\n')
    {
        body_start + 2
    } else {
        body_start
    };
    let body = after_open[body_start..].to_string();

    let mut skill: Skill =
        serde_yaml::from_str(yaml_block).map_err(|e| SkillError::Yaml(e.to_string()))?;

    skill.body = body;

    Ok(skill)
}

/// Find the closing `---` on its own line.
fn find_yaml_close(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        // Look for \n---\n or \n--- at EOF.
        if bytes[i] == b'\n' {
            let line_start = i + 1;
            if line_start + 2 < len
                && bytes[line_start] == b'-'
                && bytes[line_start + 1] == b'-'
                && bytes[line_start + 2] == b'-'
            {
                let after = line_start + 3;
                if after >= len || bytes[after] == b'\n' || bytes[after] == b'\r' {
                    return Some(line_start);
                }
            }
        } else if i == 0
            && bytes[i] == b'-'
            && bytes.get(1) == Some(&b'-')
            && bytes.get(2) == Some(&b'-')
        {
            // The opening `---` was the entire first line.
            // Look for the closing `---` starting from byte 3.
            // Already handled above since we skip the opening.
        }
        i += 1;
    }
    None
}

/// First non-empty line of `body`, truncated to `max_len` characters.
fn first_body_line(body: &str, max_len: usize) -> String {
    for line in body.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            if trimmed.chars().count() <= max_len {
                return trimmed.to_string();
            }
            return trimmed.chars().take(max_len).collect();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// A loaded skill, ready for index rendering or prompt injection.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub skill: Skill,
    /// Files in the skill directory (relative paths), excluding `SKILL.md`.
    pub files: Vec<PathBuf>,
}

/// Discover all skills under `project_root/.gremlins/skills/` and
/// `~/.config/gremlins/skills/`. Project skills shadow user-global skills on name
/// collision.
pub fn discover(project_root: &Path) -> Vec<LoadedSkill> {
    let mut by_name: HashMap<String, LoadedSkill> = HashMap::new();

    // User-global skills first.
    let user_dir = crate::config::resolve_user_config_root(None).join("skills");
    for loaded in scan_skills_dir(&user_dir, SkillSource::User) {
        by_name.entry(loaded.skill.name.clone()).or_insert(loaded);
    }

    // Project skills second (shadow user-global).
    let project_dir = project_root.join(".gremlins").join("skills");
    for loaded in scan_skills_dir(&project_dir, SkillSource::Project) {
        by_name.insert(loaded.skill.name.clone(), loaded);
    }

    let mut skills: Vec<LoadedSkill> = by_name.into_values().collect();
    skills.sort_by(|a, b| a.skill.name.cmp(&b.skill.name));
    skills
}

/// Scan a single skills directory. Directories without `SKILL.md` are
/// silently skipped.
fn scan_skills_dir(root: &Path, source: SkillSource) -> Vec<LoadedSkill> {
    let mut skills = Vec::new();

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return skills,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }

        match parse_skill_file(&skill_md, &path, source) {
            Ok(skill) => {
                let files = list_skill_files(&path);
                skills.push(LoadedSkill { skill, files });
            }
            Err(e) => {
                log::warn!("skills: {e}");
            }
        }
    }

    skills
}

/// List bundled files in a skill directory recursively, excluding `SKILL.md`.
/// Subdirectory paths are preserved relative to the skill root and directory
/// symlinks are not followed (to avoid cycles).
fn list_skill_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_skill_files(dir, dir, &mut files);
    files.sort();
    files
}

fn collect_skill_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match path.symlink_metadata() {
            Ok(m) => m.file_type(),
            Err(_) => continue,
        };
        if ft.is_dir() {
            // Skip symlink directories to avoid cycles.
            if ft.is_symlink() {
                continue;
            }
            collect_skill_files(root, &path, out);
        } else if ft.is_file() && path.file_name().is_some_and(|n| n != "SKILL.md") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Index rendering
// ---------------------------------------------------------------------------

/// Render a skill index: one `name — description` line per skill, omitting
/// skills with `disable-model-invocation: true`, grouped by source.
pub fn render_index(skills: &[LoadedSkill]) -> String {
    let (project, user): (Vec<_>, Vec<_>) = skills
        .iter()
        .filter(|s| !s.skill.disable_model_invocation)
        .partition(|s| matches!(s.skill.source, SkillSource::Project));

    let mut out = String::new();

    if !project.is_empty() {
        out.push_str("## Project skills\n\n");
        for s in &project {
            out.push_str(&format!(
                "- **{}** — {}\n",
                s.skill.name, s.skill.description
            ));
        }
        out.push('\n');
    }

    if !user.is_empty() {
        out.push_str("## User-global skills\n\n");
        for s in &user {
            out.push_str(&format!(
                "- **{}** — {}\n",
                s.skill.name, s.skill.description
            ));
        }
        out.push('\n');
    }

    if project.is_empty() && user.is_empty() {
        out.push_str("(no skills available)\n");
    }

    out
}

/// Render the skill index for the operator's `/skills` command. Hides skills
/// with `user-invocable: false`.
pub fn render_operator_index(skills: &[LoadedSkill]) -> String {
    let (project, user): (Vec<_>, Vec<_>) = skills
        .iter()
        .filter(|s| s.skill.user_invocable)
        .partition(|s| matches!(s.skill.source, SkillSource::Project));

    let mut out = String::new();

    if !project.is_empty() {
        out.push_str("project skills:\n");
        for s in &project {
            out.push_str(&format!("  {} — {}\n", s.skill.name, s.skill.description));
        }
    }

    if !user.is_empty() {
        if !project.is_empty() {
            out.push('\n');
        }
        out.push_str("user-global skills:\n");
        for s in &user {
            out.push_str(&format!("  {} — {}\n", s.skill.name, s.skill.description));
        }
    }

    if project.is_empty() && user.is_empty() {
        out.push_str("(no skills available)\n");
    }

    out
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Find a skill by name from the discovered set. Returns `None` when the
/// skill is not found or when `user-invocable: false` is set and the caller
/// hasn't asked to bypass that check.
pub fn find_skill<'a>(
    skills: &'a [LoadedSkill],
    name: &str,
    allow_not_invocable: bool,
) -> Option<&'a LoadedSkill> {
    skills
        .iter()
        .find(|s| s.skill.name == name && (allow_not_invocable || s.skill.user_invocable))
}

// ---------------------------------------------------------------------------
// Variable substitution
// ---------------------------------------------------------------------------

/// Substitute `$ARGUMENTS`, `$0`–`$N`, `${GREMLIN_SKILL_DIR}`,
/// `${GREMLIN_PROJECT_DIR}`, `${GREMLIN_SESSION_ID}`, and named argument
/// placeholders in a skill body.
///
/// Backslash-escaped argument placeholders (`\$1`) are preserved as literal
/// text. If no placeholder in the body consumed the arguments, `ARGUMENTS: <value>`
/// is appended to the end of the body.
///
/// `positional` is already shell-split; `argument_text` is the raw string for
/// `$ARGUMENTS` expansion.
pub(crate) fn substitute_variables(
    body: &str,
    positional: &[String],
    argument_text: &str,
    named_args: &[String],
    skill_dir: &Path,
    project_root: &Path,
    session_id: &str,
) -> String {
    // Does the body contain any argument-like placeholder? Escaped
    // placeholders still count — the author wrote them on purpose, so the
    // trailing `ARGUMENTS:` line is only appended for bodies with no
    // placeholder token at all.
    static HAS_PLACEHOLDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\$(?:\d+|ARGUMENTS(?:\[\d+\])?|[A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let has_any_arg_placeholder = HAS_PLACEHOLDER.is_match(body);

    // Only protect `\$` when the next char starts an argument placeholder
    // (digit, letter/underscore — but not `{`, so `${GREMLIN_*}` is left
    // alone).  The sentinel is restored to `$` after substitution.
    static PROTECT_ESCAPE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\\\$([0-9A-Za-z_])").unwrap());
    let protected = PROTECT_ESCAPE.replace_all(body, "\u{0}$1");

    // A single pass, so replacement text is never re-scanned. Alternation
    // order matters: the longer `$ARGUMENTS[n]` form must precede bare
    // `$ARGUMENTS`. Capture groups: 1 = indexed, 2 = positional, 3 = named.
    static SUBST_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"\$\{GREMLIN_SKILL_DIR\}",
            r"|\$\{GREMLIN_PROJECT_DIR\}",
            r"|\$\{GREMLIN_SESSION_ID\}",
            r"|\$ARGUMENTS\[(\d+)\]",
            r"|\$ARGUMENTS\b",
            r"|\$(\d+)",
            r"|\$([A-Za-z_][A-Za-z0-9_]*)",
        ))
        .unwrap()
    });

    let mut result = SUBST_RE
        .replace_all(&protected, |caps: &regex::Captures| {
            let whole = &caps[0];
            match whole {
                "${GREMLIN_SKILL_DIR}" => return skill_dir.display().to_string(),
                "${GREMLIN_PROJECT_DIR}" => return project_root.display().to_string(),
                "${GREMLIN_SESSION_ID}" => return session_id.to_string(),
                _ => {}
            }
            if let Some(m) = caps.get(1) {
                // `$ARGUMENTS[n]` — indexed. Parse failure or out-of-range
                // index stays as literal text.
                match m.as_str().parse::<usize>() {
                    Ok(idx) => positional
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| whole.to_string()),
                    Err(_) => whole.to_string(),
                }
            } else if let Some(m) = caps.get(2) {
                // `$n` — positional. Out-of-range indices stay literal.
                let idx: usize = m.as_str().parse().unwrap();
                positional
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| whole.to_string())
            } else if let Some(m) = caps.get(3) {
                // `$name` — named. Only substitute when the name appears
                // in the skill's `arguments` frontmatter; undeclared
                // identifiers (e.g. `$HOME`, `$PATH`) stay literal.
                let name = m.as_str();
                named_args
                    .iter()
                    .position(|n| n == name)
                    .and_then(|i| positional.get(i).cloned())
                    .unwrap_or_else(|| whole.to_string())
            } else {
                // Bare `$ARGUMENTS` — the full argument text.
                argument_text.to_string()
            }
        })
        .to_string();

    // Restore escaped dollar signs: sentinel → `$`.
    result = result.replace('\u{0}', "$");

    // If no argument placeholder consumed the args, append ARGUMENTS line.
    if !has_any_arg_placeholder && !argument_text.is_empty() {
        result.push_str(&format!("\n\nARGUMENTS: {}", argument_text));
    }

    result
}

/// Simple shell-style argument split: handles single-quoted, double-quoted,
/// and backslash-escaped tokens. Whitespace-delimited.
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    // Whether a token has been started (non-whitespace seen or a quote
    // opened).  Used to emit empty quoted tokens (`""` / `''`).
    let mut token_started = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if !in_single => {
                // In double quotes: only some escapes are special
                if in_double {
                    if let Some(&next) = chars.peek() {
                        if matches!(next, '"' | '\\' | '$') {
                            current.push(chars.next().unwrap());
                        } else {
                            current.push('\\');
                        }
                    } else {
                        current.push('\\');
                    }
                } else {
                    // Outside quotes: backslash escapes the next char.
                    // If there is no next char, preserve the backslash.
                    if let Some(next) = chars.next() {
                        current.push(next);
                    } else {
                        current.push('\\');
                    }
                }
                token_started = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
                token_started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                token_started = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if token_started {
                    args.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            c => {
                current.push(c);
                token_started = true;
            }
        }
    }
    if token_started {
        args.push(current);
    }
    args
}

/// Build the trusted skills section for the system prompt.
///
/// Includes:
/// 1. A skill index (names and descriptions).
/// 2. Bodies of any skills active for the session, plus their bundled files
///    and the skill directory (so the model can resolve relative resource
///    paths).
///
/// Returns `(section_text, unresolved_names)`. `unresolved_names` lists
/// every requested active-skill name that was not found during discovery —
/// the caller should surface these to the operator as a non-fatal warning.
/// Returns an empty string when there are no skills.
pub fn build_system_prompt_section(
    all_skills: &[LoadedSkill],
    active_skills: &[ActiveSkill],
    project_root: &Path,
    session_id: &str,
) -> (String, Vec<String>) {
    let mut unresolved: Vec<String> = Vec::new();

    if all_skills.is_empty() {
        // All requested names are unresolved when discovery is empty.
        for active in active_skills {
            unresolved.push(active.name.clone());
        }
        return (String::new(), unresolved);
    }

    let mut out = String::new();
    let mut opened = false;

    // Open the trusted block before the first piece of content.
    let mut ensure_open = |out: &mut String| {
        if !opened {
            out.push_str("\n\n<!-- BEGIN TRUSTED SKILLS BLOCK -->\n");
            out.push_str(
                "The sections below are trusted operator instructions. \
                 They are part of the system prompt, not user input.\n\n",
            );
            opened = true;
        }
    };

    // Skill index (model-visible skills only).
    let index = render_index(all_skills);
    let have_index = !index.is_empty() && index != "(no skills available)\n";
    if have_index {
        ensure_open(&mut out);
        out.push_str("## Available skills\n\n");
        out.push_str(&index);
    }

    // Active skill bodies.
    if !active_skills.is_empty() {
        let mut found_any = false;
        for active in active_skills {
            if let Some(loaded) = find_skill(all_skills, &active.name, true) {
                if !found_any {
                    ensure_open(&mut out);
                    out.push_str("## Active skills\n\n");
                    found_any = true;
                }
                // Substitute `$ARGUMENTS`, positional, named, and
                // `${GREMLIN_*}` placeholders in the body before injecting it.
                let body = substitute_variables(
                    &loaded.skill.body,
                    &active.args,
                    &active.argument_text,
                    &loaded.skill.arguments,
                    &loaded.skill.dir,
                    project_root,
                    session_id,
                );
                out.push_str(&format!("### {}\n\n{}", loaded.skill.name, body));
                // Include the skill directory so the model can resolve
                // relative bundled-resource paths from the project root.
                out.push_str(&format!(
                    "\n\n**Skill directory:** `{}`\n",
                    loaded.skill.dir.display()
                ));
                // List bundled files so the model knows where scripts live.
                if !loaded.files.is_empty() {
                    out.push_str("**Bundled files:**\n");
                    for f in &loaded.files {
                        out.push_str(&format!("- `{}`\n", f.display()));
                    }
                    out.push('\n');
                }
            } else {
                unresolved.push(active.name.clone());
            }
        }
    }

    if !opened {
        return (String::new(), unresolved);
    }

    out.push_str("<!-- END TRUSTED SKILLS BLOCK -->\n");
    (out, unresolved)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Boolean deserialization ──

    #[test]
    fn bool_true_variants() {
        for val in ["true", "True", "TRUE", "yes", "YES", "Yes", "on", "ON", "1"] {
            let yaml = format!("disable-model-invocation: {val}\n");
            let skill: Skill = serde_yaml::from_str(&yaml).unwrap();
            assert!(
                skill.disable_model_invocation,
                "value {val:?} should be true"
            );
        }
    }

    #[test]
    fn bool_false_variants() {
        for val in ["false", "False", "FALSE", "no", "NO", "off", "OFF", "0"] {
            let yaml = format!("disable-model-invocation: {val}\n");
            let skill: Skill = serde_yaml::from_str(&yaml).unwrap();
            assert!(
                !skill.disable_model_invocation,
                "value {val:?} should be false"
            );
        }
    }

    #[test]
    fn bool_native_bool() {
        let yaml = "disable-model-invocation: true\n";
        let skill: Skill = serde_yaml::from_str(yaml).unwrap();
        assert!(skill.disable_model_invocation);
    }

    // ── allowed-tools deserialization ──

    #[test]
    fn allowed_tools_space_separated() {
        let yaml = "allowed-tools: Bash Read Grep\n";
        let skill: Skill = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(skill.allowed_tools, vec!["Bash", "Read", "Grep"]);
    }

    #[test]
    fn allowed_tools_comma_separated() {
        let yaml = "allowed-tools: Bash, Read, Grep\n";
        let skill: Skill = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(skill.allowed_tools, vec!["Bash", "Read", "Grep"]);
    }

    #[test]
    fn allowed_tools_yaml_list() {
        let yaml = "allowed-tools:\n  - Bash\n  - Read\n";
        let skill: Skill = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(skill.allowed_tools, vec!["Bash", "Read"]);
    }

    // ── Frontmatter parsing ──

    #[test]
    fn parse_with_frontmatter() {
        let text =
            "---\nname: my-skill\ndescription: does things\n---\n\n# Body\n\nInstructions here.\n";
        let skill = parse_skill_text(text).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "does things");
        assert_eq!(skill.body.trim(), "# Body\n\nInstructions here.");
    }

    #[test]
    fn parse_without_opening_dashes_is_body_only() {
        let text = "# No frontmatter\n\nJust body.\n";
        let skill = parse_skill_text(text).unwrap();
        assert_eq!(skill.name, "");
        assert_eq!(skill.description, "");
        assert_eq!(skill.body, text);
    }

    #[test]
    fn parse_without_closing_dashes_is_body_only() {
        let text = "---\nname: orphan\n";
        let skill = parse_skill_text(text).unwrap();
        assert_eq!(skill.name, "");
        assert_eq!(skill.body, text);
    }

    #[test]
    fn parse_empty_frontmatter() {
        let text = "---\n---\nbody\n";
        let skill = parse_skill_text(text).unwrap();
        assert_eq!(skill.name, "");
        assert_eq!(skill.body, "body\n");
    }

    #[test]
    fn parse_empty_frontmatter_with_crlf() {
        let text = "---\r\n---\r\nbody\r\n";
        let skill = parse_skill_text(text).unwrap();
        assert_eq!(skill.name, "");
        assert_eq!(skill.body, "body\r\n");
    }

    #[test]
    fn parse_leading_whitespace_is_body_only() {
        // Opening --- is not on line 1 → whole file is body.
        let text = "\n---\nname: test\n---\nbody\n";
        let skill = parse_skill_text(text).unwrap();
        assert_eq!(skill.name, "");
        assert_eq!(skill.body, text);
    }

    #[test]
    fn parse_bad_yaml_returns_error() {
        let text = "---\nname: [unclosed\n---\nbody\n";
        let result = parse_skill_text(text);
        assert!(result.is_err());
    }

    #[test]
    fn parse_frontmatter_with_unknown_fields_is_silent() {
        let text = "---\nname: test\nmodel: gpt-5\nargument-hint: 'some hint'\ncontext: some-context\n---\nbody\n";
        let skill = parse_skill_text(text).unwrap();
        assert_eq!(skill.name, "test");
        assert_eq!(skill.body, "body\n");
    }

    // ── Name / description defaults ──

    #[test]
    fn name_defaults_to_dir_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join("pdf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Just a body\n\nNo frontmatter.\n",
        )
        .unwrap();

        let skill = parse_skill_file(
            &skill_dir.join("SKILL.md"),
            &skill_dir,
            SkillSource::Project,
        )
        .unwrap();
        assert_eq!(skill.name, "pdf");
    }

    #[test]
    fn description_defaults_to_first_body_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "Convert PDF files to text.\n\nMore instructions.\n",
        )
        .unwrap();

        let skill = parse_skill_file(
            &skill_dir.join("SKILL.md"),
            &skill_dir,
            SkillSource::Project,
        )
        .unwrap();
        assert_eq!(skill.description, "Convert PDF files to text.");
    }

    #[test]
    fn description_truncated_at_1536() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let long_line = "A".repeat(2000);
        std::fs::write(skill_dir.join("SKILL.md"), &long_line).unwrap();

        let skill = parse_skill_file(
            &skill_dir.join("SKILL.md"),
            &skill_dir,
            SkillSource::Project,
        )
        .unwrap();
        assert_eq!(skill.description.len(), 1536);
    }

    // ── Index rendering ──

    #[test]
    fn render_index_omits_disable_model_invocation() {
        let skills = vec![
            LoadedSkill {
                skill: Skill {
                    name: "visible".into(),
                    description: "shown".into(),
                    ..Default::default()
                },
                files: vec![],
            },
            LoadedSkill {
                skill: Skill {
                    name: "hidden".into(),
                    description: "not shown".into(),
                    disable_model_invocation: true,
                    ..Default::default()
                },
                files: vec![],
            },
        ];
        let index = render_index(&skills);
        assert!(index.contains("visible"));
        assert!(!index.contains("hidden"));
    }

    #[test]
    fn render_operator_index_hides_not_user_invocable() {
        let skills = vec![
            LoadedSkill {
                skill: Skill {
                    name: "visible".into(),
                    description: "shown".into(),
                    source: SkillSource::Project,
                    ..Default::default()
                },
                files: vec![],
            },
            LoadedSkill {
                skill: Skill {
                    name: "hidden".into(),
                    description: "not shown".into(),
                    user_invocable: false,
                    source: SkillSource::Project,
                    ..Default::default()
                },
                files: vec![],
            },
        ];
        let index = render_operator_index(&skills);
        assert!(index.contains("visible"));
        assert!(!index.contains("hidden"));
    }

    // ── Discovery ──

    #[test]
    fn scan_skips_dirs_without_skill_md() {
        let tmp = tempfile::TempDir::new().unwrap();
        let empty_dir = tmp.path().join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let skills = scan_skills_dir(tmp.path(), SkillSource::Project);
        assert!(skills.is_empty());
    }

    #[test]
    fn discover_finds_skill() {
        let mut env = crate::test_support::EnvGuard::lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path().join("project");

        // Isolate HOME so user-global skills don't affect discovery.
        let empty_home = tmp.path().join("empty-home");
        std::fs::create_dir_all(&empty_home).unwrap();
        env.set("HOME", empty_home.to_string_lossy().as_ref());

        let skills_dir = project_root
            .join(".gremlins")
            .join("skills")
            .join("my-skill");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill\n---\n\nDo the thing.\n",
        )
        .unwrap();

        let loaded = discover(&project_root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].skill.name, "my-skill");
        assert_eq!(loaded[0].skill.body.trim(), "Do the thing.");
    }

    #[test]
    fn project_skills_shadow_user_skills() {
        let mut env = crate::test_support::EnvGuard::lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path().join("project");

        // Create a fake home with a user-global skill.
        let home = tmp.path().join("home");
        let user_skills = home
            .join(".config")
            .join("gremlins")
            .join("skills")
            .join("shared");
        std::fs::create_dir_all(&user_skills).unwrap();
        std::fs::write(
            user_skills.join("SKILL.md"),
            "---\nname: shared\ndescription: user version\n---\nuser body\n",
        )
        .unwrap();

        // Project skill with same name.
        let proj_skills = project_root.join(".gremlins").join("skills").join("shared");
        std::fs::create_dir_all(&proj_skills).unwrap();
        std::fs::write(
            proj_skills.join("SKILL.md"),
            "---\nname: shared\ndescription: project version\n---\nproject body\n",
        )
        .unwrap();

        // Override HOME for this test (EnvGuard restores on drop).
        env.set("HOME", home.to_string_lossy().as_ref());

        // Also create a user-only skill to confirm user-global discovery runs.
        let user_only = home
            .join(".config")
            .join("gremlins")
            .join("skills")
            .join("user-only");
        std::fs::create_dir_all(&user_only).unwrap();
        std::fs::write(
            user_only.join("SKILL.md"),
            "---\nname: user-only\ndescription: only in user\n---\nuser only body\n",
        )
        .unwrap();

        let loaded = discover(&project_root);
        let shared: Vec<_> = loaded.iter().filter(|s| s.skill.name == "shared").collect();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].skill.description, "project version");
        assert_eq!(shared[0].skill.body.trim(), "project body");
        assert!(matches!(shared[0].skill.source, SkillSource::Project));
        // User-only skill must be visible (proves user-global discovery ran).
        assert!(loaded.iter().any(|s| s.skill.name == "user-only"));
    }

    #[test]
    fn list_skill_files_excludes_skill_md() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: test\n---\nbody\n").unwrap();
        std::fs::write(dir.join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(dir.join("config.json"), "{}").unwrap();

        let files = list_skill_files(&dir);
        assert_eq!(files.len(), 2);
        assert!(files.contains(&PathBuf::from("script.sh")));
        assert!(files.contains(&PathBuf::from("config.json")));
        assert!(!files.contains(&PathBuf::from("SKILL.md")));
    }

    // ── System prompt section ──

    #[test]
    fn build_system_prompt_includes_index_and_active_skills() {
        let skills = vec![LoadedSkill {
            skill: Skill {
                name: "pdf".into(),
                description: "PDF tools".into(),
                body: "Use pdftotext.\n".into(),
                source: SkillSource::Project,
                ..Default::default()
            },
            files: vec![],
        }];

        let (section, unresolved) = build_system_prompt_section(
            &skills,
            &[ActiveSkill {
                name: "pdf".into(),
                args: vec![],
                argument_text: String::new(),
            }],
            Path::new("/tmp"),
            "sess-1",
        );
        assert!(section.contains("Available skills"));
        assert!(section.contains("**pdf** — PDF tools"));
        assert!(section.contains("Active skills"));
        assert!(section.contains("Use pdftotext."));
        assert!(section.contains("TRUSTED SKILLS BLOCK"));
        assert!(unresolved.is_empty());
    }

    #[test]
    fn build_system_prompt_empty_when_no_skills() {
        let (section, unresolved) =
            build_system_prompt_section(&[], &[], Path::new("/tmp"), "sess-1");
        assert!(section.is_empty());
        assert!(unresolved.is_empty());
    }

    #[test]
    fn find_skill_respects_user_invocable() {
        let skills = vec![LoadedSkill {
            skill: Skill {
                name: "ok".into(),
                user_invocable: true,
                ..Default::default()
            },
            files: vec![],
        }];
        assert!(find_skill(&skills, "ok", false).is_some());
        assert!(find_skill(&skills, "missing", false).is_none());
    }

    #[test]
    fn find_skill_bypasses_invocable_check() {
        let skills = vec![LoadedSkill {
            skill: Skill {
                name: "hidden".into(),
                user_invocable: false,
                ..Default::default()
            },
            files: vec![],
        }];
        // Operator view rejects it.
        assert!(find_skill(&skills, "hidden", false).is_none());
        // Internal (prompt builder) accepts it.
        assert!(find_skill(&skills, "hidden", true).is_some());
    }

    // ── Split tool list ──

    #[test]
    fn build_system_prompt_reports_unresolved_names() {
        let skills = vec![LoadedSkill {
            skill: Skill {
                name: "present".into(),
                description: "Here".into(),
                body: "body".into(),
                source: SkillSource::Project,
                ..Default::default()
            },
            files: vec![],
        }];
        let active = vec![
            ActiveSkill {
                name: "present".to_string(),
                args: vec![],
                argument_text: String::new(),
            },
            ActiveSkill {
                name: "gone".to_string(),
                args: vec![],
                argument_text: String::new(),
            },
        ];
        let (section, unresolved) =
            build_system_prompt_section(&skills, &active, Path::new("/tmp"), "sess-1");
        assert_eq!(unresolved, vec!["gone"]);
        assert!(section.contains("present"));
        assert!(!section.contains("gone"));
    }

    #[test]
    fn build_system_prompt_includes_skill_directory() {
        let skills = vec![LoadedSkill {
            skill: Skill {
                name: "tool".into(),
                description: "desc".into(),
                body: "body".into(),
                dir: PathBuf::from("/home/user/.config/gremlins/skills/tool"),
                source: SkillSource::User,
                ..Default::default()
            },
            files: vec![PathBuf::from("script.sh")],
        }];
        let (section, _) = build_system_prompt_section(
            &skills,
            &[ActiveSkill {
                name: "tool".into(),
                args: vec![],
                argument_text: String::new(),
            }],
            Path::new("/tmp"),
            "sess-1",
        );
        assert!(section.contains("Skill directory:"));
        assert!(section.contains("/home/user/.config/gremlins/skills/tool"));
        assert!(section.contains("script.sh"));
    }

    #[test]
    fn list_skill_files_traverses_subdirectories() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("skill");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: test\n---\nbody\n").unwrap();
        std::fs::write(dir.join("README.md"), "readme").unwrap();
        std::fs::write(dir.join("scripts").join("run.py"), "print('hi')").unwrap();
        std::fs::write(dir.join("scripts").join("helper.sh"), "#!/bin/sh").unwrap();

        let files = list_skill_files(&dir);
        // Top-level file.
        assert!(files.contains(&PathBuf::from("README.md")));
        // Nested files with relative paths.
        assert!(files.contains(&PathBuf::from("scripts/run.py")));
        assert!(files.contains(&PathBuf::from("scripts/helper.sh")));
        // SKILL.md excluded.
        assert!(!files.contains(&PathBuf::from("SKILL.md")));
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn split_tool_list_handles_mixed_separators() {
        assert_eq!(
            split_tool_list("Bash Read, Grep"),
            vec!["Bash", "Read", "Grep"]
        );
        assert_eq!(split_tool_list(""), Vec::<String>::new());
        assert_eq!(split_tool_list("  ,  "), Vec::<String>::new());
    }

    // ── Variable substitution ──

    #[test]
    fn substitute_positional_args() {
        let body = "Run $0 with $1";
        let result = substitute_variables(
            body,
            &["alpha".to_string(), "beta".to_string()],
            "alpha beta",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Run alpha with beta");
    }

    #[test]
    fn substitute_named_args() {
        let body = "Fix $issue on $branch";
        let named = vec!["issue".to_string(), "branch".to_string()];
        let result = substitute_variables(
            body,
            &["BUG-42".to_string(), "main".to_string()],
            "BUG-42 main",
            &named,
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Fix BUG-42 on main");
    }

    #[test]
    fn substitute_arguments_full() {
        let body = "Args: $ARGUMENTS";
        let result = substitute_variables(
            body,
            &["one".to_string(), "two".to_string(), "three".to_string()],
            "one two three",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Args: one two three");
    }

    #[test]
    fn substitute_arguments_indexed() {
        let body = "Second: $ARGUMENTS[1]";
        let result = substitute_variables(
            body,
            &[
                "first".to_string(),
                "second".to_string(),
                "third".to_string(),
            ],
            "first second third",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Second: second");
    }

    #[test]
    fn substitute_gremlin_skill_dir() {
        let body = "Dir: ${GREMLIN_SKILL_DIR}";
        let result = substitute_variables(
            body,
            &[],
            "",
            &[],
            Path::new("/home/user/skills/my-skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Dir: /home/user/skills/my-skill");
    }

    #[test]
    fn substitute_gremlin_project_dir() {
        let body = "Project: ${GREMLIN_PROJECT_DIR}";
        let result = substitute_variables(
            body,
            &[],
            "",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/my/project"),
            "sess-1",
        );
        assert_eq!(result, "Project: /my/project");
    }

    #[test]
    fn substitute_session_id() {
        let body = "Session: ${GREMLIN_SESSION_ID}";
        let result = substitute_variables(
            body,
            &[],
            "",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "abc-123",
        );
        assert_eq!(result, "Session: abc-123");
    }

    #[test]
    fn backslash_escapes_preserved() {
        let body = r"Literal \$1 and \$ARGUMENTS";
        let result = substitute_variables(
            body,
            &["hello".to_string()],
            "hello",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Literal $1 and $ARGUMENTS");
    }

    #[test]
    fn missing_indexed_arg_stays_literal() {
        let body = "Got $0 and $1";
        let result = substitute_variables(
            body,
            &["only-one".to_string()],
            "only-one",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Got only-one and $1");
    }

    #[test]
    fn missing_named_arg_expands_to_empty() {
        let body = "Name: $name, Missing: $gone";
        let named = vec!["name".to_string()];
        let result = substitute_variables(
            body,
            &["value".to_string()],
            "value",
            &named,
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Name: value, Missing: $gone");
    }

    #[test]
    fn no_placeholder_appends_arguments_line() {
        let body = "Just a static body.";
        let result = substitute_variables(
            body,
            &["some".to_string(), "args".to_string(), "here".to_string()],
            "some args here",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert!(result.contains("ARGUMENTS: some args here"));
        assert!(result.starts_with("Just a static body."));
    }

    #[test]
    fn no_args_no_appended_line() {
        let body = "Static body.";
        let result = substitute_variables(
            body,
            &[],
            "",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Static body.");
    }

    #[test]
    fn shell_split_respects_quoting() {
        let body = "$0 $1";
        let result = substitute_variables(
            body,
            &["hello world".to_string(), "single quoted".to_string()],
            "\"hello world\" 'single quoted'",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "hello world single quoted");
    }

    #[test]
    fn undeclared_name_stays_literal() {
        // `$HOME` and `$PATH` are not declared in `arguments` frontmatter,
        // so they stay as literal text rather than expanding to empty.
        let body = "Env $HOME and $PATH";
        let named = vec!["issue".to_string()];
        let result = substitute_variables(
            body,
            &["bug-1".to_string()],
            "bug-1",
            &named,
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Env $HOME and $PATH");
    }

    #[test]
    fn arguments_indexed_out_of_range_stays_literal() {
        let body = "Got $ARGUMENTS[5]";
        let result = substitute_variables(
            body,
            &["a".to_string(), "b".to_string()],
            "a b",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert_eq!(result, "Got $ARGUMENTS[5]");
    }

    #[test]
    fn arguments_indexed_bad_parse_stays_literal() {
        let body = "Got $ARGUMENTS[99999999999999999999]";
        let result = substitute_variables(
            body,
            &[],
            "",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/project"),
            "sess-1",
        );
        assert!(result.contains("$ARGUMENTS[99999999999999999999]"));
    }

    #[test]
    fn backslash_before_gremlin_vars_still_expands() {
        // `\$` only protects argument placeholders, not `${GREMLIN_*}`.
        let body = r"Path: \${GREMLIN_PROJECT_DIR}";
        let result = substitute_variables(
            body,
            &[],
            "",
            &[],
            Path::new("/tmp/skill"),
            Path::new("/my/project"),
            "sess-1",
        );
        // The backslash is just a literal char, `${GREMLIN_PROJECT_DIR}`
        // still expands.
        assert_eq!(result, "Path: \\/my/project");
    }

    // ── split_args ──

    #[test]
    fn split_args_empty_quoted_tokens() {
        assert_eq!(split_args(r#""a" "" "b""#), vec!["a", "", "b"]);
        assert_eq!(split_args("a '' b"), vec!["a", "", "b"]);
    }

    #[test]
    fn split_args_trailing_backslash() {
        assert_eq!(split_args(r#"a\"#), vec!["a\\"]);
    }

    // ── build_system_prompt_section integration ──

    #[test]
    fn build_section_preserves_quoted_args() {
        // Quoted arguments from LoadSkill survive the trip through
        // ActiveSkill → build_system_prompt_section → substitute_variables
        // without re-splitting.
        let skills = vec![LoadedSkill {
            skill: Skill {
                name: "greet".into(),
                description: "Greeter".into(),
                body: "Hello $0, your dir is $1".into(),
                source: SkillSource::Project,
                ..Default::default()
            },
            files: vec![],
        }];
        let active = vec![ActiveSkill {
            name: "greet".into(),
            args: vec!["World".to_string(), "/home/world".to_string()],
            argument_text: "World /home/world".into(),
        }];
        let (section, _) =
            build_system_prompt_section(&skills, &active, Path::new("/project"), "s-1");
        assert!(section.contains("Hello World, your dir is /home/world"));
    }
}
