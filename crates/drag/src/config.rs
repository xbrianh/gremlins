// Drag config model — deserialized from JSON5.
//
// The bundled config (compiled in via include_str!) captures the current
// hard-coded defaults.  When --config or GREMLINS_DRAG_CLI_CONFIG is used
// the bundled config is NOT merged — omitted fields get serde schema
// defaults instead.
//
// The --model CLI flag always wins: it replaces the model field from
// whichever source was resolved.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bundled config compiled into the binary.
const BUNDLED_CONFIG: &str = include_str!("../config/bundled.json5");

// ---------------------------------------------------------------------------
// Root config
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct DragConfig {
    pub weights: WeightsConfig,
    pub model: Option<String>,
    pub tokenizer: String,
    pub estimates_per_survey: usize,
    pub skip_maintainability: bool,
    pub verbose: bool,
    pub format: OutputFormat,
    pub cache: Option<PathBuf>,
    pub comprehensibility: ComprehensibilityConfig,
    pub maintainability: MaintainabilityConfig,
}

impl Default for DragConfig {
    fn default() -> Self {
        Self {
            weights: WeightsConfig::default(),
            model: None,
            tokenizer: "o200k_base".to_string(),
            estimates_per_survey: 1,
            skip_maintainability: false,
            verbose: false,
            format: OutputFormat::Human,
            cache: None,
            comprehensibility: ComprehensibilityConfig::default(),
            maintainability: MaintainabilityConfig::default(),
        }
    }
}

impl DragConfig {
    /// Resolve the config via the precedence chain:
    ///   bundled → --config path | GREMLINS_DRAG_CLI_CONFIG env var → --model override
    ///
    /// `--config` and the env var are mutually exclusive.
    pub fn load(
        config_path: Option<PathBuf>,
        model_override: Option<String>,
    ) -> Result<Self, String> {
        let env_val = std::env::var("GREMLINS_DRAG_CLI_CONFIG")
            .ok()
            .filter(|v| !v.is_empty());

        let mut config: DragConfig = match (&config_path, env_val) {
            (Some(_), Some(_)) => {
                return Err(
                    "--config and GREMLINS_DRAG_CLI_CONFIG are mutually exclusive. \
                     Use one or the other."
                        .to_string(),
                );
            }
            (Some(path), None) => {
                let content = std::fs::read_to_string(path)
                    .map_err(|e| format!("failed to read config file {}: {e}", path.display()))?;
                serde_json5::from_str(&content)
                    .map_err(|e| format!("invalid config in {}: {e}", path.display()))?
            }
            (None, Some(env_json)) => serde_json5::from_str(&env_json)
                .map_err(|e| format!("GREMLINS_DRAG_CLI_CONFIG: invalid JSON5: {e}"))?,
            (None, None) => serde_json5::from_str(BUNDLED_CONFIG)
                .map_err(|e| format!("internal error: bundled config is invalid: {e}"))?,
        };

        // --model always wins (single-field surgical override).
        if let Some(model) = model_override {
            config.model = Some(model);
        }

        Ok(config)
    }

    /// Compute a hash of the comprehensibility survey definition for cache-key
    /// poisoning.  Any change to dimensions, descriptions, or anchors produces
    /// a different hash, preventing cross-config cache hits.
    pub fn comprehensibility_hash(&self) -> String {
        let json = serde_json::to_string(&self.comprehensibility).unwrap_or_default();
        blake3::hash(json.as_bytes()).to_hex()[..16].to_string()
    }

    /// Compute a hash of the maintainability survey definition for cache-key
    /// poisoning.
    pub fn maintainability_hash(&self) -> String {
        let json = serde_json::to_string(&self.maintainability).unwrap_or_default();
        blake3::hash(json.as_bytes()).to_hex()[..16].to_string()
    }
}

// ---------------------------------------------------------------------------
// Weights
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct WeightsConfig {
    #[serde(default = "default_w2")]
    pub w2: f64,
    #[serde(default = "default_m")]
    pub m: f64,
    #[serde(default = "default_p")]
    pub p: f64,
}

fn default_w2() -> f64 {
    0.5
}
fn default_m() -> f64 {
    2.5
}
fn default_p() -> f64 {
    2.5
}

impl Default for WeightsConfig {
    fn default() -> Self {
        Self {
            w2: default_w2(),
            m: default_m(),
            p: default_p(),
        }
    }
}

// ---------------------------------------------------------------------------
// Output format
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Human,
    Json,
}

