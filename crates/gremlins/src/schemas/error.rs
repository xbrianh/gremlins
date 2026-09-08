use thiserror::Error;

#[derive(Error, Debug)]
pub enum SchemaError {
    #[error("{0}")]
    Generic(String),

    #[error("include cycle detected: {0}")]
    IncludeCycle(String),

    #[error("bundled recipe not found: {name}; available: {available}")]
    BundledRecipeNotFound { name: String, available: String },

    #[error("prompt file not found: {path}")]
    PromptFileNotFound { path: String },

    #[error("pipeline {name:?} not found; available: {available}")]
    PipelineNotFound { name: String, available: String },

    #[error("prompt file is empty: {path}")]
    PromptFileEmpty { path: String },

    #[error("stage-definition {name}: {msg}")]
    StageDef { name: String, msg: String },

    #[error("stage {name}: {msg}")]
    Stage { name: String, msg: String },

    #[error("input source {name}: {msg}")]
    InputSource { name: String, msg: String },

    #[error(
        "stage {stage}: key {key:?} declared in {map:?} is not referenced in any prompt or command"
    )]
    UnusedStageKey {
        stage: String,
        key: String,
        map: String,
    },

    #[error("stage {stage}: key {key:?} declared in both bind: and interpolation: — a stage cannot both produce and consume the same key")]
    DuplicateStageKey { stage: String, key: String },

    #[error("stage {stage}: artifact {uri:?} is consumed via interpolation but never produced by any prior stage's bind, bootstrap bind_artifact, cli_out, or implicit artifact (base_sha, base_ref)")]
    UnresolvedArtifactConsumer { stage: String, uri: String },

    #[error("pipeline is missing 'default_client' — every pipeline must declare one")]
    MissingDefaultClient,

    #[error("pipeline file not found: {path}")]
    PipelineFileNotFound { path: String },

    #[error("YAML parse error in {label}: {msg}")]
    YamlParse { label: String, msg: String },

    #[error("expected a YAML mapping in {label}, got {got}")]
    YamlNotMapping { label: String, got: String },
}

impl From<std::io::Error> for SchemaError {
    fn from(e: std::io::Error) -> Self {
        SchemaError::Generic(e.to_string())
    }
}

impl From<serde_yaml::Error> for SchemaError {
    fn from(e: serde_yaml::Error) -> Self {
        SchemaError::YamlParse {
            label: "yaml".to_string(),
            msg: e.to_string(),
        }
    }
}
