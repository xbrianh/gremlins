use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::definition::r#static::prompts;
use crate::definition::r#static::resolve::BuiltinResolver;
use crate::schemas::error::SchemaError;

/// Trait for resolving gremlin definition names to file paths.
/// The built-in implementation looks up gremlin definitions by name;
/// callers can supply custom resolution logic (e.g. from a registry).
pub(crate) trait DefinitionResolver {
    fn resolve(&self, name: &str, project_root: &std::path::Path) -> Result<PathBuf, SchemaError>;
}

pub(crate) fn load_json_file(path: &Path) -> Result<serde_json::Value, SchemaError> {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SchemaError::DefinitionFileNotFound {
            path: path.display().to_string(),
        },
        _ => SchemaError::Generic(format!("could not read {}: {}", path.display(), e)),
    })?;
    // Reject JSON5 numeric literals that serde_json::Value cannot represent
    // or that json5 silently coerces (NaN, Infinity, hex, leading +).
    reject_json5_numeric_literals(&text, path)?;
    let parsed: serde_json::Value = json5::from_str(&text).map_err(|e| SchemaError::YamlParse {
        label: path.display().to_string(),
        msg: e.to_string(),
    })?;
    if !parsed.is_object() {
        return Err(SchemaError::YamlNotMapping {
            label: path.display().to_string(),
            got: format!("{:?}", parsed),
        });
    }
    Ok(parsed)
}

/// Reject JSON5 numeric-literals that `serde_json::Value` cannot represent
/// (`NaN`, `Infinity`, `-Infinity`) or that the `json5` crate silently coerces
/// into plain numbers (hex `0x…`, leading `+`).
///
/// JSON5 is scanned with enough structure to avoid false positives in strings,
/// comments, and unquoted property names: string/comment contents are replaced
/// with spaces first, then value tokens are inspected. Signed decimal
/// exponents (e.g. `1e+3`) are preserved because the `+` is inside the number
/// token rather than at its start.
pub(crate) fn reject_json5_numeric_literals(text: &str, path: &Path) -> Result<(), SchemaError> {
    let cleaned = strip_json5_strings_and_comments(text);
    let bytes = cleaned.as_bytes();
    let len = bytes.len();

    let mut stack: Vec<Container> = Vec::new();
    // At the top level the document must begin with a value. Inside an object
    // the next token is a key; inside an array (or after a colon) the next
    // token is a value.
    let mut expect_value = true;
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'{' => {
                stack.push(Container::Object);
                expect_value = false;
                i += 1;
            }
            b'}' => {
                stack.pop();
                expect_value = false;
                i += 1;
            }
            b'[' => {
                stack.push(Container::Array);
                expect_value = true;
                i += 1;
            }
            b']' => {
                stack.pop();
                expect_value = false;
                i += 1;
            }
            b':' => {
                expect_value = true;
                i += 1;
            }
            b',' => {
                expect_value = matches!(stack.last(), Some(Container::Array));
                i += 1;
            }
            _ => {
                let end = token_end(&cleaned, i);
                if expect_value {
                    reject_problematic_value_token(&bytes[i..end], path)?;
                }
                i = end;
                expect_value = false;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Container {
    Object,
    Array,
}

/// Replace string and comment contents with spaces so that `NaN`, `0xFF`, and
/// leading-plus patterns inside quoted prompts and comments cannot trip the
/// numeric-literal check. Newlines inside comments are preserved so structure
/// scanning still sees them as whitespace (which it always did).
fn strip_json5_strings_and_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                let quote = bytes[i];
                out.push(b' ');
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        out.push(b' ');
                        i += 1;
                        if i < bytes.len() {
                            out.push(b' ');
                            i += 1;
                        }
                        continue;
                    }
                    if bytes[i] == quote {
                        out.push(b' ');
                        i += 1;
                        break;
                    }
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                out.push(b' ');
                out.push(b' ');
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    if bytes[i] == b'\n' {
                        out.push(b'\n');
                    } else {
                        out.push(b' ');
                    }
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    out.push(b' ');
                    out.push(b' ');
                    i += 2;
                } else {
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .expect("replacing JSON5 string/comment bytes with spaces preserves UTF-8")
}

/// Return the end index (exclusive) of the non-whitespace, non-delimiter token
/// starting at `start`. JSON structure delimiters and whitespace terminate a
/// token; everything else — digits, letters, `+`, `-`, `.`, and so on — is
/// part of it.
fn token_end(cleaned: &str, start: usize) -> usize {
    let bytes = cleaned.as_bytes();
    let mut end = start;
    while end < bytes.len()
        && !matches!(
            bytes[end],
            b'{' | b'}' | b'[' | b']' | b':' | b',' | b' ' | b'\t' | b'\r' | b'\n'
        )
    {
        end += 1;
    }
    end
}

/// Inspect a value token from the stripped JSON5 text and reject the numeric
/// literal forms that `serde_json::Value` cannot represent or that `json5`
/// silently coerces.
fn reject_problematic_value_token(token: &[u8], path: &Path) -> Result<(), SchemaError> {
    if token.is_empty() {
        return Ok(());
    }

    let mut digits_start = 0;
    if token[0] == b'-' || token[0] == b'+' {
        if token.get(1).is_some_and(|c| c.is_ascii_digit()) && token[0] == b'+' {
            return Err(numeric_literal_error(
                path,
                token,
                "leading-plus numeric literal",
            ));
        }
        digits_start = 1;
    }

    if digits_start + 1 < token.len()
        && token[digits_start] == b'0'
        && matches!(token[digits_start + 1], b'x' | b'X')
    {
        return Err(numeric_literal_error(
            path,
            token,
            "hexadecimal numeric literal",
        ));
    }

    let word = String::from_utf8_lossy(token);
    if matches!(word.as_ref(), "NaN" | "Infinity" | "-Infinity") {
        return Err(numeric_literal_error(
            path,
            token,
            "non-finite numeric literal",
        ));
    }
    Ok(())
}

fn numeric_literal_error(path: &Path, token: &[u8], kind: &str) -> SchemaError {
    SchemaError::YamlParse {
        label: path.display().to_string(),
        msg: format!(
            "JSON5 {kind} {} is not supported; use a plain decimal number instead",
            String::from_utf8_lossy(token)
        ),
    }
}

pub(crate) fn resolve_prompt_dir(
    value: Option<&serde_json::Value>,
    def_dir: &std::path::Path,
) -> Result<PathBuf, SchemaError> {
    match value {
        None => Ok(PathBuf::from(def_dir)),
        Some(v) => {
            if let Some(s) = v.as_str() {
                let p = PathBuf::from(s);
                if p.is_absolute() {
                    Ok(p)
                } else {
                    Ok(def_dir.join(&p))
                }
            } else {
                Err(SchemaError::Generic(format!(
                    "prompt_dir must be a string, got {:?}",
                    v
                )))
            }
        }
    }
}

pub(crate) fn project_stage_def_dir(_project_root: &Path, overlay_dir: &Path) -> PathBuf {
    overlay_dir.join("stages")
}

pub(crate) fn stage_definition_dirs_with_project(
    project_root: &Path,
    overlay_dir: &Path,
) -> Vec<PathBuf> {
    vec![project_stage_def_dir(project_root, overlay_dir)]
}

pub(crate) fn load_stage_def_from_dirs(
    name: &str,
    project_root: Option<&Path>,
    overlay_dir: Option<&Path>,
) -> Result<Option<serde_json::Value>, SchemaError> {
    let dirs: Vec<PathBuf> = if let (Some(pr), Some(od)) = (project_root, overlay_dir) {
        stage_definition_dirs_with_project(pr, od)
    } else if let Some(od) = overlay_dir {
        vec![od.join("stages")]
    } else {
        crate::config::stage_definition_dirs()
    };
    for d in &dirs {
        let candidate = d.join(format!("{}.json5", name));
        if candidate.exists() {
            return match load_json_file(&candidate) {
                Ok(recipe) => Ok(Some(recipe)),
                Err(e) => Err(e),
            };
        }
    }
    Ok(None)
}

