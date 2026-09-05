use pyo3::exceptions::{PyFileNotFoundError, PyValueError};

pub use gremlins::schemas::error::SchemaError;

pub fn into_pyerr(e: SchemaError) -> pyo3::PyErr {
    match &e {
        SchemaError::PipelineFileNotFound { .. }
        | SchemaError::PromptFileNotFound { .. }
        | SchemaError::PipelineNotFound(_)
        | SchemaError::BundledRecipeNotFound { .. } => PyFileNotFoundError::new_err(e.to_string()),
        SchemaError::IncludeCycle(_)
        | SchemaError::PromptFileEmpty { .. }
        | SchemaError::StageDef { .. }
        | SchemaError::Stage { .. }
        | SchemaError::InputSource { .. }
        | SchemaError::MissingDefaultClient
        | SchemaError::YamlParse { .. }
        | SchemaError::YamlNotMapping { .. } => PyValueError::new_err(e.to_string()),
        SchemaError::Generic(_) => PyValueError::new_err(e.to_string()),
    }
}
