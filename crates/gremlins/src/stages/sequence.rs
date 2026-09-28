use serde_json::Value;

use crate::stages::composite::{ClientSpec, StageAttrs};

#[derive(Debug, Clone)]
pub struct Sequence {
    pub attrs: StageAttrs,
    pub body: Vec<Value>,
    pub client: Option<ClientSpec>,
    /// Maximum number of iterations (default 1).
    pub max_iterations: u32,
    /// Seconds between iterations.
    pub interval: Option<f64>,
}
