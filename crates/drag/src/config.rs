// Drag config model — deserialized from JSON5.
//
// The bundled config (compiled in via include_str!) captures the current
// hard-coded defaults.  When --config or GREMLINS_DRAG_CLI_CONFIG is used
// the external config is merged on top of the bundled config: keys present
// in the external config replace bundled values (including nested objects
// wholesale), while omitted keys keep their bundled defaults.
//
// The --model CLI flag always wins: it replaces the model field from
// whichever source was resolved.

use std::collections::BTreeMap;
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
    /// Target path (file or directory) to score.
    /// When GREMLINS_DRAG_CLI_CONFIG is set, this field provides the
    /// target — no positional CLI argument is required.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub weights: WeightsConfig,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_tokenizer")]
    pub tokenizer: String,
    #[serde(default = "default_estimates")]
    pub estimates_per_survey: usize,
    #[serde(default)]
    pub skip_maintainability: bool,
    #[serde(default)]
    pub verbose: bool,
    #[serde(default)]
    pub format: OutputFormat,
    #[serde(default)]
    pub cache: Option<PathBuf>,
    #[serde(default)]
    pub comprehensibility: ComprehensibilityConfig,
    #[serde(default)]
    pub maintainability: MaintainabilityConfig,
}

fn default_tokenizer() -> String {
    String::new()
}
fn default_estimates() -> usize {
    0
}

impl Default for DragConfig {
    fn default() -> Self {
        Self {
            path: None,
            weights: WeightsConfig::default(),
            model: None,
            tokenizer: String::new(),
            estimates_per_survey: 0,
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

        // Load the bundled config as a JSON Value to serve as the base.
        let mut merged: serde_json::Value = serde_json5::from_str(BUNDLED_CONFIG)
            .map_err(|e| format!("internal error: bundled config is invalid: {e}"))?;

        // If an external config is present, parse it and merge its keys on top
        // of the bundled Value.  Shallow key-level merge: when a key appears in
        // both, the external value wins (including nested objects wholesale).
        match (&config_path, &env_val) {
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
                let external: serde_json::Value = serde_json5::from_str(&content)
                    .map_err(|e| format!("invalid config in {}: {e}", path.display()))?;
                merge_json_values(&mut merged, &external);
            }
            (None, Some(env_path)) => {
                let content = std::fs::read_to_string(env_path)
                    .map_err(|e| format!("GREMLINS_DRAG_CLI_CONFIG: {}: {e}", env_path))?;
                let external: serde_json::Value = serde_json5::from_str(&content)
                    .map_err(|e| format!("GREMLINS_DRAG_CLI_CONFIG: {}: {e}", env_path))?;
                merge_json_values(&mut merged, &external);
            }
            (None, None) => {
                // No external config; use bundled as-is.
            }
        }

        // Deserialize the merged Value into DragConfig.
        let mut config: DragConfig = serde_json::from_value(merged)
            .map_err(|e| format!("failed to deserialize merged config: {e}"))?;

        // --model always wins (single-field surgical override).
        if let Some(model) = model_override {
            config.model = Some(model);
        }

        // Reject duplicate dimension keys — scoring stores results in maps
        // keyed by these strings, so duplicates would silently mask failures.
        {
            let mut seen = std::collections::HashSet::new();
            for dim in &config.comprehensibility.dimensions {
                if !seen.insert(&dim.key) {
                    return Err(format!(
                        "comprehensibility config: duplicate dimension key {:?}",
                        dim.key
                    ));
                }
            }
        }
        {
            let mut seen = std::collections::HashSet::new();
            for dim in &config.maintainability.dimensions {
                if !seen.insert(&dim.key) {
                    return Err(format!(
                        "maintainability config: duplicate dimension key {:?}",
                        dim.key
                    ));
                }
            }
        }

        Ok(config)
    }

    /// Compute a hash of the comprehensibility survey definition for cache-key
    /// poisoning.  Any change to dimensions, descriptions, or anchors produces
    /// a different hash, preventing cross-config cache hits.
    pub fn comprehensibility_hash(&self) -> String {
        // Serialize with sorted keys for deterministic output across
        // processes (structural_anchors is a BTreeMap, but other maps
        // may be added in the future).
        let json = serde_json::to_string(&self.comprehensibility)
            .expect("comprehensibility config must serialize");
        blake3::hash(json.as_bytes()).to_hex()[..16].to_string()
    }

