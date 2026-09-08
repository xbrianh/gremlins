use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::assets;
use crate::schemas::error::SchemaError;
use crate::schemas::prompts;
use crate::schemas::resolve::BuiltinResolver;

pub const GREMLINS_PREFIX: &str = "gremlins:";

/// Trait for resolving pipeline names to file paths.
/// The pyext layer provides a Python-callback implementation.
pub trait PipelineResolver {
    fn resolve(&self, name: &str, project_root: &std::path::Path) -> Result<PathBuf, SchemaError>;
}

pub fn load_yaml_file(path: &Path) -> Result<serde_yaml::Value, SchemaError> {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SchemaError::PipelineFileNotFound {
            path: path.display().to_string(),
        },
        _ => SchemaError::Generic(format!("could not read {}: {}", path.display(), e)),
    })?;
    let parsed: serde_yaml::Value =
        serde_yaml::from_str(&text).map_err(|e| SchemaError::YamlParse {
            label: path.display().to_string(),
            msg: e.to_string(),
        })?;
    if !parsed.is_mapping() {
        return Err(SchemaError::YamlNotMapping {
            label: path.display().to_string(),
            got: format!("{:?}", parsed),
        });
    }
    Ok(parsed)
}

pub fn load_bundled_recipe(raw_name: &str) -> Result<serde_yaml::Value, SchemaError> {
    let name = raw_name.replace('-', "_");
    let yaml_str = assets::RECIPES.get(name.as_str()).ok_or_else(|| {
        let mut available: Vec<_> = assets::RECIPES.keys().copied().collect();
        available.sort();
        SchemaError::BundledRecipeNotFound {
            name: format!("{GREMLINS_PREFIX}{raw_name}"),
            available: available.join(", "),
        }
    })?;
    serde_yaml::from_str(yaml_str).map_err(|e| SchemaError::YamlParse {
        label: format!("gremlins:{raw_name}"),
        msg: e.to_string(),
    })
}