// ---------------------------------------------------------------------------
// Comprehensibility config
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ComprehensibilityConfig {
    /// Preamble text inserted before the dimension list.
    #[serde(default)]
    pub preamble: String,
    /// Generic 0–4 scale anchors applied to all dimensions.
    #[serde(default)]
    pub generic_anchors: String,
    /// Ordered list of dimensions to score.
    #[serde(default)]
    pub dimensions: Vec<ComprehensibilityDimensionDef>,
    /// Per-dimension structural anchors, keyed by dimension key.
    #[serde(default)]
    pub structural_anchors: HashMap<String, String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ComprehensibilityDimensionDef {
    /// Machine-readable key (also used as the JSON field name in LLM output).
    pub key: String,
    /// Human-readable label.
    #[serde(default)]
    pub label: String,
    /// One-line description of what this dimension measures.
    #[serde(default)]
    pub description: String,
}

// ---------------------------------------------------------------------------
// Maintainability config
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct MaintainabilityConfig {
    /// Preamble text inserted before the dimension-specific prompt.
    #[serde(default)]
    pub preamble: String,
    /// Ordered list of dimensions to score.
    #[serde(default)]
    pub dimensions: Vec<MaintainabilityDimensionDef>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct MaintainabilityDimensionDef {
    /// Machine-readable key (used as the dimension identifier).
    pub key: String,
    /// Human-readable label for logging.
    #[serde(default)]
    pub label: String,
    /// One-paragraph description of what the dimension measures.
    #[serde(default)]
    pub description: String,
    /// Concrete 0–4 anchors tailored to this dimension.
    #[serde(default)]
    pub anchors: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_config_parses() {
        let config: DragConfig =
            serde_json5::from_str(BUNDLED_CONFIG).expect("bundled config must parse");
        assert_eq!(config.weights.w2, 0.5);
        assert_eq!(config.weights.m, 2.5);
        assert_eq!(config.weights.p, 2.5);
        assert_eq!(config.tokenizer, "o200k_base");
        assert_eq!(config.estimates_per_survey, 1);
        assert!(!config.skip_maintainability);
        assert!(config.model.is_none());
        assert_eq!(config.comprehensibility.dimensions.len(), 7);
        assert_eq!(config.maintainability.dimensions.len(), 10);
        // Structural anchors cover the three structural dimensions.
        assert!(config
            .comprehensibility
            .structural_anchors
            .contains_key("cohesion"));
        assert!(config
            .comprehensibility
            .structural_anchors
            .contains_key("sizing"));
        assert!(config
            .comprehensibility
            .structural_anchors
            .contains_key("test_hygiene"));
    }

    #[test]
    fn default_config_has_schema_defaults() {
        let config = DragConfig::default();
        assert_eq!(config.weights.w2, 0.5);
        assert_eq!(config.weights.m, 2.5);
        assert_eq!(config.tokenizer, "o200k_base");
        assert!(config.model.is_none());
        assert!(config.comprehensibility.dimensions.is_empty());
        assert!(config.maintainability.dimensions.is_empty());
        assert!(config.comprehensibility.preamble.is_empty());
        assert!(config.maintainability.preamble.is_empty());
    }

    #[test]
    fn partial_json_gets_schema_defaults_not_bundled() {
        // When a user provides a minimal JSON, omitted fields get schema defaults.
        let json = r#"{"weights": {"w2": 1.0}}"#;
        let config: DragConfig = serde_json5::from_str(json).expect("partial config should parse");
        assert_eq!(config.weights.w2, 1.0);
        // m and p fall back to schema defaults, not bundled values.
        assert_eq!(config.weights.m, 2.5);
        assert_eq!(config.weights.p, 2.5);
        // Omitted top-level fields get schema defaults.
        assert_eq!(config.tokenizer, "o200k_base");
        assert!(config.model.is_none());
        assert!(config.comprehensibility.dimensions.is_empty());
        assert!(config.maintainability.dimensions.is_empty());
    }

    #[test]
    fn comprehensibility_hash_changes_with_dimensions() {
        let mut cfg1 = DragConfig::default();
        cfg1.comprehensibility
            .dimensions
            .push(ComprehensibilityDimensionDef {
                key: "foo".into(),
                label: "Foo".into(),
                description: "desc".into(),
            });
        let h1 = cfg1.comprehensibility_hash();

        let mut cfg2 = DragConfig::default();
        cfg2.comprehensibility
            .dimensions
            .push(ComprehensibilityDimensionDef {
                key: "bar".into(),
                label: "Bar".into(),
                description: "desc".into(),
            });
        let h2 = cfg2.comprehensibility_hash();

        assert_ne!(h1, h2);
    }

    #[test]
    fn comprehensibility_hash_stable() {
        let mut cfg = DragConfig::default();
        cfg.comprehensibility
            .dimensions
            .push(ComprehensibilityDimensionDef {
                key: "test".into(),
                label: "Test".into(),
                description: "desc".into(),
            });
        let h1 = cfg.comprehensibility_hash();
        let h2 = cfg.comprehensibility_hash();
        assert_eq!(h1, h2);
    }
}
