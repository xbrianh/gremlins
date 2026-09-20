use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DiscoveryError {
    #[error("gremlin definition {name:?} not found; available: {available}")]
    Name { name: String, available: String },

    #[error("gremlin definition file not found: {path}")]
    File { path: PathBuf },

    #[error("gremlin definition {name:?} not found in {dirs}")]
    Path { name: String, dirs: String },
}