pub(crate) fn parse_stage_definitions(
    raw: Option<&serde_json::Value>,
    project_root: Option<&PathBuf>,
    overlay_dir: &Path,
) -> Result<HashMap<String, serde_json::Value>, SchemaError> {
    let mut defs: HashMap<String, serde_json::Value> = HashMap::new();
    match raw {
        None => {}
        Some(v) if v.is_object() => {
            let mapping = v.as_object().unwrap();
            for (k, v) in mapping {
                let name = k.as_str().to_string();
                if name.is_empty() {
                    continue;
                }
                if let Some(s) = v.as_str() {
                    match load_stage_def_from_dirs(
                        s,
                        project_root.map(|p| p.as_path()),
                        Some(overlay_dir),
                    )? {
                        Some(recipe) => {
                            defs.insert(name.clone(), recipe);
                        }
                        None => {
                            return Err(SchemaError::StageDef {
                                name: name.clone(),
                                msg: format!("must be a dict or file under stages/; tried {s:?}"),
                            });
                        }
                    }
                } else if v.is_object() {
                    defs.insert(name, v.clone());
                } else {
                    return Err(SchemaError::StageDef {
                        name: name.clone(),
                        msg: "must be a dict or gremlins: reference".to_string(),
                    });
                }
            }
        }
        Some(v) => {
            return Err(SchemaError::Generic(format!(
                "stage-definitions must be a mapping, got {:?}",
                v
            )));
        }
    }
    Ok(defs)
}

pub(crate) fn substitute_recipe(
    node: &serde_json::Value,
    ctx: &serde_json::Value,
) -> Result<serde_json::Value, SchemaError> {
    match node {
        serde_json::Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, v) in m {
                out.insert(k.clone(), substitute_recipe(v, ctx)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        serde_json::Value::Array(seq) => {
            let mut out: Vec<serde_json::Value> = Vec::new();
            for item in seq {
                if let serde_json::Value::String(s) = item {
                    if s.starts_with("{{") && s.ends_with("}}") && s.matches("{{").count() == 1 {
                        let key = s[2..s.len() - 2].trim();
                        match resolve_placeholder(key, ctx) {
                            Ok(resolved) => {
                                if let serde_json::Value::Array(resolved_seq) = resolved {
                                    out.extend(resolved_seq);
                                } else {
                                    out.push(resolved);
                                }
                                continue;
                            }
                            Err(e) => {
                                return Err(SchemaError::Generic(e));
                            }
                        }
                    }
                }
                out.push(substitute_recipe(item, ctx)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        serde_json::Value::String(s) => {
            if s.starts_with("{{") && s.ends_with("}}") && s.matches("{{").count() == 1 {
                let key = s[2..s.len() - 2].trim();
                match resolve_placeholder(key, ctx) {
                    Ok(resolved) => Ok(resolved),
                    Err(e) => Err(SchemaError::Generic(e)),
                }
            } else if s.contains("{{") {
                static INLINE_RE: LazyLock<regex::Regex> =
                    LazyLock::new(|| regex::Regex::new(r"\{\{([^}]+)\}\}").unwrap());
                let result = INLINE_RE.replace_all(s, |caps: &regex::Captures| {
                    let key = caps[1].trim();
                    match resolve_placeholder(key, ctx) {
                        Ok(serde_json::Value::Array(seq)) => seq
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(" && "),
                        Ok(val) => val_to_string(&val),
                        Err(_) => caps[0].to_string(),
                    }
                });
                Ok(serde_json::Value::String(result.into_owned()))
            } else {
                Ok(node.clone())
            }
        }
        _ => Ok(node.clone()),
    }
}

fn val_to_string(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => format!("{other:?}"),
    }
}

pub(crate) fn resolve_placeholder(
    key: &str,
    ctx: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (dotted_key, has_default, default_val) = if let Some(idx) = key.find(" | default(") {
        let raw_default = &key[idx + " | default(".len()..];
        let raw_default = raw_default.strip_suffix(')').unwrap_or(raw_default);
        let default = parse_default(raw_default);
        (key[..idx].trim(), true, default)
    } else {
        (key.trim(), false, serde_json::Value::Null)
    };

    let parts: Vec<&str> = dotted_key.split('.').collect();
    let mut val = ctx;
    for part in &parts {
        match val.as_object().and_then(|m| m.get(*part)) {
            Some(v) => val = v,
            None => {
                if has_default {
                    return Ok(default_val);
                }
                return Err(format!(
                    "placeholder {{{{{dotted_key}}}}}: key {part:?} not found in context"
                ));
            }
        }
    }

    match val {
        serde_json::Value::Object(_)
        | serde_json::Value::Array(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Bool(_)
        | serde_json::Value::Null => Ok(val.clone()),
        serde_json::Value::String(s) => Ok(serde_json::Value::String(s.clone())),
    }
}

pub(crate) fn parse_default(raw: &str) -> serde_json::Value {
    let s = raw.trim();
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if first == last && (first == '"' || first == '\'') {
            return serde_json::Value::String(s[1..s.len() - 1].to_string());
        }
    }
    // Try integer first, then float, then fall back to string.
    if let Ok(n) = s.parse::<i64>() {
        return serde_json::Value::Number(serde_json::Number::from(n));
    }
    if let Ok(n) = s.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return serde_json::Value::Number(num);
        }
    }
    serde_json::Value::String(s.to_string())
}

/// Check whether a key appears in the stage's text as `{KEY}` (not `${KEY}`).
/// The runtime normalizes hyphens to underscores (and vice versa) during
/// substitution, so e.g. `{child-plan}` matches a key declared as `child_plan`.
pub(crate) fn key_referenced_in_text(key_str: &str, text: &str) -> bool {
    let mut targets = Vec::with_capacity(2);
    targets.push(format!("{{{key_str}}}"));
    if key_str.contains('-') {
        targets.push(format!("{{{}}}", key_str.replace('-', "_")));
    } else if key_str.contains('_') {
        targets.push(format!("{{{}}}", key_str.replace('_', "-")));
    }

    for target in &targets {
        let mut pos = 0;
        while let Some(idx) = text[pos..].find(target) {
            let abs_idx = pos + idx;
            // Must not be preceded by `$`
            if abs_idx == 0 || text.as_bytes().get(abs_idx - 1) != Some(&b'$') {
                return true;
            }
            pos = abs_idx + target.len();
        }
    }
    false
}

/// Parse a gremlin definition YAML file from disk, expanding includes, stage-definitions,
/// and prompts. Returns the fully expanded YAML tree.
pub(crate) fn parse_definition_file(
    yaml_path: &Path,
    project_root: &Path,
    overlay_dir: &Path,
) -> Result<serde_json::Value, SchemaError> {
    let resolver = BuiltinResolver;
    expand_definition(yaml_path, Some(project_root), overlay_dir, &resolver)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_definition(
    yaml_path: &Path,
    project_root: Option<&Path>,
    overlay_dir: &Path,
    resolver: &dyn DefinitionResolver,
) -> Result<serde_json::Value, SchemaError> {
    let project_root = project_root.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        let parent = yaml_path.parent().unwrap_or(yaml_path);
        if parent.file_name().is_some_and(|n| n == ".gremlins") {
            parent
                .parent()
                .map(PathBuf::from)
                .unwrap_or(PathBuf::from("."))
        } else {
            PathBuf::from(parent)
        }
    });

    let chain: Vec<PathBuf> = Vec::new();
    _expand(yaml_path, &project_root, overlay_dir, &chain, resolver)
}

#[allow(clippy::too_many_arguments)]
fn _expand(
    yaml_path: &Path,
    project_root: &PathBuf,
    overlay_dir: &Path,
    chain: &[PathBuf],
    resolver: &dyn DefinitionResolver,
) -> Result<serde_json::Value, SchemaError> {
    let resolved = yaml_path
        .canonicalize()
        .unwrap_or_else(|_| yaml_path.to_path_buf());
    if chain.contains(&resolved) {
        let mut cycle_parts: Vec<String> = chain.iter().map(|p| p.display().to_string()).collect();
        cycle_parts.push(resolved.display().to_string());
        return Err(SchemaError::IncludeCycle(cycle_parts.join(" -> ")));
    }

    let raw = load_json_file(yaml_path)?;
    let raw_mapping = raw.as_object().unwrap();

    if raw_mapping
        .get("__gremlins_expanded__")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let mut result = raw.clone();
        if let Some(m) = result.as_object_mut() {
            m.remove("__gremlins_expanded__");
        }
        return Ok(result);
    }

    let def_dir = yaml_path.parent().unwrap_or(yaml_path);

    let prompt_dir = resolve_prompt_dir(raw_mapping.get("prompt_dir"), def_dir)?;

    let new_chain: Vec<PathBuf> = chain
        .iter()
        .chain(std::iter::once(&resolved))
        .cloned()
        .collect();

    let named_prompts = prompts::parse_named_prompts(raw_mapping.get("prompts"), &prompt_dir)?;

    let stage_defs = parse_stage_definitions(
        raw_mapping.get("stage-definitions"),
        Some(project_root),
        overlay_dir,
    )?;

    let stages_raw = raw_mapping.get("stages");
    let stages_list: Vec<serde_json::Value> = match stages_raw {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(v) if v.is_array() => v.as_array().cloned().unwrap_or_default(),
        Some(_v) => {
            return Err(SchemaError::Generic("'stages' must be a list".to_string()));
        }
    };

    let mut expanded_stages: Vec<serde_json::Value> = Vec::new();
    for entry in stages_list {
        let expanded = _expand_entry(
            &entry,
            &prompt_dir,
            project_root,
            overlay_dir,
            &new_chain,
            &named_prompts,
            &stage_defs,
            &HashSet::new(),
            resolver,
        )?;
        expanded_stages.extend(expanded);
    }

    let mut result = serde_json::Map::new();
    for (k, v) in raw_mapping {
        let key_str = k.as_str();
        if key_str == "stages"
            || key_str == "prompt_dir"
            || key_str == "prompts"
            || key_str == "stage-definitions"
        {
            continue;
        }
        result.insert(k.clone(), v.clone());
    }
    result.insert(
        "stages".to_string(),
        serde_json::Value::Array(expanded_stages),
    );
    result.insert(
        "__gremlins_expanded__".to_string(),
        serde_json::Value::Bool(true),
    );

    Ok(serde_json::Value::Object(result))
}

