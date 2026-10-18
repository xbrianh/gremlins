//! OpenAI backend — an OpenAI-protocol backend declared with
//! [`crate::declare_openai_backend`].

const PROVIDER_NAME: &str = "openai";
const API_KEY_ENV: &str = "OPENAI_API_KEY";
const BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o";

super::declare_openai_backend! {
    struct OpenAiBackend,
    provider: PROVIDER_NAME,
    api_key_env: API_KEY_ENV,
    base_url_env: "OPENAI_BASE_URL",
    base_url: BASE_URL,
    default_model: DEFAULT_MODEL,
    dialect: rig_core::providers::openai::wire::OPENAI,
    classifier: None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "openai");
        assert_eq!(API_KEY_ENV, "OPENAI_API_KEY");
        assert_eq!(BASE_URL, "https://api.openai.com/v1");
        assert_eq!(DEFAULT_MODEL, "gpt-4o");
    }
}
