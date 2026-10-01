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

pub(crate) fn load_yaml_file(path: &Path) -> Result<serde_yaml::Value, SchemaError> {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SchemaError::DefinitionFileNotFound {
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

pub(crate) fn resolve_prompt_dir(
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
) -> Result<Option<serde_yaml::Value>, SchemaError> {
    let dirs: Vec<PathBuf> = if let (Some(pr), Some(od)) = (project_root, overlay_dir) {
        stage_definition_dirs_with_project(pr, od)
    } else if let Some(od) = overlay_dir {
        vec![od.join("stages")]
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

pub(crate) fn parse_stage_definitions(
    raw: Option<&serde_yaml::Value>,
    project_root: Option<&PathBuf>,
    overlay_dir: &Path,
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

pub(crate) fn substitute_recipe(
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

pub(crate) fn resolve_placeholder(
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
        serde_yaml::Value::Mapping(_)
        | serde_yaml::Value::Sequence(_)
        | serde_yaml::Value::Number(_)
        | serde_yaml::Value::Bool(_)
        | serde_yaml::Value::Null => Ok(val.clone()),
        serde_yaml::Value::String(s) => Ok(serde_yaml::Value::String(s.clone())),
        other => Ok(serde_yaml::Value::String(format!("{other:?}"))),
    }
}

pub(crate) fn parse_default(raw: &str) -> serde_yaml::Value {
    let s = raw.trim();
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if first == last && (first == '"' || first == '\'') {
            return serde_yaml::Value::String(s[1..s.len() - 1].to_string());
        }
    }
    // Try integer first, then float, then fall back to string.
    if let Ok(n) = s.parse::<i64>() {
        return serde_yaml::Value::Number(serde_yaml::Number::from(n));
    }
    if let Ok(n) = s.parse::<f64>() {
        if n.is_finite() {
            return serde_yaml::Value::Number(serde_yaml::Number::from(n));
        }
    }
    serde_yaml::Value::String(s.to_string())
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
) -> Result<serde_yaml::Value, SchemaError> {
    let resolver = BuiltinResolver;
    expand_definition(yaml_path, Some(project_root), overlay_dir, &resolver)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_definition(
    yaml_path: &Path,
    project_root: Option<&Path>,
    overlay_dir: &Path,
    resolver: &dyn DefinitionResolver,
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
    _expand(yaml_path, &project_root, overlay_dir, &chain, resolver)
}

#[allow(clippy::too_many_arguments)]
fn _expand(
    yaml_path: &Path,
    project_root: &PathBuf,
    overlay_dir: &Path,
    chain: &[PathBuf],
    resolver: &dyn DefinitionResolver,
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

    let stage_defs = parse_stage_definitions(
        raw_mapping.get("stage-definitions"),
        Some(project_root),
        overlay_dir,
    )?;

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
            overlay_dir,
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
    result.insert(
        serde_yaml::Value::String("__gremlins_expanded__".to_string()),
        serde_yaml::Value::Bool(true),
    );

    Ok(serde_yaml::Value::Mapping(result))
}

#[allow(clippy::too_many_arguments)]
fn _expand_entry(
    entry: &serde_yaml::Value,
    prompt_dir: &PathBuf,
    project_root: &PathBuf,
    overlay_dir: &Path,
    chain: &[PathBuf],
    named_prompts: &HashMap<String, Vec<String>>,
    stage_defs: &HashMap<String, serde_yaml::Value>,
    seen_defs: &HashSet<String>,
    resolver: &dyn DefinitionResolver,
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
        let included = _expand(&included_path, project_root, overlay_dir, chain, resolver)?;
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
                overlay_dir,
                chain,
                named_prompts,
                seen_defs,
                resolver,
            );
        }
        // Try stage definition directories first (e.g. .gremlins/stages/plan.yaml).
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
                        Some(serde_yaml::Value::Sequence(s)) => s.clone(),
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
    let entry_map = entry.as_mapping_mut().unwrap();

    if entry_map.contains_key("prompt") {
        let prompt_val = entry_map.get("prompt").unwrap().clone();
        let texts = prompts::read_prompts(&prompt_val, prompt_dir, named_prompts)?;
        entry_map.insert(
            serde_yaml::Value::String("prompt".to_string()),
            serde_yaml::Value::Sequence(texts.into_iter().map(serde_yaml::Value::String).collect()),
        );
    }

    let is_parallel_body = entry_map
        .get("type")
        .and_then(|v| v.as_str())
        .is_some_and(|t| t == "parallel");

    if let Some(body_val) = entry_map.get("body") {
        if let Some(body_list) = body_val.as_sequence() {
            let mut expanded_body: Vec<serde_yaml::Value> = Vec::new();
            for body_entry in body_list {
                let child_dict = body_entry.as_mapping();
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
                    expanded_body.push(serde_yaml::Value::Mapping(seq));
                }
            }
            entry_map.insert(
                serde_yaml::Value::String("body".to_string()),
                serde_yaml::Value::Sequence(expanded_body),
            );
        }
    }

    Ok(vec![entry])
}

/// Validate call-site interpolation shape before distributing to stages.
/// Rejects scalars, non-mapping values, misspelled/legacy sub-keys, and
/// non-mapping `inputs`/`outputs` values — the same validation that
/// `yaml_interpolation_nested` applies to primitive stages.
fn validate_call_site_interpolation(
    call_site_map: &serde_yaml::Mapping,
) -> Result<(), SchemaError> {
    let Some(interp_val) = call_site_map.get("interpolation").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let interp_map = interp_val
        .as_mapping()
        .ok_or_else(|| SchemaError::Generic("'interpolation' must be a mapping".to_string()))?;
    for key in interp_map.keys() {
        match key.as_str() {
            Some("inputs") | Some("outputs") => {}
            Some(other) => {
                return Err(SchemaError::Generic(format!(
                    "unknown key {other:?} in 'interpolation'; expected 'inputs' or 'outputs'"
                )));
            }
            None => {
                return Err(SchemaError::Generic(
                    "'interpolation' keys must be strings".to_string(),
                ));
            }
        }
    }
    // Validate inputs values are mappings (not scalars)
    if let Some(inputs_val) = interp_map.get("inputs").filter(|v| !v.is_null()) {
        if !inputs_val.is_mapping() {
            return Err(SchemaError::Generic(
                "'interpolation.inputs' must be a mapping".to_string(),
            ));
        }
    }
    // Validate outputs values are mappings (not scalars)
    if let Some(outputs_val) = interp_map.get("outputs").filter(|v| !v.is_null()) {
        if !outputs_val.is_mapping() {
            return Err(SchemaError::Generic(
                "'interpolation.outputs' must be a mapping".to_string(),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn _expand_stage_def(
    call_site: &serde_yaml::Value,
    def_name: &str,
    stage_defs: &HashMap<String, serde_yaml::Value>,
    prompt_dir: &PathBuf,
    project_root: &PathBuf,
    overlay_dir: &Path,
    chain: &[PathBuf],
    named_prompts: &HashMap<String, Vec<String>>,
    seen_defs: &HashSet<String>,
    resolver: &dyn DefinitionResolver,
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
        // Also reject interpolation.outputs in recipe definitions
        if let Some(interp) = def_map.get("interpolation").and_then(|v| v.as_mapping()) {
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

        // Validate call-site interpolation shape before distributing to stages
        validate_call_site_interpolation(call_site_map)?;

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
                // Merge call-site interpolation.inputs into inner stage's interpolation.inputs
                if let Some(interpolation_val) = call_site_map.get("interpolation") {
                    if let Some(m) = interpolation_val.as_mapping() {
                        if let Some(cs_inputs) = m.get("inputs").and_then(|v| v.as_mapping()) {
                            let mut merged_interpolation = inner_map
                                .get("interpolation")
                                .and_then(|v| v.as_mapping())
                                .and_then(|m| m.get("inputs"))
                                .and_then(|v| v.as_mapping())
                                .map(|im| {
                                    im.iter()
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect::<serde_yaml::Mapping>()
                                })
                                .unwrap_or_default();
                            for (k, v) in cs_inputs {
                                merged_interpolation.insert(k.clone(), v.clone());
                            }
                            let mut nested = serde_yaml::Mapping::new();
                            nested.insert(
                                serde_yaml::Value::String("inputs".to_string()),
                                serde_yaml::Value::Mapping(merged_interpolation),
                            );
                            // Preserve existing outputs if any
                            if let Some(existing_outputs) = inner_map
                                .get("interpolation")
                                .and_then(|v| v.as_mapping())
                                .and_then(|m| m.get("outputs").cloned())
                            {
                                nested.insert(
                                    serde_yaml::Value::String("outputs".to_string()),
                                    existing_outputs,
                                );
                            }
                            inner_map.insert(
                                serde_yaml::Value::String("interpolation".to_string()),
                                serde_yaml::Value::Mapping(nested),
                            );
                        }
                    }
                }
            }
            if i == last_idx {
                // Merge call-site interpolation.outputs into inner stage's interpolation.outputs
                if let Some(interp_val) = call_site_map.get("interpolation") {
                    if let Some(m) = interp_val.as_mapping() {
                        if let Some(outputs_val) = m.get("outputs") {
                            // New nested form: merge into interpolation.outputs
                            let cs_out = outputs_val.as_mapping().ok_or_else(|| {
                                SchemaError::Generic(
                                    "'interpolation.outputs' must be a mapping".to_string(),
                                )
                            })?;
                            let existing_outputs = inner_map
                                .get("interpolation")
                                .and_then(|v| v.as_mapping())
                                .and_then(|m| m.get("outputs"))
                                .and_then(|v| v.as_mapping())
                                .map(|m| {
                                    m.iter()
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect::<serde_yaml::Mapping>()
                                })
                                .unwrap_or_default();

                            let mut merged_outputs = existing_outputs;
                            for (k, v) in cs_out {
                                merged_outputs.insert(k.clone(), v.clone());
                            }

                            // Build nested interpolation mapping
                            let mut nested = serde_yaml::Mapping::new();
                            // Preserve existing inputs
                            if let Some(interp_mapping) =
                                inner_map.get("interpolation").and_then(|v| v.as_mapping())
                            {
                                if let Some(existing_inputs) = interp_mapping.get("inputs").cloned()
                                {
                                    nested.insert(
                                        serde_yaml::Value::String("inputs".to_string()),
                                        existing_inputs,
                                    );
                                }
                            }
                            nested.insert(
                                serde_yaml::Value::String("outputs".to_string()),
                                serde_yaml::Value::Mapping(merged_outputs),
                            );
                            inner_map.insert(
                                serde_yaml::Value::String("interpolation".to_string()),
                                serde_yaml::Value::Mapping(nested),
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
    if let Some(interp) = def_map.get("interpolation").and_then(|v| v.as_mapping()) {
        if interp.contains_key("outputs") {
            return Err(SchemaError::StageDef {
                name: def_name.to_string(),
                msg: "must not declare 'interpolation.outputs:' keys; declare them at each call site instead"
                    .to_string(),
            });
        }
    }

    let mut merged = definition.clone();
    let merged_map = merged.as_mapping_mut().unwrap();

    // Merge call-site keys: name, and interpolation (inputs/outputs)
    for key in &["name", "interpolation"] {
        if let Some(v) = call_site_map.get(*key) {
            if *key == "interpolation" {
                // Handle nested interpolation merge
                if let Some(cs_map) = v.as_mapping() {
                    // Merge into existing interpolation entry-by-entry.
                    let mut existing = merged_map
                        .get("interpolation")
                        .and_then(|ev| ev.as_mapping())
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<serde_yaml::Mapping>()
                        })
                        .unwrap_or_default();
                    for (sub_k, sub_v) in cs_map {
                        if let Some(cs_sub_map) = sub_v.as_mapping() {
                            // Merge call-site entries into existing sub-map
                            let mut merged_sub = existing
                                .get(sub_k)
                                .and_then(|v| v.as_mapping())
                                .map(|m| {
                                    m.iter()
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect::<serde_yaml::Mapping>()
                                })
                                .unwrap_or_default();
                            for (entry_k, entry_v) in cs_sub_map {
                                merged_sub.insert(entry_k.clone(), entry_v.clone());
                            }
                            existing.insert(sub_k.clone(), serde_yaml::Value::Mapping(merged_sub));
                        } else {
                            existing.insert(sub_k.clone(), sub_v.clone());
                        }
                    }
                    merged_map.insert(
                        serde_yaml::Value::String("interpolation".to_string()),
                        serde_yaml::Value::Mapping(existing),
                    );
                } else {
                    // Non-mapping value — insert as-is so schema validation can reject it
                    merged_map.insert(
                        serde_yaml::Value::String("interpolation".to_string()),
                        v.clone(),
                    );
                }
            } else {
                merged_map.insert(serde_yaml::Value::String(key.to_string()), v.clone());
            }
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
    fn test_serde_yaml_preserves_loop_iter_in_quoted_string() {
        // Regression: serde_yaml 0.9 (YAML 1.2) must preserve {loop_iter}
        // inside double-quoted strings.
        let yaml_str = r#"
stages:
  - name: verify
    type: sequence
    skip_if_exists: "artifact://{loop_iter}/done"
    body:
      - name: fix
        type: sequence
        skip_if_exists: "artifact://{loop_iter}/done"
        body:
          - name: fix
            type: agent
"#;
        let parsed: serde_yaml::Value = serde_yaml::from_str(yaml_str).unwrap();
        let stages = parsed["stages"].as_sequence().unwrap();
        let seq_stage = &stages[0];
        let sif = seq_stage["skip_if_exists"].as_str().unwrap();
        assert_eq!(
            sif, "artifact://{loop_iter}/done",
            "skip_if_exists should preserve {{loop_iter}}"
        );
        let body = seq_stage["body"].as_sequence().unwrap();
        let fix_wrapper = &body[0];
        let skip = fix_wrapper["skip_if_exists"].as_str().unwrap();
        assert_eq!(
            skip, "artifact://{loop_iter}/done",
            "skip_if_exists on sequence wrapper should preserve {{loop_iter}}"
        );
    }

    #[test]
    fn test_verify_recipe_skip_if_exists_preserves_loop_iter() {
        let recipe = load_yaml_file(std::path::Path::new(&format!(
            "{}/../../.gremlins/stages/verify.yaml",
            env!("CARGO_MANIFEST_DIR")
        )))
        .unwrap();
        let stages = recipe["stages"].as_sequence().unwrap();
        let seq_stage = &stages[0];
        let body = seq_stage["body"].as_sequence().unwrap();
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
        let definition = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
type: agent
prompt:
  - |
    use {foo} and {baz}
interpolation:
  inputs:
    foo: content("artifact://foo.md")
"#,
        )
        .unwrap();

        // Call site: new nested form
        let call_site = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
name: test
interpolation:
  inputs:
    baz: content("artifact://baz.md")
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
            .and_then(|v| v.as_mapping())
            .expect("interpolation must be a mapping");

        // Must be the nested form
        assert!(
            interp.contains_key("inputs"),
            "merged interpolation must have 'inputs' key"
        );

        let inputs = interp
            .get("inputs")
            .and_then(|v| v.as_mapping())
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
}
