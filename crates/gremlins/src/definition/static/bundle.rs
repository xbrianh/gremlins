use std::path::Path;

use crate::config;
use crate::definition::r#static::expand;
use crate::schemas::error::SchemaError;

/// Bundle a modular definition into a self-contained monolith YAML.
///
/// Expands all structural references (stage-definitions, prompt files,
/// includes) into inline values while preserving model configuration
/// and bootstrap parameters verbatim so the output remains
/// environment-tunable.
///
/// The returned [`serde_yaml::Value`] is suitable for serialization as
/// a standalone YAML file that `gremlins launch` can consume directly.
pub fn bundle_definition(yaml_path: &Path) -> Result<serde_yaml::Value, SchemaError> {
    let project_root = project_root_for(yaml_path);
    let overlay_dir = config::overlay_dir_without_env(&project_root);
    let expanded = expand::parse_definition_file(yaml_path, &project_root, &overlay_dir)?;

    let mut mapping = match expanded {
        serde_yaml::Value::Mapping(m) => m,
        other => {
            return Err(SchemaError::Generic(format!(
                "expected YAML mapping after expansion, got {:?}",
                other
            )));
        }
    };

    // Strip structural keys that serve no purpose once references are inlined.
    mapping.remove("__gremlins_expanded__");
    mapping.remove("prompt_dir");
    mapping.remove("prompts");
    mapping.remove("stage-definitions");

    Ok(serde_yaml::Value::Mapping(mapping))
}

/// Walk up from `path` looking for a `.gremlins` overlay directory.
/// When found, the directory *containing* `.gremlins` is the project root.
/// Falls back to the YAML file's parent directory.
fn project_root_for(path: &Path) -> std::path::PathBuf {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory
            .file_name()
            .is_some_and(|name| name == config::overlay_dirname())
        {
            if let Some(parent) = directory.parent() {
                return parent.to_path_buf();
            }
            break;
        }
        current = directory.parent();
    }
    path.parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}