#[allow(clippy::too_many_arguments)]
fn _expand_entry(
    entry: &serde_json::Value,
    prompt_dir: &PathBuf,
    project_root: &PathBuf,
    overlay_dir: &Path,
    chain: &[PathBuf],
    named_prompts: &HashMap<String, Vec<String>>,
    stage_defs: &HashMap<String, serde_json::Value>,
    seen_defs: &HashSet<String>,
    resolver: &dyn DefinitionResolver,
) -> Result<Vec<serde_json::Value>, SchemaError> {
    let mapping = match entry.as_object() {
        Some(m) => m,
        None => return Ok(vec![entry.clone()]),
    };

    // include: single-key entry
    if mapping.len() == 1 && mapping.contains_key("include") {
        let name = mapping
            .get("include")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if name.is_empty() {
            return Err(SchemaError::Generic(
                "include: value must be a non-empty string".to_string(),
            ));
        }
        let included_path: PathBuf = resolver.resolve(name, project_root)?;
        let included = _expand(&included_path, project_root, overlay_dir, chain, resolver)?;
        let stages = match included.get("stages") {
            Some(serde_json::Value::Array(s)) => s.clone(),
            _ => Vec::new(),
        };
        return Ok(stages);
    }

    let stage_type = mapping.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if !stage_type.is_empty() {
        if let Some(_def) = stage_defs.get(stage_type) {
            return _expand_stage_def(
                entry,
                stage_type,
                stage_defs,
                prompt_dir,
                project_root,
                overlay_dir,
                chain,
                named_prompts,
                seen_defs,
                resolver,
            );
        }
        // Try stage definition directories first (e.g. .gremlins/stages/plan.json5).
        // This must precede the gremlin-definition lookup so that stage recipes
        // receive call-site {{prompt}} and {{options}} substitution.
        if let Some(recipe) =
            load_stage_def_from_dirs(stage_type, Some(project_root), Some(overlay_dir))?
        {
            let mut direct_defs = stage_defs.clone();
            direct_defs.insert(stage_type.to_string(), recipe);
            return _expand_stage_def(
                entry,
                stage_type,
                &direct_defs,
                prompt_dir,
                project_root,
                overlay_dir,
                chain,
                named_prompts,
                seen_defs,
                resolver,
            );
        }
        // Try resolving as gremlin definition name
        match resolver.resolve(stage_type, project_root) {
            Ok(included_path) => {
                if !chain.contains(&included_path) {
                    let included =
                        _expand(&included_path, project_root, overlay_dir, chain, resolver)?;
                    let stages = match included.get("stages") {
                        Some(serde_json::Value::Array(s)) => s.clone(),
                        _ => Vec::new(),
                    };
                    return Ok(stages);
                }
            }
            Err(SchemaError::DefinitionNotFound { .. }) => {
                // Not found anywhere — fall through to loader validation
            }
            Err(e) => return Err(e),
        }
    }

    let mut entry = entry.clone();
    let entry_map = entry.as_object_mut().unwrap();

    if entry_map.contains_key("prompt") {
        let prompt_val = entry_map.get("prompt").unwrap().clone();
        let texts = prompts::read_prompts(&prompt_val, prompt_dir, named_prompts)?;
        entry_map.insert(
            "prompt".to_string(),
            serde_json::Value::Array(texts.into_iter().map(serde_json::Value::String).collect()),
        );
    }

    let is_parallel_body = entry_map
        .get("type")
        .and_then(|v| v.as_str())
        .is_some_and(|t| t == "parallel");

    if let Some(body_val) = entry_map.get("body") {
        if let Some(body_list) = body_val.as_array() {
            let mut expanded_body: Vec<serde_json::Value> = Vec::new();
            for body_entry in body_list {
                let child_dict = body_entry.as_object();
                let include_name = child_dict
                    .filter(|m| m.len() == 1)
                    .and_then(|m| m.get("include"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let expanded = _expand_entry(
                    body_entry,
                    prompt_dir,
                    project_root,
                    overlay_dir,
                    chain,
                    named_prompts,
                    stage_defs,
                    seen_defs,
                    resolver,
                )?;

                if is_parallel_body && expanded.is_empty() {
                    return Err(SchemaError::Generic(
                        "parallel child expanded to 0 stages via include; includes inside parallel groups must resolve to at least one stage".to_string()
                    ));
                }
                if !is_parallel_body || expanded.len() == 1 {
                    expanded_body.extend(expanded);
                } else {
                    let name =
                        include_name.unwrap_or_else(|| format!("sequence-{}", expanded_body.len()));
                    let mut seq = serde_json::Map::new();
                    seq.insert("name".to_string(), serde_json::Value::String(name));
                    seq.insert(
                        "type".to_string(),
                        serde_json::Value::String("sequence".to_string()),
                    );
                    seq.insert("body".to_string(), serde_json::Value::Array(expanded));
                    expanded_body.push(serde_json::Value::Object(seq));
                }
            }
            entry_map.insert("body".to_string(), serde_json::Value::Array(expanded_body));
        }
    }

    Ok(vec![entry])
}

/// Reject legacy `client`/`task-clients` model-selection keys at a recipe
/// call site *before* expansion copies only `model`/`task-models` onto the
/// expanded stage(s). Without this check those keys are silently dropped.
fn reject_legacy_call_site_model_keys(
    call_site_map: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), SchemaError> {
    for (legacy, replacement) in [("client", "model"), ("task-clients", "task-models")] {
        if call_site_map.contains_key(legacy) {
            return Err(SchemaError::Generic(format!(
                "stage-definition call site: '{legacy}' is no longer supported; use '{replacement}' instead"
            )));
        }
    }
    Ok(())
}

/// Validate call-site interpolation shape before distributing to stages.
/// Rejects scalars, non-mapping values, misspelled/legacy sub-keys, and
/// non-mapping `inputs`/`outputs` values — the same validation that
/// `yaml_interpolation_nested` applies to primitive stages.
fn validate_call_site_interpolation(
    call_site_map: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), SchemaError> {
    let Some(interp_val) = call_site_map.get("interpolation").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let interp_map = interp_val
        .as_object()
        .ok_or_else(|| SchemaError::Generic("'interpolation' must be a mapping".to_string()))?;
    for key in interp_map.keys() {
        match key.as_str() {
            "inputs" | "outputs" => {}
            other => {
                return Err(SchemaError::Generic(format!(
                    "unknown key {other:?} in 'interpolation'; expected 'inputs' or 'outputs'"
                )));
            }
        }
    }
    // Validate inputs values are mappings (not scalars)
    if let Some(inputs_val) = interp_map.get("inputs").filter(|v| !v.is_null()) {
        if !inputs_val.is_object() {
            return Err(SchemaError::Generic(
                "'interpolation.inputs' must be a mapping".to_string(),
            ));
        }
    }
    // Validate outputs values are mappings (not scalars)
    if let Some(outputs_val) = interp_map.get("outputs").filter(|v| !v.is_null()) {
        if !outputs_val.is_object() {
            return Err(SchemaError::Generic(
                "'interpolation.outputs' must be a mapping".to_string(),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn _expand_stage_def(
    call_site: &serde_json::Value,
    def_name: &str,
    stage_defs: &HashMap<String, serde_json::Value>,
    prompt_dir: &PathBuf,
    project_root: &PathBuf,
    overlay_dir: &Path,
    chain: &[PathBuf],
    named_prompts: &HashMap<String, Vec<String>>,
    seen_defs: &HashSet<String>,
    resolver: &dyn DefinitionResolver,
) -> Result<Vec<serde_json::Value>, SchemaError> {
    if seen_defs.contains(def_name) {
        return Err(SchemaError::Generic(format!(
            "stage-definition cycle: {def_name:?}"
        )));
    }

    let definition = stage_defs
        .get(def_name)
        .ok_or_else(|| SchemaError::Generic(format!("stage-definition {def_name:?} not found")))?;

    let mut new_seen = seen_defs.clone();
    new_seen.insert(def_name.to_string());

    let def_map = definition.as_object().ok_or_else(|| {
        SchemaError::Generic(format!("stage-definition {def_name:?} is not a mapping"))
    })?;

    let call_site_map = call_site.as_object().unwrap();
    reject_legacy_call_site_model_keys(call_site_map)?;

    if let Some(inner_list) = def_map.get("stages").and_then(|v| v.as_array()) {
        if inner_list.is_empty() {
            return Err(SchemaError::StageDef {
                name: def_name.to_string(),
                msg: "'stages' must be a non-empty list".to_string(),
            });
        }
        if def_map.contains_key("bind") {
            return Err(SchemaError::StageDef {
                name: def_name.to_string(),
                msg: "must not declare 'bind:' keys; declare them at each call site instead"
                    .to_string(),
            });
        }
        // Also reject interpolation.outputs in recipe definitions
        if let Some(interp) = def_map.get("interpolation").and_then(|v| v.as_object()) {
            if interp.contains_key("outputs") {
                return Err(SchemaError::StageDef {
                    name: def_name.to_string(),
                    msg: "must not declare 'interpolation.outputs:' keys; declare them at each call site instead"
                        .to_string(),
                });
            }
        }

        let last_idx = inner_list.len() - 1;
        let required_opts: Vec<String> = def_map
            .get("required-options")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Read flatten_into_parent before building the substitution context:
        // when true, inner stages are expanded inline into the caller and
        // call-site interpolation inputs/outputs are injected into ctx.options.
        let flatten_into_parent = match def_map.get("flatten_into_parent") {
            None => false,
            Some(v) => match v.as_bool() {
                Some(b) => b,
                None => {
                    return Err(SchemaError::StageDef {
                        name: def_name.to_string(),
                        msg: format!("'flatten_into_parent' must be a boolean, got {:?}", v),
                    });
                }
            },
        };

        // Validate call-site interpolation shape before checking flatten constraints
        // so malformed call-site data is reported before definition-structure errors.
        validate_call_site_interpolation(call_site_map)?;

        // When flattening, reject definition-level keys that must live on the
        // wrapper (which does not exist in flatten mode).
        if flatten_into_parent {
            // Keys that can be set at the call site — advise moving them there.
            let call_site_keys: &[&str] = &["model", "task-models"];
            // Keys that only live on the wrapper — advise removing flatten.
            let wrapper_only_keys: &[&str] = &["max-iterations", "interval", "skip_if_exists"];

            for key in call_site_keys.iter().chain(wrapper_only_keys.iter()) {
                if def_map.contains_key(*key) {
                    let hint = if call_site_keys.contains(key) {
                        format!(
                            "remove 'flatten_into_parent: true' to enable wrapping \
                             or move '{key}' to each call site"
                        )
                    } else {
                        format!(
                            "remove 'flatten_into_parent: true' so '{key}' can live \
                             on the sequence wrapper"
                        )
                    };
                    return Err(SchemaError::StageDef {
                        name: def_name.to_string(),
                        msg: format!(
                            "'{key}' cannot be used with 'flatten_into_parent: true'; {hint}"
                        ),
                    });
                }
            }
        }

        let mut cs_opts: HashMap<String, serde_json::Value> = call_site_map
            .get("options")
            .and_then(|v| v.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        for opt in &required_opts {
            let val = cs_opts.get(opt);
            let is_empty = match val {
                None => true,
                Some(serde_json::Value::Array(s)) => s.is_empty(),
                Some(serde_json::Value::Null) => true,
                _ => false,
            };
            if is_empty {
                let stage_display = call_site_map
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(def_name);
                return Err(SchemaError::Stage {
                    name: stage_display.to_string(),
                    msg: format!("required option {opt:?} is missing or empty"),
                });
            }
        }

        // When flattening, expose call-site interpolation inputs and outputs
        // through ctx.options so every inner stage can reference them via
        // {{options.<key>}}. The verbatim interpolation merge below is skipped
        // for flattened recipes, so this is the only channel for those values.
        //
        // Merge precedence (last-write-wins): call-site options, then
        // interpolation.outputs, then interpolation.inputs.  Inputs win over
        // outputs because they are the typical artifact source channel and
        // matching a same-named output would silently break the data flow.
        if flatten_into_parent {
            if let Some(outputs) = call_site_map
                .get("interpolation")
                .and_then(|v| v.as_object())
                .and_then(|m| m.get("outputs"))
                .and_then(|v| v.as_object())
            {
                for (k, v) in outputs {
                    cs_opts.insert(k.clone(), v.clone());
                }
            }
            if let Some(inputs) = call_site_map
                .get("interpolation")
                .and_then(|v| v.as_object())
                .and_then(|m| m.get("inputs"))
                .and_then(|v| v.as_object())
            {
                for (k, v) in inputs {
                    cs_opts.insert(k.clone(), v.clone());
                }
            }
        }

        let cs_prompts: Vec<String> = if call_site_map.contains_key("prompt") {
            prompts::read_prompts(
                call_site_map.get("prompt").unwrap(),
                prompt_dir,
                named_prompts,
            )?
        } else {
            Vec::new()
        };

        if def_map
            .get("required-prompt")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && cs_prompts.is_empty()
        {
            let stage_display = call_site_map
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(def_name);
            return Err(SchemaError::Stage {
                name: stage_display.to_string(),
                msg: "required prompt is missing or empty".to_string(),
            });
        }

        let mut ctx = serde_json::Map::new();
        if let Some(name) = call_site_map.get("name").and_then(|v| v.as_str()) {
            ctx.insert(
                "name".to_string(),
                serde_json::Value::String(name.to_string()),
            );
        }
        ctx.insert(
            "options".to_string(),
            serde_json::Value::Object(cs_opts.into_iter().collect()),
        );
        ctx.insert(
            "prompt".to_string(),
            serde_json::Value::Array(
                cs_prompts
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
        );

        let ctx_value = serde_json::Value::Object(ctx);

        let mut result: Vec<serde_json::Value> = Vec::new();
        for (i, raw_inner) in inner_list.iter().enumerate() {
            let substituted = substitute_recipe(raw_inner, &ctx_value)?;
            let mut inner = substituted.clone();
            if !inner.is_object() {
                return Err(SchemaError::StageDef {
                    name: def_name.to_string(),
                    msg: format!("inner stage {i} must be a mapping, got {:?}", inner),
                });
            }
            let inner_map = inner.as_object_mut().unwrap();

            // Only propagate call-site name / model / task-models to the first
            // inner stage when flattening. When wrapping, those keys live on the
            // sequence wrapper instead.
            if flatten_into_parent && i == 0 {
                if let Some(name) = call_site_map.get("name") {
                    inner_map.insert("name".to_string(), name.clone());
                } else if inner_map.contains_key("name") {
                    let existing_name = inner_map.remove("name").unwrap();
                    inner_map.insert("_auto_name".to_string(), existing_name);
                }
                if let Some(model) = call_site_map.get("model") {
                    inner_map.insert("model".to_string(), model.clone());
                }
                if let Some(task_models) = call_site_map.get("task-models") {
                    inner_map.insert("task-models".to_string(), task_models.clone());
                }
            }
            if !flatten_into_parent && i == 0 {
                // Merge call-site interpolation.inputs into inner stage's interpolation.inputs
                if let Some(interpolation_val) = call_site_map.get("interpolation") {
                    if let Some(m) = interpolation_val.as_object() {
                        if let Some(cs_inputs) = m.get("inputs").and_then(|v| v.as_object()) {
                            let mut merged_interpolation = inner_map
                                .get("interpolation")
                                .and_then(|v| v.as_object())
                                .and_then(|m| m.get("inputs"))
                                .and_then(|v| v.as_object())
                                .map(|im| {
                                    im.iter()
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect::<serde_json::Map<String, serde_json::Value>>()
                                })
                                .unwrap_or_default();
                            for (k, v) in cs_inputs {
                                merged_interpolation.insert(k.clone(), v.clone());
                            }
                            let mut nested = serde_json::Map::new();
                            nested.insert(
                                "inputs".to_string(),
                                serde_json::Value::Object(merged_interpolation),
                            );
                            // Preserve existing outputs if any
                            if let Some(existing_outputs) = inner_map
                                .get("interpolation")
                                .and_then(|v| v.as_object())
                                .and_then(|m| m.get("outputs").cloned())
                            {
                                nested.insert("outputs".to_string(), existing_outputs);
                            }
                            inner_map.insert(
                                "interpolation".to_string(),
                                serde_json::Value::Object(nested),
                            );
                        }
                    }
                }
            }
            if !flatten_into_parent && i == last_idx {
                // Merge call-site interpolation.outputs into inner stage's interpolation.outputs
                if let Some(interp_val) = call_site_map.get("interpolation") {
                    if let Some(m) = interp_val.as_object() {
                        if let Some(outputs_val) = m.get("outputs") {
                            // New nested form: merge into interpolation.outputs
                            let cs_out = outputs_val.as_object().ok_or_else(|| {
                                SchemaError::Generic(
                                    "'interpolation.outputs' must be a mapping".to_string(),
                                )
                            })?;
                            let existing_outputs = inner_map
                                .get("interpolation")
                                .and_then(|v| v.as_object())
                                .and_then(|m| m.get("outputs"))
                                .and_then(|v| v.as_object())
                                .map(|m| {
                                    m.iter()
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect::<serde_json::Map<String, serde_json::Value>>()
                                })
                                .unwrap_or_default();

                            let mut merged_outputs = existing_outputs;
                            for (k, v) in cs_out {
                                merged_outputs.insert(k.clone(), v.clone());
                            }

                            // Build nested interpolation mapping
                            let mut nested = serde_json::Map::new();
                            // Preserve existing inputs
                            if let Some(interp_mapping) =
                                inner_map.get("interpolation").and_then(|v| v.as_object())
                            {
                                if let Some(existing_inputs) = interp_mapping.get("inputs").cloned()
                                {
                                    nested.insert("inputs".to_string(), existing_inputs);
                                }
                            }
                            nested.insert(
                                "outputs".to_string(),
                                serde_json::Value::Object(merged_outputs),
                            );
                            inner_map.insert(
                                "interpolation".to_string(),
                                serde_json::Value::Object(nested),
                            );
                        }
                    }
                }
            }

            let expanded = _expand_entry(
                &inner,
                prompt_dir,
                project_root,
                overlay_dir,
                chain,
                named_prompts,
                stage_defs,
                &new_seen,
                resolver,
            )?;
            result.extend(expanded);
        }
        // Unrecognized-key validation — applies in both modes.
        let recognized: &[&str] = &[
            "stages",
            "flatten_into_parent",
            "required-options",
            "required-prompt",
            "max-iterations",
            "interval",
            "skip_if_exists",
            "model",
            "task-models",
            "name",
            "interpolation",
        ];
        for key in def_map.keys() {
            if !recognized.contains(&key.as_str()) {
                return Err(SchemaError::StageDef {
                    name: def_name.to_string(),
                    msg: format!("unrecognized key {key:?} in stage definition"),
                });
            }
        }

        // When wrapping (default), build a sequence that contains the expanded
        // inner stages. Call-site values take precedence over definition-level
        // values for shared keys.
        if !flatten_into_parent {
            let mut seq = serde_json::Map::new();

            // Name: call-site name or the definition file stem.
            let wrapper_name = call_site_map
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| def_name.to_string());
            seq.insert("name".to_string(), serde_json::Value::String(wrapper_name));
            seq.insert(
                "type".to_string(),
                serde_json::Value::String("sequence".to_string()),
            );
            seq.insert("body".to_string(), serde_json::Value::Array(result));

            // model: call-site takes precedence
            if let Some(model) = call_site_map.get("model") {
                seq.insert("model".to_string(), model.clone());
            } else if let Some(model) = def_map.get("model") {
                seq.insert("model".to_string(), substitute_recipe(model, &ctx_value)?);
            }

            // task-models: call-site takes precedence
            if let Some(tm) = call_site_map.get("task-models") {
                seq.insert("task-models".to_string(), tm.clone());
            } else if let Some(tm) = def_map.get("task-models") {
                // Substitute each value in the map individually.
                let substituted = match tm {
                    serde_json::Value::Object(m) => {
                        let mut out = serde_json::Map::new();
                        for (k, v) in m {
                            out.insert(k.clone(), substitute_recipe(v, &ctx_value)?);
                        }
                        serde_json::Value::Object(out)
                    }
                    other => substitute_recipe(other, &ctx_value)?,
                };
                seq.insert("task-models".to_string(), substituted);
            }

            // max-iterations: definition-level only
            if let Some(mi) = def_map.get("max-iterations") {
                seq.insert(
                    "max-iterations".to_string(),
                    substitute_recipe(mi, &ctx_value)?,
                );
            }

            // interval: definition-level only
            if let Some(interval) = def_map.get("interval") {
                seq.insert(
                    "interval".to_string(),
                    substitute_recipe(interval, &ctx_value)?,
                );
            }

            // skip_if_exists: call-site takes precedence
            if let Some(sif) = call_site_map.get("skip_if_exists") {
                seq.insert("skip_if_exists".to_string(), sif.clone());
            } else if let Some(sif) = def_map.get("skip_if_exists") {
                seq.insert(
                    "skip_if_exists".to_string(),
                    substitute_recipe(sif, &ctx_value)?,
                );
            }

            return Ok(vec![serde_json::Value::Object(seq)]);
        }

        return Ok(result);
    }

    // Single-primitive definition
    if def_map.contains_key("bind") {
        return Err(SchemaError::StageDef {
            name: def_name.to_string(),
            msg: "must not declare 'bind:' keys; declare them at each call site instead"
                .to_string(),
        });
    }
    // Also reject interpolation.outputs in recipe definitions
    if let Some(interp) = def_map.get("interpolation").and_then(|v| v.as_object()) {
        if interp.contains_key("outputs") {
            return Err(SchemaError::StageDef {
                name: def_name.to_string(),
                msg: "must not declare 'interpolation.outputs:' keys; declare them at each call site instead"
                    .to_string(),
            });
        }
    }

    let mut merged = definition.clone();
    let merged_map = merged.as_object_mut().unwrap();

    // Merge call-site keys: name, interpolation (inputs/outputs), model, and
    // task-models. `model:` / `task-models:` propagate to a
    // single-primitive recipe the same way they propagate to the first inner
    // stage of a multi-stage recipe.
    for key in &["name", "interpolation", "model", "task-models"] {
        if let Some(v) = call_site_map.get(*key) {
            if *key == "interpolation" {
                // Handle nested interpolation merge
                if let Some(cs_map) = v.as_object() {
                    // Merge into existing interpolation entry-by-entry.
                    let mut existing = merged_map
                        .get("interpolation")
                        .and_then(|ev| ev.as_object())
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<serde_json::Map<String, serde_json::Value>>()
                        })
                        .unwrap_or_default();
                    for (sub_k, sub_v) in cs_map {
                        if let Some(cs_sub_map) = sub_v.as_object() {
                            // Merge call-site entries into existing sub-map
                            let mut merged_sub = existing
                                .get(sub_k)
                                .and_then(|v| v.as_object())
                                .map(|m| {
                                    m.iter()
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect::<serde_json::Map<String, serde_json::Value>>()
                                })
                                .unwrap_or_default();
                            for (entry_k, entry_v) in cs_sub_map {
                                merged_sub.insert(entry_k.clone(), entry_v.clone());
                            }
                            existing.insert(sub_k.clone(), serde_json::Value::Object(merged_sub));
                        } else {
                            existing.insert(sub_k.clone(), sub_v.clone());
                        }
                    }
                    merged_map.insert(
                        "interpolation".to_string(),
                        serde_json::Value::Object(existing),
                    );
                } else {
                    // Non-mapping value — insert as-is so schema validation can reject it
                    merged_map.insert("interpolation".to_string(), v.clone());
                }
            } else {
                merged_map.insert(key.to_string(), v.clone());
            }
        }
    }
    if !call_site_map.contains_key("name") && merged_map.contains_key("name") {
        let existing_name = merged_map.remove("name").unwrap();
        merged_map.insert("_auto_name".to_string(), existing_name);
    }

    _expand_entry(
        &merged,
        prompt_dir,
        project_root,
        overlay_dir,
        chain,
        named_prompts,
        stage_defs,
        &new_seen,
        resolver,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serde_json_preserves_loop_iter_in_quoted_string() {
        // Regression: serde_json must preserve {loop_iter} inside a string.
        let json_str = r#"
{
  "stages": [
    {
      "name": "verify",
      "type": "sequence",
      "skip_if_exists": "artifact://{loop_iter}/done",
      "body": [
        {
          "name": "fix",
          "type": "sequence",
          "skip_if_exists": "artifact://{loop_iter}/done",
          "body": [
            {
              "name": "fix",
              "type": "agent"
            }
          ]
        }
      ]
    }
  ]
}
"#;
        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let stages = parsed["stages"].as_array().unwrap();
        let seq_stage = &stages[0];
        let sif = seq_stage["skip_if_exists"].as_str().unwrap();
        assert_eq!(
            sif, "artifact://{loop_iter}/done",
            "skip_if_exists should preserve {{loop_iter}}"
        );
        let body = seq_stage["body"].as_array().unwrap();
        let fix_wrapper = &body[0];
        let skip = fix_wrapper["skip_if_exists"].as_str().unwrap();
        assert_eq!(
            skip, "artifact://{loop_iter}/done",
            "skip_if_exists on sequence wrapper should preserve {{loop_iter}}"
        );
    }

    #[test]
    fn reject_json5_numeric_literals_allows_strings_comments_and_exponents() {
        let path = Path::new("demo.json5");
        // `NaN`/`0xFF` appear only inside strings and a comment; `1e+3` is a
        // valid signed decimal exponent; `NaN` as an unquoted key is valid.
        reject_json5_numeric_literals(
            r#"{
              // echo NaN in a comment
              "cmd": 'echo NaN',
              shell: "echo 0xFF",
              NaN: 1,
              amount: 1e+3,
            }"#,
            path,
        )
        .unwrap();
    }

    #[test]
    fn reject_json5_numeric_literals_rejects_real_bad_literals() {
        let path = Path::new("demo.json5");
        for (bad, needle) in [
            ("{ value: NaN }", "NaN"),
            ("{ value: Infinity }", "Infinity"),
            ("{ value: -Infinity }", "-Infinity"),
            ("{ value: 0xFF }", "0xFF"),
            ("{ value: +1 }", "+1"),
        ] {
            let err = reject_json5_numeric_literals(bad, path).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(needle), "{msg}");
            assert!(msg.contains("not supported"), "{msg}");
        }
    }

    #[test]
    fn test_verify_recipe_skip_if_exists_preserves_loop_iter() {
        let recipe = load_json_file(std::path::Path::new(&format!(
            "{}/../../.gremlins/stages/verify.json5",
            env!("CARGO_MANIFEST_DIR")
        )))
        .unwrap();
        let stages = recipe["stages"].as_array().unwrap();
        let seq_stage = &stages[0];
        let body = seq_stage["body"].as_array().unwrap();
        // The fix child is now wrapped in a sequence (agents can't carry
        // skip_if_exists directly), so body[1] is the sequence wrapper.
        let fix_wrapper = &body[1];
        let skip = fix_wrapper["skip_if_exists"].as_str().unwrap();
        assert_eq!(
            skip, "artifact://{loop_iter}/done",
            "Raw verify recipe skip_if_exists should preserve {{loop_iter}}"
        );
    }

    #[test]
    fn test_substitute_recipe_simple() {
        let mut ctx_map = serde_json::Map::new();
        ctx_map.insert(
            "options".to_string(),
            serde_json::Value::Object({
                let mut m = serde_json::Map::new();
                m.insert(
                    "key".to_string(),
                    serde_json::Value::String("value".to_string()),
                );
                m
            }),
        );
        let ctx = serde_json::Value::Object(ctx_map);

        let input = serde_json::Value::String("{{options.key}}".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "value");
    }

    #[test]
    fn test_substitute_recipe_default() {
        let ctx = serde_json::Value::Object(serde_json::Map::new());
        let input =
            serde_json::Value::String("{{options.missing | default(fallback)}}".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "fallback");
    }

    #[test]
    fn test_substitute_recipe_missing_placeholder_errors() {
        let ctx = serde_json::Value::Object(serde_json::Map::new());
        let input = serde_json::Value::String("{{options.missing}}".to_string());
        let err = substitute_recipe(&input, &ctx).unwrap_err();
        assert!(err.to_string().contains("not found in context"));
    }

    #[test]
    fn test_parse_default_quoted() {
        let result = parse_default("\"hello\"");
        assert_eq!(result.as_str().unwrap(), "hello");
    }

    #[test]
    fn test_parse_default_unquoted() {
        let result = parse_default("hello");
        assert_eq!(result.as_str().unwrap(), "hello");
    }

    #[test]
    fn test_substitute_recipe_inline() {
        let mut ctx_map = serde_json::Map::new();
        ctx_map.insert(
            "options".to_string(),
            serde_json::Value::Object({
                let mut m = serde_json::Map::new();
                m.insert(
                    "key".to_string(),
                    serde_json::Value::String("value".to_string()),
                );
                m
            }),
        );
        let ctx = serde_json::Value::Object(ctx_map);
        let input = serde_json::Value::String("foo {{options.key}} bar".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "foo value bar");
    }

    #[test]
    fn test_substitute_recipe_list_join() {
        let mut ctx_map = serde_json::Map::new();
        ctx_map.insert(
            "options".to_string(),
            serde_json::Value::Object({
                let mut m = serde_json::Map::new();
                m.insert(
                    "cmds".to_string(),
                    serde_json::Value::Array(vec![
                        serde_json::Value::String("cmd1".to_string()),
                        serde_json::Value::String("cmd2".to_string()),
                    ]),
                );
                m
            }),
        );
        let ctx = serde_json::Value::Object(ctx_map);
        let input = serde_json::Value::String("run {{options.cmds}} please".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "run cmd1 && cmd2 please");
    }

    #[test]
    fn test_substitute_recipe_unresolved_verbatim() {
        let ctx = serde_json::Value::Object(serde_json::Map::new());
        let input = serde_json::Value::String("hello {{missing}} world".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "hello {{missing}} world");
    }

    /// When a single-primitive stage definition uses nested
    /// `interpolation: {inputs: {foo: bar}}` and the call site also uses the nested
    /// `interpolation: {inputs: {baz: qux}}`, the merge must combine both into
    /// `inputs:`.
    #[test]
    fn test_merge_nested_definition_with_call_site_nested() {
        // A mock resolver that always returns "not found" — we test with
        // in-memory stage_defs.
        struct NotFoundResolver;
        impl DefinitionResolver for NotFoundResolver {
            fn resolve(
                &self,
                name: &str,
                _project_root: &std::path::Path,
            ) -> Result<PathBuf, SchemaError> {
                Err(SchemaError::DefinitionNotFound {
                    name: name.to_string(),
                    available: String::new(),
                })
            }
        }

        // Definition: nested interpolation
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"
{
  "type": "agent",
  "prompt": ["use {foo} and {baz}\n"],
  "interpolation": {
    "inputs": {
      "foo": "content(\"artifact://foo.md\")"
    }
  }
}
"#,
        )
        .unwrap();

        // Call site: new nested form
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"
{
  "name": "test",
  "interpolation": {
    "inputs": {
      "baz": "content(\"artifact://baz.md\")"
    }
  }
}
"#,
        )
        .unwrap();

        let mut stage_defs = HashMap::new();
        stage_defs.insert("mydef".to_string(), definition);

        let prompt_dir = PathBuf::from(".");
        let project_root = PathBuf::from(".");
        let overlay_dir = Path::new(".");
        let chain: Vec<PathBuf> = vec![];
        let named_prompts = HashMap::new();
        let seen_defs = HashSet::new();
        let resolver = NotFoundResolver;

        let result = _expand_stage_def(
            &call_site,
            "mydef",
            &stage_defs,
            &prompt_dir,
            &project_root,
            overlay_dir,
            &chain,
            &named_prompts,
            &seen_defs,
            &resolver,
        );

        // Should succeed — no error about missing keys
        let expanded = result.unwrap();
        assert_eq!(expanded.len(), 1, "single-primitive def produces one stage");

        let stage = &expanded[0];
        let interp = stage
            .get("interpolation")
            .and_then(|v| v.as_object())
            .expect("interpolation must be a mapping");

        // Must be the nested form
        assert!(
            interp.contains_key("inputs"),
            "merged interpolation must have 'inputs' key"
        );

        let inputs = interp
            .get("inputs")
            .and_then(|v| v.as_object())
            .expect("inputs must be a mapping");

        // Both the definition's foo and the call-site's baz must be present
        assert!(
            inputs.contains_key("foo"),
            "definition's 'foo' key must be preserved under inputs"
        );
        assert!(
            inputs.contains_key("baz"),
            "call-site's 'baz' key must be present under inputs"
        );
        assert_eq!(
            inputs.get("foo").and_then(|v| v.as_str()).unwrap(),
            "content(\"artifact://foo.md\")"
        );
        assert_eq!(
            inputs.get("baz").and_then(|v| v.as_str()).unwrap(),
            "content(\"artifact://baz.md\")"
        );
    }

    #[test]
    fn recipe_call_site_rejects_legacy_model_keys_before_expansion() {
        struct NotFoundResolver;
        impl DefinitionResolver for NotFoundResolver {
            fn resolve(
                &self,
                name: &str,
                _project_root: &std::path::Path,
            ) -> Result<PathBuf, SchemaError> {
                Err(SchemaError::DefinitionNotFound {
                    name: name.to_string(),
                    available: String::new(),
                })
            }
        }

        for (legacy, replacement) in [("client", "model"), ("task-clients", "task-models")] {
            let definition =
                serde_json::from_str::<serde_json::Value>(r#"{"type": "agent", "prompt": "hi"}"#)
                    .unwrap();

            let call_site = serde_json::from_str::<serde_json::Value>(&format!(
                r#"{{"name": "test", "{legacy}": "openai/gpt-4"}}"#
            ))
            .unwrap();

            let mut stage_defs = HashMap::new();
            stage_defs.insert("mydef".to_string(), definition);

            let prompt_dir = PathBuf::from(".");
            let project_root = PathBuf::from(".");
            let overlay_dir = Path::new(".");
            let chain: Vec<PathBuf> = vec![];
            let named_prompts = HashMap::new();
            let seen_defs = HashSet::new();
            let resolver = NotFoundResolver;

            let err = _expand_stage_def(
                &call_site,
                "mydef",
                &stage_defs,
                &prompt_dir,
                &project_root,
                overlay_dir,
                &chain,
                &named_prompts,
                &seen_defs,
                &resolver,
            )
            .expect_err("recipe call sites with legacy model keys must be rejected");

            let msg = err.to_string();
            assert!(
                msg.contains(legacy) && msg.contains(replacement),
                "error must name {legacy:?} and its replacement {replacement:?}, got: {msg}"
            );
        }
    }

    // ── flatten_into_parent tests ───────────────────────────────────────

    /// Helper: build a resolver that returns DefinitionNotFound for every name.
    fn not_found_resolver() -> impl DefinitionResolver {
        struct R;
        impl DefinitionResolver for R {
            fn resolve(&self, name: &str, _: &std::path::Path) -> Result<PathBuf, SchemaError> {
                Err(SchemaError::DefinitionNotFound {
                    name: name.to_string(),
                    available: String::new(),
                })
            }
        }
        R
    }

    /// Convenience wrapper around _expand_stage_def with default arguments.
    fn expand_stage_def(
        call_site: &serde_json::Value,
        def_name: &str,
        stage_defs: &HashMap<String, serde_json::Value>,
    ) -> Result<Vec<serde_json::Value>, SchemaError> {
        _expand_stage_def(
            call_site,
            def_name,
            stage_defs,
            &PathBuf::from("."),
            &PathBuf::from("."),
            Path::new("."),
            &[],
            &HashMap::new(),
            &HashSet::new(),
            &not_found_resolver(),
        )
    }

    #[test]
    fn default_wrap_two_stage_recipe_returns_single_sequence() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "stages": [
                    {"name": "first", "type": "agent", "prompt": ["hello\n"]},
                    {"name": "second", "type": "exec", "options": {"cmds": ["true"]}}
                ]
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "wrapper", "type": "mydef"}"#)
                .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        assert_eq!(result.len(), 1, "default wrap produces a single stage");
        let seq = &result[0];
        assert_eq!(seq["type"].as_str().unwrap(), "sequence");
        assert_eq!(seq["name"].as_str().unwrap(), "wrapper");
        let body = seq["body"].as_array().unwrap();
        assert_eq!(body.len(), 2, "body contains both inner stages");
    }

    #[test]
    fn flatten_into_parent_true_produces_flat_stages() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "stages": [
                    {"name": "first", "type": "agent", "prompt": ["hello\n"]},
                    {"name": "second", "type": "exec", "options": {"cmds": ["true"]}}
                ]
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "inline", "type": "mydef"}"#)
                .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        assert_eq!(result.len(), 2, "flatten produces two top-level stages");
        assert_eq!(result[0]["name"].as_str().unwrap(), "inline");
        assert_eq!(result[1]["name"].as_str().unwrap(), "second");
    }

    #[test]
    fn flatten_with_max_iterations_errors() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "max-iterations": 3,
                "stages": [
                    {"name": "s", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "x", "type": "mydef"}"#).unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let err = expand_stage_def(&call_site, "mydef", &defs).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("max-iterations"),
            "error must name max-iterations: {msg}"
        );
        assert!(
            msg.contains("flatten_into_parent"),
            "error must mention flatten: {msg}"
        );
    }

    #[test]
    fn flatten_with_interval_errors() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "interval": 60,
                "stages": [{"name": "s", "type": "agent", "prompt": ["hi\n"]}]
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "x", "type": "mydef"}"#).unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let err = expand_stage_def(&call_site, "mydef", &defs).unwrap_err();
        assert!(err.to_string().contains("interval"));
    }

    #[test]
    fn flatten_with_skip_if_exists_errors() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "skip_if_exists": "artifact://done",
                "stages": [{"name": "s", "type": "agent", "prompt": ["hi\n"]}]
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "x", "type": "mydef"}"#).unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let err = expand_stage_def(&call_site, "mydef", &defs).unwrap_err();
        assert!(err.to_string().contains("skip_if_exists"));
    }

    #[test]
    fn flatten_with_def_model_errors() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "model": "openai/gpt-4",
                "stages": [{"name": "s", "type": "agent", "prompt": ["hi\n"]}]
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "x", "type": "mydef"}"#).unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let err = expand_stage_def(&call_site, "mydef", &defs).unwrap_err();
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn flatten_injects_call_site_interpolation_inputs_into_options() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "stages": [
                    {"name": "first", "type": "agent", "prompt": ["plan: {{options.plan}}\n"]},
                    {"name": "second", "type": "exec", "options": {"cmds": ["cat {{options.plan}}"]}}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{
                "name": "inline",
                "type": "mydef",
                "interpolation": {
                    "inputs": {
                        "plan": "content(\"artifact://plan.md\")"
                    }
                }
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        assert_eq!(result.len(), 2);

        assert_eq!(
            result[0]["prompt"][0].as_str().unwrap(),
            "plan: content(\"artifact://plan.md\")\n"
        );
        assert_eq!(
            result[1]["options"]["cmds"][0].as_str().unwrap(),
            "cat content(\"artifact://plan.md\")"
        );
        assert!(result[0].get("interpolation").is_none());
        assert!(result[1].get("interpolation").is_none());
    }

    #[test]
    fn flatten_injects_call_site_interpolation_outputs_into_options() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "stages": [
                    {"name": "first", "type": "agent", "prompt": ["out: {{options.artifact}}\n"]},
                    {"name": "second", "type": "agent", "prompt": ["again: {{options.artifact}}\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{
                "name": "flat",
                "type": "mydef",
                "interpolation": {
                    "outputs": {
                        "artifact": "artifact://result.md"
                    }
                }
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0]["prompt"][0].as_str().unwrap(),
            "out: artifact://result.md\n"
        );
        assert_eq!(
            result[1]["prompt"][0].as_str().unwrap(),
            "again: artifact://result.md\n"
        );
        assert!(result[0].get("interpolation").is_none());
        assert!(result[1].get("interpolation").is_none());
    }

    #[test]
    fn wrapped_recipe_merges_call_site_interpolation_into_first_and_last() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "stages": [
                    {"name": "first", "type": "agent", "prompt": ["hi\n"]},
                    {"name": "middle", "type": "agent", "prompt": ["mid\n"]},
                    {"name": "last", "type": "agent", "prompt": ["bye\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{
                "name": "wrapped",
                "type": "mydef",
                "interpolation": {
                    "inputs": {"plan": "content(\"artifact://plan.md\")"},
                    "outputs": {"result": "artifact://result.md"}
                }
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        assert_eq!(result.len(), 1);
        let seq = &result[0];
        assert_eq!(seq["type"].as_str().unwrap(), "sequence");
        let body = seq["body"].as_array().unwrap();
        assert_eq!(body.len(), 3);

        let first_inputs = body[0]["interpolation"]["inputs"].as_object().unwrap();
        assert_eq!(
            first_inputs["plan"].as_str().unwrap(),
            "content(\"artifact://plan.md\")"
        );
        assert!(body[0]["interpolation"].get("outputs").is_none());
        assert!(body[1].get("interpolation").is_none());
        let last_outputs = body[2]["interpolation"]["outputs"].as_object().unwrap();
        assert_eq!(
            last_outputs["result"].as_str().unwrap(),
            "artifact://result.md"
        );
    }

    #[test]
    fn flatten_interpolation_input_wins_over_call_site_option_on_key_collision() {
        // Plan guardrail: when call-site options and interpolation.inputs share a
        // key, the interpolation input wins because it is merged after options.
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "stages": [
                    {"name": "s", "type": "agent", "prompt": ["{{options.plan}}\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{
                "name": "cs",
                "type": "mydef",
                "options": {"plan": "from-options"},
                "interpolation": {
                    "inputs": {"plan": "from-interpolation"}
                }
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0]["prompt"][0].as_str().unwrap(),
            "from-interpolation\n"
        );
    }

    #[test]
    fn flatten_interpolation_input_wins_over_output_on_key_collision() {
        // When interpolation.inputs and interpolation.outputs share a key,
        // inputs win because they are merged after outputs.
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "stages": [
                    {"name": "s", "type": "agent", "prompt": ["{{options.artifact}}\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{
                "name": "cs",
                "type": "mydef",
                "interpolation": {
                    "inputs": {"artifact": "artifact://input.md"},
                    "outputs": {"artifact": "artifact://output.md"}
                }
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0]["prompt"][0].as_str().unwrap(),
            "artifact://input.md\n"
        );
    }

    #[test]
    fn flatten_interpolation_key_does_not_satisfy_required_option() {
        // Interpolation inputs/outputs are injected into cs_opts *after* the
        // required_opts check, so an interpolation key must not accidentally
        // satisfy a required option.
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": true,
                "required-options": ["plan"],
                "stages": [
                    {"name": "s", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{
                "name": "cs",
                "type": "mydef",
                "interpolation": {
                    "inputs": {"plan": "content(\"artifact://plan.md\")"}
                }
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let err = expand_stage_def(&call_site, "mydef", &defs).unwrap_err();
        assert!(err.to_string().contains("required option"));
        assert!(err.to_string().contains("plan"));
    }

    #[test]
    fn wrap_puts_call_site_model_and_task_models_on_sequence() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{
                "name": "wrapped",
                "type": "mydef",
                "model": "anthropic/claude",
                "task-models": {"plan": "openai/gpt-4"}
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        assert_eq!(result.len(), 1);
        let seq = &result[0];
        assert_eq!(seq["type"].as_str().unwrap(), "sequence");
        assert_eq!(seq["model"].as_str().unwrap(), "anthropic/claude");
        let tm = seq["task-models"].as_object().unwrap();
        assert_eq!(tm["plan"].as_str().unwrap(), "openai/gpt-4");
        // Inner stage must NOT have model/task-models propagated
        let body = seq["body"].as_array().unwrap();
        let inner = &body[0];
        assert!(
            inner.get("model").is_none(),
            "inner stage must not get model in wrap mode"
        );
        assert!(
            inner.get("task-models").is_none(),
            "inner stage must not get task-models in wrap mode"
        );
    }

    #[test]
    fn wrap_passes_def_max_iterations_to_sequence() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "max-iterations": 5,
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "w", "type": "mydef"}"#).unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(seq["max-iterations"].as_i64().unwrap(), 5);
    }

    #[test]
    fn wrap_call_site_skip_if_exists_takes_precedence() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "skip_if_exists": "artifact://def-done",
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{
                "name": "w",
                "type": "mydef",
                "skip_if_exists": "artifact://cs-done"
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(
            seq["skip_if_exists"].as_str().unwrap(),
            "artifact://cs-done"
        );
    }

    #[test]
    fn unrecognized_key_at_definition_level_errors() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ],
                "foo": "bar"
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "w", "type": "mydef"}"#).unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let err = expand_stage_def(&call_site, "mydef", &defs).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unrecognized"),
            "error must mention unrecognized: {msg}"
        );
        assert!(msg.contains("foo"), "error must name the bad key: {msg}");
    }

    #[test]
    fn flatten_into_parent_must_be_boolean() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "flatten_into_parent": "true",
                "stages": [
                    {"name": "s", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "x", "type": "mydef"}"#).unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let err = expand_stage_def(&call_site, "mydef", &defs).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must be a boolean"),
            "error must say must be a boolean: {msg}"
        );
    }

    #[test]
    fn wrap_substitutes_def_max_iterations_placeholder() {
        // Definition-level max-iterations can reference call-site options.
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "max-iterations": "{{options.max_iterations | default(3)}}",
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        // When the option is supplied, it is used.
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{"name": "w", "type": "mydef", "options": {"max_iterations": 7}}"#,
        )
        .unwrap();
        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(
            seq["max-iterations"].as_i64().unwrap(),
            7,
            "supplied option resolves to 7"
        );

        // When omitted, the default applies.
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "w", "type": "mydef"}"#).unwrap();
        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(
            seq["max-iterations"].as_i64().unwrap(),
            3,
            "default resolves to 3"
        );
    }

    #[test]
    fn wrap_substitutes_def_model_placeholder() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "model": "{{options.x}}",
                "task-models": {"plan": "{{options.x}}"},
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{"name": "w", "type": "mydef", "options": {"x": "anthropic/claude"}}"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(seq["model"].as_str().unwrap(), "anthropic/claude");
        assert_eq!(
            seq["task-models"].as_object().unwrap()["plan"]
                .as_str()
                .unwrap(),
            "anthropic/claude"
        );
    }

    #[test]
    fn wrap_call_site_model_takes_precedence_over_def_placeholder() {
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "model": "{{options.x}}",
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();
        // Call-site model wins even though the definition would also resolve.
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{"name": "w", "type": "mydef", "model": "openai/gpt-4", "options": {"x": "anthropic/claude"}}"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(seq["model"].as_str().unwrap(), "openai/gpt-4");
    }

    #[test]
    fn wrap_substitutes_def_interval_placeholder() {
        // Definition-level interval can reference call-site options.
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "interval": "{{options.delay | default(1.5)}}",
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        // When the option is supplied, it is used.
        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{"name": "w", "type": "mydef", "options": {"delay": 2.5}}"#,
        )
        .unwrap();
        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(
            seq["interval"].as_f64().unwrap(),
            2.5,
            "supplied option resolves to 2.5"
        );

        // When omitted, the default applies.
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "w", "type": "mydef"}"#).unwrap();
        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(
            seq["interval"].as_f64().unwrap(),
            1.5,
            "default resolves to 1.5"
        );
    }

    #[test]
    fn wrap_substitutes_def_skip_if_exists_placeholder() {
        // Definition-level skip_if_exists can reference call-site options.
        let definition = serde_json::from_str::<serde_json::Value>(
            r#"{
                "skip_if_exists": "{{options.guard_uri | default('artifact://sentinel-default')}}",
                "stages": [
                    {"name": "inner", "type": "agent", "prompt": ["hi\n"]}
                ]
            }"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert("mydef".to_string(), definition);

        let call_site = serde_json::from_str::<serde_json::Value>(
            r#"{"name": "w", "type": "mydef", "options": {"guard_uri": "artifact://sentinel"}}"#,
        )
        .unwrap();
        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(
            seq["skip_if_exists"].as_str().unwrap(),
            "artifact://sentinel",
            "supplied option resolves correctly"
        );

        // When omitted, the default applies.
        let call_site =
            serde_json::from_str::<serde_json::Value>(r#"{"name": "w", "type": "mydef"}"#).unwrap();
        let result = expand_stage_def(&call_site, "mydef", &defs).unwrap();
        let seq = &result[0];
        assert_eq!(
            seq["skip_if_exists"].as_str().unwrap(),
            "artifact://sentinel-default",
            "default resolves correctly"
        );
    }
}
