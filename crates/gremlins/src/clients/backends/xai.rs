//! xAI backend — an OpenAI-protocol backend declared with
//! [`crate::declare_openai_backend`].

const PROVIDER_NAME: &str = "xai";
const API_KEY_ENV: &str = "XAI_API_KEY";
const BASE_URL: &str = "https://api.x.ai";
const DEFAULT_MODEL: &str = "grok-4";

super::declare_openai_backend! {
    struct XaiBackend,
    provider: PROVIDER_NAME,
    api_key_env: API_KEY_ENV,
    base_url_env: "XAI_BASE_URL",
    base_url: BASE_URL,
    default_model: DEFAULT_MODEL,
    dialect: rig_core::providers::xai::DIALECT,
    classifier: None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "xai");
        assert_eq!(API_KEY_ENV, "XAI_API_KEY");
        assert_eq!(BASE_URL, "https://api.x.ai");
        assert_eq!(DEFAULT_MODEL, "grok-4");
    }
}
