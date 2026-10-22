//! Operator-defined chat skills (Claude Code `SKILL.md` format).
//!
//! Skills are markdown files with YAML frontmatter, placed under
//! `.gremlins/skills/<name>/SKILL.md` (project) or
//! `~/.gremlins/skills/<name>/SKILL.md` (user-global). Project skills shadow
//! user-global skills on name collision.
//!
//! The on-disk format is the Claude Code `SKILL.md` spec: a `---`-delimited
//! YAML block followed by a markdown body. Every field uses `#[serde(default)]`
//! so unknown keys are silently dropped for Claude Code compatibility.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

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
/// `~/.gremlins/skills/`. Project skills shadow user-global skills on name
/// collision.
pub fn discover(project_root: &Path) -> Vec<LoadedSkill> {
    let mut by_name: HashMap<String, LoadedSkill> = HashMap::new();

    // User-global skills first.
    if let Some(home) = dirs::home_dir() {
        let user_dir = home.join(".gremlins").join("skills");
        for loaded in scan_skills_dir(&user_dir, SkillSource::User) {
            by_name.entry(loaded.skill.name.clone()).or_insert(loaded);
        }
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

/// List bundled files in a skill directory, excluding `SKILL.md`.
fn list_skill_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return files,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|n| n != "SKILL.md") {
            if let Ok(rel) = path.strip_prefix(dir) {
                files.push(rel.to_path_buf());
            }
        }
    }
    files.sort();
    files
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

/// Build the trusted skills section for the system prompt.
///
/// Includes:
/// 1. A skill index (names and descriptions).
/// 2. Bodies of any skills active for the session, plus their bundled files.
///
/// Returns an empty string when there are no skills.
pub fn build_system_prompt_section(all_skills: &[LoadedSkill], active_names: &[String]) -> String {
    if all_skills.is_empty() {
        return String::new();
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
    if !active_names.is_empty() {
        let mut found_any = false;
        for name in active_names {
            if let Some(loaded) = find_skill(all_skills, name, true) {
                if !found_any {
                    ensure_open(&mut out);
                    out.push_str("## Active skills\n\n");
                    found_any = true;
                }
                out.push_str(&format!(
                    "### {}\n\n{}\n",
                    loaded.skill.name, loaded.skill.body
                ));
                // List bundled files so the model knows where scripts live.
                if !loaded.files.is_empty() {
                    out.push_str("**Bundled files:**\n");
                    for f in &loaded.files {
                        out.push_str(&format!("- `{}`\n", f.display()));
                    }
                    out.push('\n');
                }
            }
        }
    }

    if !opened {
        return String::new();
    }

    out.push_str("<!-- END TRUSTED SKILLS BLOCK -->\n");
    out
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
        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
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
        let user_skills = home.join(".gremlins").join("skills").join("shared");
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
        let user_only = home.join(".gremlins").join("skills").join("user-only");
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

        let section = build_system_prompt_section(&skills, &["pdf".to_string()]);
        assert!(section.contains("Available skills"));
        assert!(section.contains("**pdf** — PDF tools"));
        assert!(section.contains("Active skills"));
        assert!(section.contains("Use pdftotext."));
        assert!(section.contains("TRUSTED SKILLS BLOCK"));
    }

    #[test]
    fn build_system_prompt_empty_when_no_skills() {
        let section = build_system_prompt_section(&[], &[]);
        assert!(section.is_empty());
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
    fn split_tool_list_handles_mixed_separators() {
        assert_eq!(
            split_tool_list("Bash Read, Grep"),
            vec!["Bash", "Read", "Grep"]
        );
        assert_eq!(split_tool_list(""), Vec::<String>::new());
        assert_eq!(split_tool_list("  ,  "), Vec::<String>::new());
    }
}
