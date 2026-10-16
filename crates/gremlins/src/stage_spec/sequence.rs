use serde_json::Value;

use crate::clients::client::ModelURI;
use crate::stage_spec::composite::StageAttrs;

#[derive(Debug, Clone)]
pub struct Sequence {
    pub attrs: StageAttrs,
    pub body: Vec<Value>,
    pub model: Option<ModelURI>,
    /// Maximum number of iterations (default 1).
    pub max_iterations: u32,
    /// Seconds between iterations.
    pub interval: Option<f64>,
}
