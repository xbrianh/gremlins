use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;

use crate::schemas::error::SchemaError;
use crate::schemas::prompts;

pub const GREMLINS_PREFIX: &str = "gremlins:";

/// Trait for resolving pipeline names to file paths.
/// The pyext layer provides a Python-callback implementation.
pub trait PipelineResolver {
    fn resolve(&self, name: &str, project_root: &std::path::Path) -> Result<PathBuf, SchemaError>;
}

pub fn load_yaml_file(path: &PathBuf) -> Result<serde_yaml::Value, SchemaError> {
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

pub fn load_bundled_recipe(
    raw_name: &str,
    bundled_stage_def_dir: &PathBuf,
) -> Result<serde_yaml::Value, SchemaError> {
    let name = raw_name.replace('-', "_");
    let recipe_path = bundled_stage_def_dir.join(format!("{}.yaml", name));
    let bundled_dir = bundled_stage_def_dir
        .canonicalize()
        .unwrap_or_else(|_| bundled_stage_def_dir.clone());

    // Reject path traversal even when the file doesn't exist
    if recipe_path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(SchemaError::Generic(format!(
            "invalid bundled recipe name: {raw_name:?}"
        )));
    }

    let recipe_path = recipe_path.canonicalize().unwrap_or(recipe_path);
    if !recipe_path.starts_with(&bundled_dir) {
        return Err(SchemaError::Generic(format!(
            "invalid bundled recipe name: {raw_name:?}"
        )));
    }
    if !recipe_path.exists() {
        let mut available = Vec::new();
        if let Ok(entries) = std::fs::read_dir(bundled_stage_def_dir) {
            for entry in entries.flatten() {
                if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                    available.push(stem.to_string());
                }
            }
        }
        available.sort();
        return Err(SchemaError::BundledRecipeNotFound {
            name: format!("{GREMLINS_PREFIX}{raw_name}"),
            available: available.join(", "),
        });
    }
    load_yaml_file(&recipe_path)
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

pub fn parse_stage_definitions(
    raw: Option<&serde_yaml::Value>,
    bundled_stage_def_dir: &PathBuf,
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
                        match load_bundled_recipe(recipe_name, bundled_stage_def_dir) {
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
                        return Err(SchemaError::StageDef {
                            name: name.clone(),
                            msg: "must be a dict or gremlins: reference".to_string(),
                        });
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

#[allow(clippy::too_many_arguments)]
pub fn expand_pipeline(
    yaml_path: &PathBuf,
    project_root: Option<&PathBuf>,
    bundled_stage_def_dir: &PathBuf,
    bundled_prompt_dir: &PathBuf,
    resolver: &dyn PipelineResolver,
) -> Result<serde_yaml::Value, SchemaError> {
    let project_root = project_root.cloned().unwrap_or_else(|| {
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
    _expand(
        yaml_path,
        &project_root,
        &chain,
        bundled_stage_def_dir,
        bundled_prompt_dir,
        resolver,
    )
}

#[allow(clippy::too_many_arguments)]
fn _expand(
    yaml_path: &PathBuf,
    project_root: &PathBuf,
    chain: &[PathBuf],
    bundled_stage_def_dir: &PathBuf,
    bundled_prompt_dir: &PathBuf,
    resolver: &dyn PipelineResolver,
) -> Result<serde_yaml::Value, SchemaError> {
    let resolved = yaml_path
        .canonicalize()
        .unwrap_or_else(|_| yaml_path.clone());
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

    let named_prompts =
        prompts::parse_named_prompts(raw_mapping.get("prompts"), &prompt_dir, bundled_prompt_dir)?;

    let stage_defs =
        parse_stage_definitions(raw_mapping.get("stage-definitions"), bundled_stage_def_dir)?;

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
            bundled_stage_def_dir,
            bundled_prompt_dir,
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
    bundled_stage_def_dir: &PathBuf,
    bundled_prompt_dir: &PathBuf,
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
        let included = _expand(
            &included_path,
            project_root,
            chain,
            bundled_stage_def_dir,
            bundled_prompt_dir,
            resolver,
        )?;
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
                bundled_stage_def_dir,
                bundled_prompt_dir,
                resolver,
            );
        }
        if let Some(recipe_name) = stage_type.strip_prefix(GREMLINS_PREFIX) {
            if recipe_name.is_empty() {
                return Err(SchemaError::Generic(format!(
                    "missing name after {GREMLINS_PREFIX:?}"
                )));
            }
            let recipe_def = load_bundled_recipe(recipe_name, bundled_stage_def_dir)?;
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
                bundled_stage_def_dir,
                bundled_prompt_dir,
                resolver,
            );
        }
        // Auto-resolve bundled stage-definitions by type name
        let recipe_path =
            bundled_stage_def_dir.join(format!("{}.yaml", stage_type.replace('-', "_")));
        if recipe_path.exists() {
            let auto_def = load_yaml_file(&recipe_path)?;
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
                bundled_stage_def_dir,
                bundled_prompt_dir,
                resolver,
            );
        }
        // Try resolving as pipeline name
        let pipeline_result = resolver.resolve(stage_type, project_root);
        match pipeline_result {
            Ok(included_path) => {
                if !chain.contains(&included_path) {
                    let included = _expand(
                        &included_path,
                        project_root,
                        chain,
                        bundled_stage_def_dir,
                        bundled_prompt_dir,
                        resolver,
                    )?;
                    let stages = match included.get("stages") {
                        Some(serde_yaml::Value::Sequence(s)) => s.clone(),
                        _ => Vec::new(),
                    };
                    return Ok(stages);
                }
            }
            Err(SchemaError::PipelineNotFound(_)) => {
                // Not a pipeline — fall through to loader validation
            }
            Err(e) => return Err(e),
        }
    }

    let mut entry = entry.clone();
    let entry_map = entry.as_mapping_mut().unwrap();

    if entry_map.contains_key("prompt") {
        let prompt_val = entry_map.get("prompt").unwrap().clone();
        let texts =
            prompts::read_prompts(&prompt_val, prompt_dir, named_prompts, bundled_prompt_dir)?;
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
                    bundled_stage_def_dir,
                    bundled_prompt_dir,
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
                    bundled_stage_def_dir,
                    bundled_prompt_dir,
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
    bundled_stage_def_dir: &PathBuf,
    bundled_prompt_dir: &PathBuf,
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
                bundled_prompt_dir,
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
                bundled_stage_def_dir,
                bundled_prompt_dir,
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
        bundled_stage_def_dir,
        bundled_prompt_dir,
        resolver,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