    /// Compute a hash of the maintainability survey definition for cache-key
    /// poisoning.
    pub fn maintainability_hash(&self) -> String {
        let json = serde_json::to_string(&self.maintainability)
            .expect("maintainability config must serialize");
        blake3::hash(json.as_bytes()).to_hex()[..16].to_string()
    }
}

/// Shallow key-level merge of JSON objects.
///
/// When `external` contains a key present in `base`, the external value
/// replaces the base value wholesale (including nested objects).  Keys
/// only in `external` are inserted into `base`.
fn merge_json_values(base: &mut serde_json::Value, external: &serde_json::Value) {
    if let (serde_json::Value::Object(base_map), serde_json::Value::Object(ext_map)) =
        (base, external)
    {
        for (k, v) in ext_map {
            base_map.insert(k.clone(), v.clone());
        }
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
            w2: 0.5,
            m: 2.5,
            p: 2.5,
        }
    }
}

// ---------------------------------------------------------------------------
// Output format
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
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
    /// BTreeMap provides deterministic serialization for cache-key hashing.
    #[serde(default)]
    pub structural_anchors: BTreeMap<String, String>,
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
        assert_eq!(config.estimates_per_survey, 5);
        assert!(!config.skip_maintainability);
        assert!(config.model.is_none());
        assert_eq!(config.comprehensibility.dimensions.len(), 8);
        assert_eq!(config.maintainability.dimensions.len(), 10);
        // Structural anchors cover the four structural dimensions.
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
        assert!(config
            .comprehensibility
            .structural_anchors
            .contains_key("duplication"));
    }

    #[test]
    fn default_config_has_schema_defaults() {
        let config = DragConfig::default();
        // Weights default to 0.5, 2.5, 2.5 (schema defaults match bundled config).
        assert_eq!(config.weights.w2, 0.5);
        assert_eq!(config.weights.m, 2.5);
        assert_eq!(config.weights.p, 2.5);
        // Operational fields default to empty / zero.
        assert_eq!(config.tokenizer, "");
        assert_eq!(config.estimates_per_survey, 0);
        assert!(config.model.is_none());
        assert!(config.comprehensibility.dimensions.is_empty());
        assert!(config.maintainability.dimensions.is_empty());
        assert!(config.comprehensibility.preamble.is_empty());
        assert!(config.maintainability.preamble.is_empty());
    }

    #[test]
    fn partial_json_gets_bundled_defaults() {
        // When a user provides a minimal JSON, omitted fields get schema defaults.
        // Schema defaults now match bundled values (0.5, 2.5, 2.5) so partial
        // overrides don't silently zero the weights.
        let json = r#"{"weights": {"w2": 1.0}}"#;
        let config: DragConfig = serde_json5::from_str(json).expect("partial config should parse");
        assert_eq!(config.weights.w2, 1.0);
        // m and p fall back to schema defaults (2.5), matching bundled values.
        assert_eq!(config.weights.m, 2.5);
        assert_eq!(config.weights.p, 2.5);
        // Omitted top-level fields get schema defaults.
        assert_eq!(config.tokenizer, "");
        assert_eq!(config.estimates_per_survey, 0);
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

    #[test]
    fn maintainability_hash_stable() {
        let mut cfg = DragConfig::default();
        cfg.maintainability
            .dimensions
            .push(MaintainabilityDimensionDef {
                key: "test".into(),
                label: "Test".into(),
                description: "desc".into(),
                anchors: "0: bad\n4: good".into(),
            });
        let h1 = cfg.maintainability_hash();
        let h2 = cfg.maintainability_hash();
        assert_eq!(h1, h2);
    }

    // ── DragConfig::load tests ──
    //
    // These test the individual components rather than calling load()
    // directly because load() reads GREMLINS_DRAG_CLI_CONFIG from the
    // process environment, which is not thread-safe for parallel tests.

    #[test]
    fn load_bundled_parses_with_expected_values() {
        let config: DragConfig =
            serde_json5::from_str(BUNDLED_CONFIG).expect("bundled config must parse");
        assert_eq!(config.comprehensibility.dimensions.len(), 8);
        assert_eq!(config.maintainability.dimensions.len(), 10);
        assert_eq!(config.tokenizer, "o200k_base");
        assert_eq!(config.weights.w2, 0.5);
    }

    #[test]
    fn model_override_replaces_model_field() {
        // Simulate what load() does: parse the bundled config, then
        // replace model if --model is given.
        let mut config: DragConfig =
            serde_json5::from_str(BUNDLED_CONFIG).expect("bundled config must parse");
        assert!(config.model.is_none());
        config.model = Some("openai/gpt-4o".to_string());
        assert_eq!(config.model.as_deref(), Some("openai/gpt-4o"));
        // Other fields unchanged.
        assert_eq!(config.comprehensibility.dimensions.len(), 8);
    }

    #[test]
    fn file_config_overrides_bundled_fields() {
        // Direct deserialization without merge: when a partial JSON is
        // deserialized directly (not via load()), omitted fields get schema
        // defaults.  The merge step in load() fills those in from bundled.json5
        // before deserialization.
        let json = r#"{"tokenizer": "cl100k_base", "verbose": true}"#;
        let config: DragConfig = serde_json5::from_str(json).expect("file config must parse");
        assert_eq!(config.tokenizer, "cl100k_base");
        assert!(config.verbose);
        // Without merge, omitted fields get schema defaults.
        assert!(config.comprehensibility.dimensions.is_empty());
    }

    #[test]
    fn merge_external_config_preserves_bundled_survey() {
        // Simulate load()'s merge step: start with bundled, merge an external
        // config that only sets tokenizer on top, then deserialize.
        let mut bundled: serde_json::Value =
            serde_json5::from_str(BUNDLED_CONFIG).expect("bundled must parse");
        let external: serde_json::Value =
            serde_json5::from_str(r#"{"tokenizer": "cl100k_base"}"#).expect("external must parse");
        merge_json_values(&mut bundled, &external);
        let config: DragConfig = serde_json::from_value(bundled).expect("merged config must parse");
        // External override wins.
        assert_eq!(config.tokenizer, "cl100k_base");
        // Bundled survey dimensions are preserved.
        assert_eq!(config.comprehensibility.dimensions.len(), 8);
        assert_eq!(config.maintainability.dimensions.len(), 10);
        // Bundled weights preserved.
        assert_eq!(config.weights.w2, 0.5);
    }

    #[test]
    fn model_override_on_file_config() {
        // --model overrides the model field from a file config.
        let json = r#"{"model": "anthropic/claude", "tokenizer": "cl100k_base"}"#;
        let mut config: DragConfig = serde_json5::from_str(json).expect("file config must parse");
        assert_eq!(config.model.as_deref(), Some("anthropic/claude"));
        // --model override
        config.model = Some("openai/gpt-4o".to_string());
        assert_eq!(config.model.as_deref(), Some("openai/gpt-4o"));
        assert_eq!(config.tokenizer, "cl100k_base");
    }

    #[test]
    fn partial_file_config_gets_schema_defaults_for_survey() {
        // Direct deserialization without merge: a config that sets only
        // weights gets empty surveys.  The load() merge step would fill
        // these in from bundled.json5.
        let json = r#"{"weights": {"w2": 1.0}}"#;
        let config: DragConfig = serde_json5::from_str(json).expect("partial config must parse");
        assert_eq!(config.weights.w2, 1.0);
        assert!(config.comprehensibility.dimensions.is_empty());
        assert!(config.maintainability.dimensions.is_empty());
    }

    #[test]
    fn load_rejects_duplicate_comprehensibility_keys() {
        let json = r#"{
            "comprehensibility": {
                "dimensions": [
                    {"key": "naming", "description": "naming"},
                    {"key": "naming", "description": "duplicate naming"}
                ]
            }
        }"#;
        let config: DragConfig = serde_json5::from_str(json).expect("parse must succeed");
        // Simulate the validation logic in load().
        let mut seen = std::collections::HashSet::new();
        let mut dup = None;
        for dim in &config.comprehensibility.dimensions {
            if !seen.insert(&dim.key) {
                dup = Some(dim.key.clone());
                break;
            }
        }
        assert_eq!(dup, Some("naming".to_string()));
    }

    #[test]
    fn load_rejects_duplicate_maintainability_keys() {
        let json = r#"{
            "maintainability": {
                "dimensions": [
                    {"key": "decomposition", "description": "desc"},
                    {"key": "decomposition", "description": "dup"}
                ]
            }
        }"#;
        let config: DragConfig = serde_json5::from_str(json).expect("parse must succeed");
        let mut seen = std::collections::HashSet::new();
        let mut dup = None;
        for dim in &config.maintainability.dimensions {
            if !seen.insert(&dim.key) {
                dup = Some(dim.key.clone());
                break;
            }
        }
        assert_eq!(dup, Some("decomposition".to_string()));
    }
}