pub fn resolve_prompt_dir(
    value: Option<&serde_yaml::Value>,
    yaml_dir: &std::path::Path,
) -> Result<PathBuf, SchemaError> {
    match value {
        None => Ok(PathBuf::from(yaml_dir)),
        Some(v) => {
            if let Some(s) = v.as_str() {
                let p = PathBuf::from(s);
                if p.is_absolute() {
                    Ok(p)
                } else {
                    Ok(yaml_dir.join(&p))
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

pub fn project_stage_def_dir(project_root: &Path) -> PathBuf {
    crate::config::project_overlay_dir(project_root).join("stages")
}

pub fn stage_definition_dirs_with_project(project_root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project_stage_def_dir(project_root)];
    dirs.extend(crate::config::stage_definition_dirs());
    dirs
}

pub fn load_stage_def_from_dirs(
    name: &str,
    project_root: Option<&Path>,
) -> Result<Option<serde_yaml::Value>, SchemaError> {
    let dirs: Vec<PathBuf> = if let Some(pr) = project_root {
        stage_definition_dirs_with_project(pr)
    } else {
        crate::config::stage_definition_dirs()
    };
    for d in &dirs {
        let candidate = d.join(format!("{}.yaml", name));
        if candidate.exists() {
            return match load_yaml_file(&candidate) {
                Ok(recipe) => Ok(Some(recipe)),
                Err(e) => Err(e),
            };
        }
    }
    Ok(None)
}

pub fn parse_stage_definitions(
    raw: Option<&serde_yaml::Value>,
    project_root: Option<&PathBuf>,
) -> Result<HashMap<String, serde_yaml::Value>, SchemaError> {
    let mut defs: HashMap<String, serde_yaml::Value> = HashMap::new();
    match raw {
        None => {}
        Some(v) if v.is_mapping() => {
            let mapping = v.as_mapping().unwrap();
            for (k, v) in mapping {
                let name = k.as_str().unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                if let Some(s) = v.as_str() {
                    if let Some(recipe_name) = s.strip_prefix(GREMLINS_PREFIX) {
                        if recipe_name.is_empty() {
                            return Err(SchemaError::StageDef {
                                name: name.clone(),
                                msg: format!("missing name after {GREMLINS_PREFIX:?}"),
                            });
                        }
                        match load_bundled_recipe(recipe_name) {
                            Ok(recipe) => {
                                defs.insert(name, recipe);
                            }
                            Err(err) => match &err {
                                SchemaError::BundledRecipeNotFound { .. } => return Err(err),
                                _ => {
                                    return Err(SchemaError::StageDef {
                                        name: name.clone(),
                                        msg: err.to_string(),
                                    });
                                }
                            },
                        }
                    } else {
                        match load_stage_def_from_dirs(s, project_root.map(|p| p.as_path()))? {
                            Some(recipe) => {
                                defs.insert(name.clone(), recipe);
                            }
                            None => {
                                return Err(SchemaError::StageDef {
                                    name: name.clone(),
                                    msg: format!("must be a dict, gremlins: reference, or file under stages/; tried {s:?}"),
                                });
                            }
                        }
                    }
                } else if v.is_mapping() {
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

pub fn substitute_recipe(
    node: &serde_yaml::Value,
    ctx: &serde_yaml::Value,
) -> Result<serde_yaml::Value, SchemaError> {
    match node {
        serde_yaml::Value::Mapping(m) => {
            let mut out = serde_yaml::Mapping::new();
            for (k, v) in m {
                out.insert(k.clone(), substitute_recipe(v, ctx)?);
            }
            Ok(serde_yaml::Value::Mapping(out))
        }
        serde_yaml::Value::Sequence(seq) => {
            let mut out: Vec<serde_yaml::Value> = Vec::new();
            for item in seq {
                if let serde_yaml::Value::String(s) = item {
                    if s.starts_with("{{") && s.ends_with("}}") && s.matches("{{").count() == 1 {
                        let key = s[2..s.len() - 2].trim();
                        match resolve_placeholder(key, ctx) {
                            Ok(resolved) => {
                                if let serde_yaml::Value::Sequence(resolved_seq) = resolved {
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
            Ok(serde_yaml::Value::Sequence(out))
        }
        serde_yaml::Value::String(s) => {
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
                        Ok(serde_yaml::Value::Sequence(seq)) => seq
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(" && "),
                        Ok(val) => val_to_string(&val),
                        Err(_) => caps[0].to_string(),
                    }
                });
                Ok(serde_yaml::Value::String(result.into_owned()))
            } else {
                Ok(node.clone())
            }
        }
        _ => Ok(node.clone()),
    }
}

fn val_to_string(val: &serde_yaml::Value) -> String {
    match val {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Null => "null".to_string(),
        other => format!("{other:?}"),
    }
}

pub fn resolve_placeholder(
    key: &str,
    ctx: &serde_yaml::Value,
) -> Result<serde_yaml::Value, String> {
    let (dotted_key, has_default, default_val) = if let Some(idx) = key.find(" | default(") {
        let raw_default = &key[idx + " | default(".len()..];
        let raw_default = raw_default.strip_suffix(')').unwrap_or(raw_default);
        let default = parse_default(raw_default);
        (key[..idx].trim(), true, default)
    } else {
        (key.trim(), false, serde_yaml::Value::Null)
    };

    let parts: Vec<&str> = dotted_key.split('.').collect();
    let mut val = ctx;
    for part in &parts {
        match val.as_mapping().and_then(|m| m.get(*part)) {
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
        serde_yaml::Value::Mapping(_) | serde_yaml::Value::Sequence(_) => Ok(val.clone()),
        serde_yaml::Value::String(s) => Ok(serde_yaml::Value::String(s.clone())),
        serde_yaml::Value::Number(n) => Ok(serde_yaml::Value::String(n.to_string())),
        serde_yaml::Value::Bool(b) => Ok(serde_yaml::Value::String(b.to_string())),
        serde_yaml::Value::Null => Ok(serde_yaml::Value::String("null".to_string())),
        other => Ok(serde_yaml::Value::String(format!("{other:?}"))),
    }
}

pub fn parse_default(raw: &str) -> serde_yaml::Value {
    let s = raw.trim();
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if first == last && (first == '"' || first == '\'') {
            return serde_yaml::Value::String(s[1..s.len() - 1].to_string());
        }
    }
    serde_yaml::Value::String(s.to_string())
}

/// Validate that every key declared in each stage's `bind:` or `interpolation:`
/// map is actually referenced as `{KEY}`, `$KEY`, or `${KEY}` somewhere in the
/// stage's prompts or commands. Also catches keys declared in both maps.
/// Stages whose `type` is a bundled recipe (gremlins:xxx or a bare name that
/// resolves to a bundled recipe) are skipped because their keys are used
/// internally by the recipe.
pub fn validate_stage_keys(expanded_yaml: &serde_yaml::Value) -> Result<(), Vec<SchemaError>> {
    let mut errors = Vec::new();

    // Validate the `land` stage if present
    if let Some(land) = expanded_yaml.get("land") {
        validate_stage_keys_for_stage(land, &mut errors);
    }

    // Validate each stage in `stages`
    if let Some(stages) = expanded_yaml.get("stages").and_then(|v| v.as_sequence()) {
        for stage in stages {
            validate_stage_keys_for_stage(stage, &mut errors);
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_stage_keys_for_stage(stage: &serde_yaml::Value, errors: &mut Vec<SchemaError>) {
    let mapping = match stage.as_mapping() {
        Some(m) => m,
        None => return,
    };

    let stage_name = mapping.get("name").and_then(|v| v.as_str()).unwrap_or("?");

    let bind_map = mapping.get("bind").and_then(|v| v.as_mapping());
    let interp_map = mapping.get("interpolation").and_then(|v| v.as_mapping());

    // Nothing to check if neither map exists
    if bind_map.is_none() && interp_map.is_none() {
        return;
    }

    // Check for collisions: keys appearing in both bind: and interpolation:.
    // The trailing `?` on optional bind keys is stripped by the runtime, so
    // `foo?` in bind collides with `foo` in interpolation.
    let mut colliding_keys: HashSet<String> = HashSet::new();
    if let (Some(bind), Some(interp)) = (&bind_map, &interp_map) {
        let bind_keys: HashSet<String> = bind
            .keys()
            .filter_map(|k| k.as_str())
            .map(|k| k.strip_suffix('?').unwrap_or(k).to_string())
            .collect();
        let interp_keys: HashSet<&str> = interp.keys().filter_map(|k| k.as_str()).collect();
        for interp_key in &interp_keys {
            if bind_keys.contains(*interp_key) {
                colliding_keys.insert(interp_key.to_string());
                errors.push(SchemaError::DuplicateStageKey {
                    stage: stage_name.to_string(),
                    key: interp_key.to_string(),
                });
            }
        }
    }

    // Collect keys from both bind: and interpolation: — all must be referenced.
    // Skip keys that contain `{...}` templates (these are framework substitution
    // variables resolved at runtime, e.g. `{name}`, `{model}`).
    let mut keys: Vec<(String, String)> = Vec::new(); // (key, map_name)
    if let Some(interp) = interp_map {
        for key in interp.keys() {
            if let Some(k) = key.as_str() {
                if !colliding_keys.contains(k) && !k.contains('{') {
                    keys.push((k.to_string(), "interpolation".to_string()));
                }
            }
        }
    }
    if let Some(bind) = bind_map {
        for key in bind.keys() {
            if let Some(k) = key.as_str() {
                if !colliding_keys.contains(k) && !k.contains('{') {
                    keys.push((k.to_string(), "bind".to_string()));
                }
            }
        }
    }

    // Collect all text to search
    let mut text = String::new();

    // Own prompts
    if let Some(prompts) = mapping.get("prompt").and_then(|v| v.as_sequence()) {
        for p in prompts {
            if let Some(s) = p.as_str() {
                text.push_str(s);
                text.push('\n');
            }
        }
    }

    // Own commands
    if let Some(options) = mapping.get("options").and_then(|v| v.as_mapping()) {
        if let Some(cmds) = options.get("cmds").and_then(|v| v.as_sequence()) {
            for cmd in cmds {
                if let Some(s) = cmd.as_str() {
                    text.push_str(s);
                    text.push('\n');
                }
            }
        }
    }

    // Collect text from body children
    if let Some(body) = mapping.get("body").and_then(|v| v.as_sequence()) {
        for child in body {
            collect_stage_text(child, &mut text);
        }
    }

    // Collect text from parallel children
    if let Some(parallel) = mapping.get("parallel").and_then(|v| v.as_sequence()) {
        for child in parallel {
            collect_stage_text(child, &mut text);
        }
    }

    for (key_str, map_name) in &keys {
        if key_referenced_in_text(key_str, &text) {
            continue;
        }
        // For bind keys with trailing `?`, the exec stage runtime strips the
        // `?` before substitution, so also check the un-suffixed form.
        if map_name == "bind" && key_str.ends_with('?') {
            let stripped = &key_str[..key_str.len() - 1];
            if key_referenced_in_text(stripped, &text) {
                continue;
            }
        }

        errors.push(SchemaError::UnusedStageKey {
            stage: stage_name.to_string(),
            key: key_str.to_string(),
            map: map_name.clone(),
        });
    }
}

/// Check whether a key appears in the stage's text as `{KEY}`, `$KEY`, or `${KEY}`.
fn key_referenced_in_text(key_str: &str, text: &str) -> bool {
    // Check for {KEY}
    let brace_form = format!("{{{key_str}}}");
    if text.contains(&brace_form) {
        return true;
    }
    // Check for ${KEY}
    let dollar_brace_form = String::from("${") + key_str;
    if text.contains(&dollar_brace_form) {
        return true;
    }
    // Check $KEY — must not be followed by an identifier-continuation character
    let dollar_form = format!("${key_str}");
    if text.contains(&dollar_form) {
        let mut pos = 0;
        while let Some(idx) = text[pos..].find(&dollar_form) {
            let abs_idx = pos + idx;
            let after = abs_idx + dollar_form.len();
            if after >= text.len()
                || !text
                    .as_bytes()
                    .get(after)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                return true;
            }
            pos = after;
        }
    }
    false
}

/// Recursively collect all prompt and command text from a stage and its descendants.
fn collect_stage_text(stage: &serde_yaml::Value, out: &mut String) {
    let mapping = match stage.as_mapping() {
        Some(m) => m,
        None => return,
    };

    if let Some(prompts) = mapping.get("prompt").and_then(|v| v.as_sequence()) {
        for p in prompts {
            if let Some(s) = p.as_str() {
                out.push_str(s);
                out.push('\n');
            }
        }
    }

    if let Some(options) = mapping.get("options").and_then(|v| v.as_mapping()) {
        if let Some(cmds) = options.get("cmds").and_then(|v| v.as_sequence()) {
            for cmd in cmds {
                if let Some(s) = cmd.as_str() {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
    }

    if let Some(body) = mapping.get("body").and_then(|v| v.as_sequence()) {
        for child in body {
            collect_stage_text(child, out);
        }
    }

    if let Some(parallel) = mapping.get("parallel").and_then(|v| v.as_sequence()) {
        for child in parallel {
            collect_stage_text(child, out);
        }
    }
}

/// Parse a pipeline YAML file from disk, expanding includes, stage-definitions,
/// and prompts. Returns the fully expanded YAML tree.
pub fn parse_pipeline_file(
    yaml_path: &Path,
    project_root: &Path,
) -> Result<serde_yaml::Value, SchemaError> {
    let resolver = BuiltinResolver;
    let expanded = expand_pipeline(yaml_path, Some(project_root), &resolver)?;

    // Validate bind: and interpolation: keys are referenced
    if let Err(errors) = validate_stage_keys(&expanded) {
        return Err(errors.into_iter().next().unwrap());
    }

    Ok(expanded)
}

#[allow(clippy::too_many_arguments)]
pub fn expand_pipeline(
    yaml_path: &Path,
    project_root: Option<&Path>,
    resolver: &dyn PipelineResolver,
) -> Result<serde_yaml::Value, SchemaError> {
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
    _expand(yaml_path, &project_root, &chain, resolver)
}

#[allow(clippy::too_many_arguments)]
fn _expand(
    yaml_path: &Path,
    project_root: &PathBuf,
    chain: &[PathBuf],
    resolver: &dyn PipelineResolver,
) -> Result<serde_yaml::Value, SchemaError> {
    let resolved = yaml_path
        .canonicalize()
        .unwrap_or_else(|_| yaml_path.to_path_buf());
    if chain.contains(&resolved) {
        let mut cycle_parts: Vec<String> = chain.iter().map(|p| p.display().to_string()).collect();
        cycle_parts.push(resolved.display().to_string());
        return Err(SchemaError::IncludeCycle(cycle_parts.join(" -> ")));
    }

    let raw = load_yaml_file(yaml_path)?;
    let raw_mapping = raw.as_mapping().unwrap();

    if raw_mapping
        .get("__gremlins_expanded__")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let mut result = raw.clone();
        if let Some(m) = result.as_mapping_mut() {
            m.remove("__gremlins_expanded__");
        }
        return Ok(result);
    }

    let yaml_dir = yaml_path.parent().unwrap_or(yaml_path);

    let prompt_dir = resolve_prompt_dir(raw_mapping.get("prompt_dir"), yaml_dir)?;

    let new_chain: Vec<PathBuf> = chain
        .iter()
        .chain(std::iter::once(&resolved))
        .cloned()
        .collect();

    let named_prompts = prompts::parse_named_prompts(raw_mapping.get("prompts"), &prompt_dir)?;

    let stage_defs =
        parse_stage_definitions(raw_mapping.get("stage-definitions"), Some(project_root))?;

    let stages_raw = raw_mapping.get("stages");
    let stages_list: Vec<serde_yaml::Value> = match stages_raw {
        None | Some(serde_yaml::Value::Null) => Vec::new(),
        Some(v) if v.is_sequence() => v.as_sequence().cloned().unwrap_or_default(),
        Some(_v) => {
            return Err(SchemaError::Generic("'stages' must be a list".to_string()));
        }
    };

    let mut expanded_stages: Vec<serde_yaml::Value> = Vec::new();
    for entry in stages_list {
        let expanded = _expand_entry(
            &entry,
            &prompt_dir,
            project_root,
            &new_chain,
            &named_prompts,
            &stage_defs,
            &HashSet::new(),
            resolver,
        )?;
        expanded_stages.extend(expanded);
    }

    let mut result = serde_yaml::Mapping::new();
    for (k, v) in raw_mapping {
        let key_str = k.as_str().unwrap_or("");
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
        serde_yaml::Value::String("stages".to_string()),
        serde_yaml::Value::Sequence(expanded_stages),
    );

    Ok(serde_yaml::Value::Mapping(result))
}

#[allow(clippy::too_many_arguments)]
fn _expand_entry(
    entry: &serde_yaml::Value,
    prompt_dir: &PathBuf,
    project_root: &PathBuf,
    chain: &[PathBuf],
    named_prompts: &HashMap<String, Vec<String>>,
    stage_defs: &HashMap<String, serde_yaml::Value>,
    seen_defs: &HashSet<String>,
    resolver: &dyn PipelineResolver,
) -> Result<Vec<serde_yaml::Value>, SchemaError> {
    let mapping = match entry.as_mapping() {
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
        let included = _expand(&included_path, project_root, chain, resolver)?;
        let stages = match included.get("stages") {
            Some(serde_yaml::Value::Sequence(s)) => s.clone(),
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
                chain,
                named_prompts,
                seen_defs,
                resolver,
            );
        }
        if let Some(recipe_name) = stage_type.strip_prefix(GREMLINS_PREFIX) {
            if recipe_name.is_empty() {
                return Err(SchemaError::Generic(format!(
                    "missing name after {GREMLINS_PREFIX:?}"
                )));
            }
            match load_bundled_recipe(recipe_name) {
                Ok(recipe_def) => {
                    let mut direct_defs = stage_defs.clone();
                    direct_defs.insert(stage_type.to_string(), recipe_def);
                    return _expand_stage_def(
                        entry,
                        stage_type,
                        &direct_defs,
                        prompt_dir,
                        project_root,
                        chain,
                        named_prompts,
                        seen_defs,
                        resolver,
                    );
                }
                Err(SchemaError::BundledRecipeNotFound { .. }) => {
                    // Not a bundled recipe — try stage definition directories.
                    if let Some(recipe) = load_stage_def_from_dirs(recipe_name, Some(project_root))?
                    {
                        let mut direct_defs = stage_defs.clone();
                        direct_defs.insert(stage_type.to_string(), recipe);
                        return _expand_stage_def(
                            entry,
                            stage_type,
                            &direct_defs,
                            prompt_dir,
                            project_root,
                            chain,
                            named_prompts,
                            seen_defs,
                            resolver,
                        );
                    }
                    // Not found anywhere — raise the original error
                    return Err(SchemaError::BundledRecipeNotFound {
                        name: format!("{GREMLINS_PREFIX}{recipe_name}"),
                        available: assets::RECIPES
                            .keys()
                            .copied()
                            .collect::<Vec<_>>()
                            .join(", "),
                    });
                }
                Err(e) => return Err(e),
            }
        }
        // Auto-resolve bundled stage-definitions by type name
        let underscored = stage_type.replace('-', "_");
        if assets::RECIPES.contains_key(underscored.as_str()) {
            let auto_def = load_bundled_recipe(underscored.as_str())?;
            let mut auto_defs = stage_defs.clone();
            auto_defs.insert(stage_type.to_string(), auto_def);
            return _expand_stage_def(
                entry,
                stage_type,
                &auto_defs,
                prompt_dir,
                project_root,
                chain,
                named_prompts,
                seen_defs,
                resolver,
            );
        }
        // Try resolving as pipeline name
        let pipeline_result = resolver.resolve(stage_type, project_root);
        match pipeline_result {
            Ok(included_path) => {
                if !chain.contains(&included_path) {
                    let included = _expand(&included_path, project_root, chain, resolver)?;
                    let stages = match included.get("stages") {
                        Some(serde_yaml::Value::Sequence(s)) => s.clone(),
                        _ => Vec::new(),
                    };
                    return Ok(stages);
                }
            }
            Err(SchemaError::PipelineNotFound { .. }) => {
                // Not a pipeline — try stage definition directories.
                if let Some(recipe) = load_stage_def_from_dirs(stage_type, Some(project_root))? {
                    let mut direct_defs = stage_defs.clone();
                    direct_defs.insert(stage_type.to_string(), recipe);
                    return _expand_stage_def(
                        entry,
                        stage_type,
                        &direct_defs,
                        prompt_dir,
                        project_root,
                        chain,
                        named_prompts,
                        seen_defs,
                        resolver,
                    );
                }
                // Not found anywhere — fall through to loader validation
            }
            Err(e) => return Err(e),
        }
    }

    let mut entry = entry.clone();
    let entry_map = entry.as_mapping_mut().unwrap();

    if entry_map.contains_key("prompt") {
        let prompt_val = entry_map.get("prompt").unwrap().clone();
        let texts = prompts::read_prompts(&prompt_val, prompt_dir, named_prompts)?;
        entry_map.insert(
            serde_yaml::Value::String("prompt".to_string()),
            serde_yaml::Value::Sequence(texts.into_iter().map(serde_yaml::Value::String).collect()),
        );
    }

    if let Some(parallel_val) = entry_map.get("parallel") {
        if let Some(parallel_list) = parallel_val.as_sequence() {
            let mut expanded_parallel: Vec<serde_yaml::Value> = Vec::new();
            for child in parallel_list {
                let child_dict = child.as_mapping();
                let include_name = child_dict
                    .filter(|m| m.len() == 1)
                    .and_then(|m| m.get("include"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let expanded = _expand_entry(
                    child,
                    prompt_dir,
                    project_root,
                    chain,
                    named_prompts,
                    stage_defs,
                    seen_defs,
                    resolver,
                )?;

                if expanded.is_empty() {
                    return Err(SchemaError::Generic(
                        "parallel child expanded to 0 stages via include; includes inside parallel groups must resolve to at least one stage".to_string()
                    ));
                }
                if expanded.len() == 1 {
                    expanded_parallel.push(expanded.into_iter().next().unwrap());
                } else {
                    let name = include_name
                        .unwrap_or_else(|| format!("sequence-{}", expanded_parallel.len()));
                    let mut seq = serde_yaml::Mapping::new();
                    seq.insert(
                        serde_yaml::Value::String("name".to_string()),
                        serde_yaml::Value::String(name),
                    );
                    seq.insert(
                        serde_yaml::Value::String("type".to_string()),
                        serde_yaml::Value::String("sequence".to_string()),
                    );
                    seq.insert(
                        serde_yaml::Value::String("body".to_string()),
                        serde_yaml::Value::Sequence(expanded),
                    );
                    expanded_parallel.push(serde_yaml::Value::Mapping(seq));
                }
            }
            entry_map.insert(
                serde_yaml::Value::String("parallel".to_string()),
                serde_yaml::Value::Sequence(expanded_parallel),
            );
        }
    }

    if let Some(body_val) = entry_map.get("body") {
        if let Some(body_list) = body_val.as_sequence() {
            let mut expanded_body: Vec<serde_yaml::Value> = Vec::new();
            for body_entry in body_list {
                let expanded = _expand_entry(
                    body_entry,
                    prompt_dir,
                    project_root,
                    chain,
                    named_prompts,
                    stage_defs,
                    seen_defs,
                    resolver,
                )?;
                expanded_body.extend(expanded);
            }
            entry_map.insert(
                serde_yaml::Value::String("body".to_string()),
                serde_yaml::Value::Sequence(expanded_body),
            );
        }
    }

    Ok(vec![entry])
}

#[allow(clippy::too_many_arguments)]
fn _expand_stage_def(
    call_site: &serde_yaml::Value,
    def_name: &str,
    stage_defs: &HashMap<String, serde_yaml::Value>,
    prompt_dir: &PathBuf,
    project_root: &PathBuf,
    chain: &[PathBuf],
    named_prompts: &HashMap<String, Vec<String>>,
    seen_defs: &HashSet<String>,
    resolver: &dyn PipelineResolver,
) -> Result<Vec<serde_yaml::Value>, SchemaError> {
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

    let def_map = definition.as_mapping().ok_or_else(|| {
        SchemaError::Generic(format!("stage-definition {def_name:?} is not a mapping"))
    })?;

    let call_site_map = call_site.as_mapping().unwrap();

    if let Some(inner_list) = def_map.get("stages").and_then(|v| v.as_sequence()) {
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

        let last_idx = inner_list.len() - 1;
        let required_opts: Vec<String> = def_map
            .get("required-options")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let cs_opts: HashMap<String, serde_yaml::Value> = call_site_map
            .get("options")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.as_str().unwrap_or("").to_string(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();

        for opt in &required_opts {
            let val = cs_opts.get(opt);
            let is_empty = match val {
                None => true,
                Some(serde_yaml::Value::Sequence(s)) => s.is_empty(),
                Some(serde_yaml::Value::Null) => true,
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

        let mut ctx = serde_yaml::Mapping::new();
        if let Some(name) = call_site_map.get("name").and_then(|v| v.as_str()) {
            ctx.insert(
                serde_yaml::Value::String("name".to_string()),
                serde_yaml::Value::String(name.to_string()),
            );
        }
        ctx.insert(
            serde_yaml::Value::String("options".to_string()),
            serde_yaml::Value::Mapping(
                cs_opts
                    .into_iter()
                    .map(|(k, v)| (serde_yaml::Value::String(k), v))
                    .collect(),
            ),
        );
        ctx.insert(
            serde_yaml::Value::String("prompt".to_string()),
            serde_yaml::Value::Sequence(
                cs_prompts
                    .iter()
                    .map(|s| serde_yaml::Value::String(s.clone()))
                    .collect(),
            ),
        );

        let ctx_value = serde_yaml::Value::Mapping(ctx);

        let mut result: Vec<serde_yaml::Value> = Vec::new();
        for (i, raw_inner) in inner_list.iter().enumerate() {
            let substituted = substitute_recipe(raw_inner, &ctx_value)?;
            let mut inner = substituted.clone();
            if !inner.is_mapping() {
                return Err(SchemaError::StageDef {
                    name: def_name.to_string(),
                    msg: format!("inner stage {i} must be a mapping, got {:?}", inner),
                });
            }
            let inner_map = inner.as_mapping_mut().unwrap();

            if i == 0 {
                if let Some(name) = call_site_map.get("name") {
                    inner_map.insert(serde_yaml::Value::String("name".to_string()), name.clone());
                } else if inner_map.contains_key("name") {
                    let existing_name = inner_map.remove("name").unwrap();
                    inner_map.insert(
                        serde_yaml::Value::String("_auto_name".to_string()),
                        existing_name,
                    );
                }
                if let Some(client) = call_site_map.get("client") {
                    inner_map.insert(
                        serde_yaml::Value::String("client".to_string()),
                        client.clone(),
                    );
                }
                if let Some(interpolation_val) = call_site_map.get("interpolation") {
                    let mut merged_interpolation = inner_map
                        .get("interpolation")
                        .and_then(|v| v.as_mapping())
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<serde_yaml::Mapping>()
                        })
                        .unwrap_or_default();
                    if let Some(cs_interpolation) = interpolation_val.as_mapping() {
                        for (k, v) in cs_interpolation {
                            merged_interpolation.insert(k.clone(), v.clone());
                        }
                    }
                    inner_map.insert(
                        serde_yaml::Value::String("interpolation".to_string()),
                        serde_yaml::Value::Mapping(merged_interpolation),
                    );
                }
            }
            if i == last_idx {
                if let Some(bind_val) = call_site_map.get("bind") {
                    if inner_map.contains_key("bind") {
                        return Err(SchemaError::StageDef {
                            name: def_name.to_string(),
                            msg: format!(
                                "inner stage {i} declares 'bind:'; call-site must not also declare 'bind:'"
                            ),
                        });
                    }
                    inner_map.insert(
                        serde_yaml::Value::String("bind".to_string()),
                        bind_val.clone(),
                    );
                }
            }

            let expanded = _expand_entry(
                &inner,
                prompt_dir,
                project_root,
                chain,
                named_prompts,
                stage_defs,
                &new_seen,
                resolver,
            )?;
            result.extend(expanded);
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

    let mut merged = definition.clone();
    let merged_map = merged.as_mapping_mut().unwrap();

    for key in &["name", "interpolation", "bind"] {
        if let Some(v) = call_site_map.get(*key) {
            merged_map.insert(serde_yaml::Value::String(key.to_string()), v.clone());
        }
    }
    if !call_site_map.contains_key("name") && merged_map.contains_key("name") {
        let existing_name = merged_map.remove("name").unwrap();
        merged_map.insert(
            serde_yaml::Value::String("_auto_name".to_string()),
            existing_name,
        );
    }

    _expand_entry(
        &merged,
        prompt_dir,
        project_root,
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
    fn test_serde_yaml_preserves_loop_iter_in_quoted_string() {
        // Regression: serde_yaml 0.9 (YAML 1.2) must preserve {loop_iter}
        // inside double-quoted strings.
        let yaml_str = r#"
stages:
  - name: verify
    type: loop
    stop_when_exists: "artifact://{loop_iter}/done"
    body:
      - name: fix
        type: agent
        skip_if_exists: "artifact://{loop_iter}/done"
"#;
        let parsed: serde_yaml::Value = serde_yaml::from_str(yaml_str).unwrap();
        let stages = parsed["stages"].as_sequence().unwrap();
        let loop_stage = &stages[0];
        let sw = loop_stage["stop_when_exists"].as_str().unwrap();
        assert_eq!(
            sw, "artifact://{loop_iter}/done",
            "stop_when_exists should preserve {{loop_iter}}"
        );
        let body = loop_stage["body"].as_sequence().unwrap();
        let fix = &body[0];
        let skip = fix["skip_if_exists"].as_str().unwrap();
        assert_eq!(
            skip, "artifact://{loop_iter}/done",
            "skip_if_exists should preserve {{loop_iter}}"
        );
    }

    #[test]
    fn test_verify_recipe_skip_if_exists_preserves_loop_iter() {
        let recipe = load_bundled_recipe("verify").unwrap();
        let stages = recipe["stages"].as_sequence().unwrap();
        let loop_stage = &stages[0];
        let body = loop_stage["body"].as_sequence().unwrap();
        let fix = &body[1];
        let skip = fix["skip_if_exists"].as_str().unwrap();
        assert_eq!(
            skip, "artifact://{loop_iter}/done",
            "Raw verify recipe skip_if_exists should preserve {{loop_iter}}"
        );
    }

    #[test]
    fn test_substitute_recipe_simple() {
        let mut ctx_map = serde_yaml::Mapping::new();
        ctx_map.insert(
            serde_yaml::Value::String("options".to_string()),
            serde_yaml::Value::Mapping({
                let mut m = serde_yaml::Mapping::new();
                m.insert(
                    serde_yaml::Value::String("key".to_string()),
                    serde_yaml::Value::String("value".to_string()),
                );
                m
            }),
        );
        let ctx = serde_yaml::Value::Mapping(ctx_map);

        let input = serde_yaml::Value::String("{{options.key}}".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "value");
    }

    #[test]
    fn test_substitute_recipe_default() {
        let ctx = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let input =
            serde_yaml::Value::String("{{options.missing | default(fallback)}}".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "fallback");
    }

    #[test]
    fn test_substitute_recipe_missing_placeholder_errors() {
        let ctx = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let input = serde_yaml::Value::String("{{options.missing}}".to_string());
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
        let mut ctx_map = serde_yaml::Mapping::new();
        ctx_map.insert(
            serde_yaml::Value::String("options".to_string()),
            serde_yaml::Value::Mapping({
                let mut m = serde_yaml::Mapping::new();
                m.insert(
                    serde_yaml::Value::String("key".to_string()),
                    serde_yaml::Value::String("value".to_string()),
                );
                m
            }),
        );
        let ctx = serde_yaml::Value::Mapping(ctx_map);
        let input = serde_yaml::Value::String("foo {{options.key}} bar".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "foo value bar");
    }

    #[test]
    fn test_substitute_recipe_list_join() {
        let mut ctx_map = serde_yaml::Mapping::new();
        ctx_map.insert(
            serde_yaml::Value::String("options".to_string()),
            serde_yaml::Value::Mapping({
                let mut m = serde_yaml::Mapping::new();
                m.insert(
                    serde_yaml::Value::String("cmds".to_string()),
                    serde_yaml::Value::Sequence(vec![
                        serde_yaml::Value::String("cmd1".to_string()),
                        serde_yaml::Value::String("cmd2".to_string()),
                    ]),
                );
                m
            }),
        );
        let ctx = serde_yaml::Value::Mapping(ctx_map);
        let input = serde_yaml::Value::String("run {{options.cmds}} please".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "run cmd1 && cmd2 please");
    }

    #[test]
    fn test_substitute_recipe_unresolved_verbatim() {
        let ctx = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let input = serde_yaml::Value::String("hello {{missing}} world".to_string());
        let result = substitute_recipe(&input, &ctx).unwrap();
        assert_eq!(result.as_str().unwrap(), "hello {{missing}} world");
    }

    // --- validate_stage_keys tests ---

    #[test]
    fn test_bind_key_found_in_prompt() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    bind:
      foo: artifact://x
    prompt:
      - "use {foo}"
"#,
        )
        .unwrap();
        assert!(validate_stage_keys(&yaml).is_ok());
    }

    #[test]
    fn test_interpolation_key_found_in_command() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    interpolation:
      bar: content(...)
    options:
      cmds:
        - "echo ${bar}"
"#,
        )
        .unwrap();
        assert!(validate_stage_keys(&yaml).is_ok());
    }

    #[test]
    fn test_bind_key_not_referenced() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    bind:
      orphan: artifact://z
    prompt:
      - "hello"
"#,
        )
        .unwrap();
        let errs = validate_stage_keys(&yaml).unwrap_err();
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            SchemaError::UnusedStageKey { key, map, .. } => {
                assert_eq!(key, "orphan");
                assert_eq!(map, "bind");
            }
            _ => panic!("expected UnusedStageKey"),
        }
    }

    #[test]
    fn test_interpolation_key_not_referenced() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    interpolation:
      orphan: content(...)
    prompt:
      - "hello"
"#,
        )
        .unwrap();
        let errs = validate_stage_keys(&yaml).unwrap_err();
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            SchemaError::UnusedStageKey { key, map, .. } => {
                assert_eq!(key, "orphan");
                assert_eq!(map, "interpolation");
            }
            _ => panic!("expected UnusedStageKey"),
        }
    }

    #[test]
    fn test_collision_between_bind_and_interpolation() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    bind:
      key: artifact://x
    interpolation:
      key: content(...)
    prompt:
      - "use {key}"
"#,
        )
        .unwrap();
        let errs = validate_stage_keys(&yaml).unwrap_err();
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            SchemaError::DuplicateStageKey { key, .. } => {
                assert_eq!(key, "key");
            }
            _ => panic!("expected DuplicateStageKey"),
        }
    }

    #[test]
    fn test_bind_key_in_body_child_text() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: parent
    bind:
      foo: artifact://x
    body:
      - name: child
        prompt:
          - "use {foo}"
"#,
        )
        .unwrap();
        assert!(validate_stage_keys(&yaml).is_ok());
    }

    #[test]
    fn test_bind_key_referenced_as_dollar_key() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    bind:
      foo: artifact://x
    options:
      cmds:
        - "echo $foo"
"#,
        )
        .unwrap();
        assert!(validate_stage_keys(&yaml).is_ok());
    }

    #[test]
    fn test_stage_with_no_bind_or_interpolation() {
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    prompt:
      - "hello"
"#,
        )
        .unwrap();
        assert!(validate_stage_keys(&yaml).is_ok());
    }

    #[test]
    fn test_bind_key_with_trailing_question_mark_in_prompt() {
        // Agent stages use the literal key (with ?) in prompts.
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    bind:
      foo?: artifact://x
    prompt:
      - "use {foo?}"
"#,
        )
        .unwrap();
        assert!(validate_stage_keys(&yaml).is_ok());
    }

    #[test]
    fn test_bind_key_with_trailing_question_mark_in_cmd() {
        // Exec stages strip the `?` before substitution, so commands reference
        // the un-suffixed key.
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    bind:
      foo?: artifact://x
    options:
      cmds:
        - "echo {foo}"
"#,
        )
        .unwrap();
        assert!(validate_stage_keys(&yaml).is_ok());
    }

    #[test]
    fn test_collision_bind_qmark_interpolation() {
        // `foo?` in bind collides with `foo` in interpolation (runtime strips `?`).
        let yaml = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
stages:
  - name: test
    bind:
      foo?: artifact://x
    interpolation:
      foo: content(...)
    prompt:
      - "use {foo}"
"#,
        )
        .unwrap();
        let errs = validate_stage_keys(&yaml).unwrap_err();
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            SchemaError::DuplicateStageKey { key, .. } => {
                assert_eq!(key, "foo");
            }
            _ => panic!("expected DuplicateStageKey"),
        }
    }
}
