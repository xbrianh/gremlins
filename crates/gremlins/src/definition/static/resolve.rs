use std::path::{Path, PathBuf};

use crate::core::discovery;
use crate::definition::r#static::expand::DefinitionResolver;
use crate::schemas::error::SchemaError;

/// Resolves gremlin definition names to file paths by searching project overlay
/// directories. Since bundled gremlin definitions were removed, only project overlays
/// (`.gremlins/`) are searched.
pub(crate) struct BuiltinResolver;

impl DefinitionResolver for BuiltinResolver {
    fn resolve(&self, name: &str, project_root: &Path) -> Result<PathBuf, SchemaError> {
        discovery::resolve_definition_name(name, project_root.to_path_buf()).map_err(|e| {
            SchemaError::DefinitionNotFound {
                name: name.to_string(),
                available: e.to_string(),
            }
        })
    }
}
