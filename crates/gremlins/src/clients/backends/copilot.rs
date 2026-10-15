use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::providers::copilot::{Copilot, CopilotConfig, CopilotIntent};
use rig_core::driver::DynModel;
use rig_core::operation::Completion;
use serde::Deserialize;
use std::time::Duration;

use crate::clients::agent_loop::{
    default_classify, run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext,
};
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::interactive::InteractiveSession;
use crate::clients::openai_protocol;
use crate::clients::params::{self, ResolvedParams};
use crate::clients::protocol::CompletedRun;

const PROVIDER_NAME: &str = "copilot";
const DEFAULT_MODEL: &str = "gpt-4o";

const GITHUB_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const GITHUB_COPILOT_TOKEN_TIMEOUT: Duration = Duration::from_secs(10);
const COPILOT_EDITOR_VERSION: &str = "vscode/1.107.0";
const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";

#[derive(Debug, Deserialize)]
struct CopilotTokenResponse {
    token: Option<String>,
    endpoints: Option<CopilotTokenEndpoints>,
}

#[derive(Debug, Deserialize)]
struct CopilotTokenEndpoints {
    api: Option<String>,
}

/// Resolve credentials and build a Copilot client.
/// OAuth device-code flow is disabled — gremlins run unattended.
///
/// Auth precedence: `GITHUB_COPILOT_API_KEY` → `COPILOT_API_KEY` →
/// `providers.yaml` `"copilot"` entry (`api-key`) → `GITHUB_TOKEN` →
/// `GH_TOKEN` → `gh auth token`.
fn resolve_auth() -> Result<Copilot, String> {
    if let Some(key) = crate::config::copilot_api_key() {
        return Ok(CopilotConfig::new(key).client());
    }
    if let Some(key) = crate::clients::config::api_key("", PROVIDER_NAME) {
        return Ok(CopilotConfig::new(key).client());
    }
    if let Some(access_token) = github_access_token() {
        return exchange_github_token(&access_token);
    }
    Err(format!(
        "no credentials for provider '{PROVIDER_NAME}': set GITHUB_COPILOT_API_KEY, \
         COPILOT_API_KEY, GITHUB_TOKEN, or GH_TOKEN; add an entry with \"api-key\" in {}; \
         or authenticate the gh CLI (`gh auth login` / `gh auth token`).",
        crate::config::user_config_root()
            .join("providers.yaml")
            .display(),
    ))
}

/// Read a nonblank GitHub access token from the environment.
fn github_access_token_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Read `gh auth token`, returning `None` when the CLI or login is absent.
fn gh_auth_token() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// Resolve a GitHub access token: `GITHUB_TOKEN` → `GH_TOKEN` → `gh auth token`.
fn github_access_token() -> Option<String> {
    github_access_token_env("GITHUB_TOKEN")
        .or_else(|| github_access_token_env("GH_TOKEN"))
        .or_else(gh_auth_token)
}

/// Exchange a GitHub access token for a Copilot session token.
///
/// This mirrors rig's synchronous-free GitHub-token auth request shape: a `GET`
/// to GitHub Copilot's token endpoint with the Copilot editor identity headers
/// and the GitHub token in `Authorization: token ...`.
fn exchange_github_token(access_token: &str) -> Result<Copilot, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(GITHUB_COPILOT_TOKEN_TIMEOUT)
        .build()
        .map_err(|err| format!("failed to build GitHub Copilot token exchange client: {err}"))?;

    let response = client
        .get(GITHUB_COPILOT_TOKEN_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("editor-version", COPILOT_EDITOR_VERSION)
        .header("editor-plugin-version", COPILOT_EDITOR_PLUGIN_VERSION)
        .header("user-agent", COPILOT_USER_AGENT)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("token {access_token}"),
        )
        .send()
        .map_err(|err| format!("GitHub Copilot token exchange failed: {err}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "GitHub Copilot token exchange failed with HTTP {}",
            response.status()
        ));
    }

    let record: CopilotTokenResponse = response
        .json()
        .map_err(|err| format!("GitHub Copilot token exchange response was invalid: {err}"))?;
    let CopilotTokenResponse { token, endpoints } = record;
    let api_base = endpoints
        .and_then(|endpoints| endpoints.api)
        .map(|api| api.trim().to_string())
        .filter(|api| !api.is_empty());
    let session_token = token
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            "GitHub Copilot token exchange response did not include a session token".to_string()
        })?;

    Ok(copilot_client(session_token, api_base))
}

fn copilot_client(session_token: String, api_base: Option<String>) -> Copilot {
    let config = CopilotConfig::new(session_token);
    match api_base {
        Some(api_base) => config.with_base_url(api_base).client(),
        None => config.client(),
    }
}

// ── CopilotRunState ──────────────────────────────────────────────────────

struct CopilotRunState {
    client: Copilot,
    model: String,
    tool_filter: Option<Vec<String>>,
    intent: Option<CopilotIntent>,
    strict_tools: bool,
    tool_result_array_content: bool,
    resolved: ResolvedParams,
    last_ctx: Mutex<Option<RunContext>>,
    cancels: Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>,
    next_id: AtomicU64,
    log_label: String,
}

/// Parse Copilot-specific typed flags from the client-spec parameter map.
///
/// `intent` accepts `edits` or `panel`; `strict_tools` and
/// `tool_result_array_content` are strict bools.  A malformed value is a
/// build-time error, never a silent fallback.
fn parse_extra_params(
    extra_params: &indexmap::IndexMap<String, String>,
) -> Result<(Option<CopilotIntent>, bool, bool), String> {
    let intent = match extra_params.get("intent") {
        Some(v) => match v.as_str() {
            "edits" => Some(CopilotIntent::Edits),
            "panel" => Some(CopilotIntent::Panel),
            _ => {
                return Err(format!(
                    "copilot: invalid value {v:?} for client param \"intent\": expected one of edits, panel"
                ))
            }
        },
        None => None,
    };
    let strict_tools = match extra_params.get("strict_tools") {
        Some(v) => params::parse_bool("copilot", "strict_tools", v)?,
        None => false,
    };
    let tool_result_array_content = match extra_params.get("tool_result_array_content") {
        Some(v) => params::parse_bool("copilot", "tool_result_array_content", v)?,
        None => false,
    };
    Ok((intent, strict_tools, tool_result_array_content))
}

impl CopilotRunState {
    fn make_model(&self, override_model: Option<&str>) -> DynModel<Completion> {
        let model_name = match override_model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => self.model.clone(),
        };
        let mut mdl = self.client.completion(&model_name);
        if let Some(intent) = self.intent {
            mdl.wire = mdl.wire.with_intent(intent);
        }
        if self.strict_tools {
            mdl.wire = mdl.wire.with_strict_tools();
        }
        if self.tool_result_array_content {
            mdl.wire = mdl.wire.with_tool_result_array_content();
        }
        mdl.erase()
    }
}

// ── CopilotBackend ───────────────────────────────────────────────────────

pub struct CopilotBackend {
    state: CopilotRunState,
}

impl CopilotBackend {
    /// Build a Copilot backend.
    ///
    /// Auth precedence: `GITHUB_COPILOT_API_KEY` → `COPILOT_API_KEY` →
    /// `providers.yaml` `"copilot"` entry → `GITHUB_TOKEN` → `GH_TOKEN` →
    /// `gh auth token` → error.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let client = resolve_auth()?;

        let model = if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model.to_string()
        };

        let tool_filter = openai_protocol::tool_filter(native_block);

        let resolved = Self::resolve_params(extra_params)?;
        let (intent, strict_tools, tool_result_array_content) =
            parse_extra_params(extra_params)?;

        Ok(Arc::new(Self {
            state: CopilotRunState {
                client,
                model,
                tool_filter,
                intent,
                strict_tools,
                tool_result_array_content,
                resolved,
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "CopilotBackend".to_string(),
            },
        }))
    }
}

#[async_trait]
impl Backend for CopilotBackend {
    fn make_model(&self, spec: &str) -> Option<DynModel<Completion>> {
        let (provider, model) = openai_protocol::provider_and_model(spec)?;
        if provider == PROVIDER_NAME {
            Some(self.state.client.completion(model).erase())
        } else {
            None
        }
    }

    async fn run(
        &self,
        params: RunParams,
        interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError> {
        let idle_timeout = params
            .idle_timeout
            .unwrap_or_else(crate::config::stream_idle_timeout);
        let prefix = if params.label.is_empty() {
            String::new()
        } else {
            format!("[{}] ", params.label)
        };
        let ctx = RunContext {
            params: params.clone(),
            prefix: prefix.clone(),
            idle_timeout,
            expected_artifact_paths: params.expected_artifact_paths.clone(),
            reminder_budget: crate::config::artifact_reminder_budget(),
            completion_nudge_budget: crate::config::completion_nudge_budget(),
            stream_events: params.stream_events.clone(),
            prepopulated_messages: params.prepopulated_messages.clone(),
        };
        *self.state.last_ctx.lock().unwrap() = Some(ctx.clone());

        let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);

        if cancel.is_cancelled() {
            return Err(ClientError::Runtime {
                message: "cancelled".into(),
            });
        }

        let gremlin_id = ctx.params.gremlin_id.clone().unwrap_or_default();
        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        self.state
            .cancels
            .lock()
            .unwrap()
            .entry(gremlin_id.clone())
            .or_default()
            .insert(id, cancel.clone());

        let model = self.state.make_model(ctx.params.model.as_deref());
        let result = run_agent_loop(
            model,
            &params.prompt,
            ctx,
            cancel.clone(),
            LoopOpts {
                extra: self.state.resolved.additional_value(),
                tool_filter: self.state.tool_filter.as_deref(),
                classify_error: Some(default_classify as ErrorClassifier),
                temperature: self.state.resolved.temperature,
                max_tokens: self.state.resolved.max_tokens,
            },
            interactive,
        )
        .await;

        if let Ok(mut guard) = self.state.cancels.lock() {
            if let Some(inner) = guard.get_mut(&gremlin_id) {
                inner.remove(&id);
                if inner.is_empty() {
                    guard.remove(&gremlin_id);
                }
            }
        }
        result
    }

    async fn resume(&self) -> Result<CompletedRun, ClientError> {
        let params = {
            let guard = self.state.last_ctx.lock().unwrap();
            let ctx = guard.as_ref().ok_or_else(|| ClientError::Runtime {
                message: "resume() called before run()".into(),
            })?;
            ctx.params.clone()
        };
        self.run(params, None).await
    }

    fn reap_all(&self, gremlin_id: &str) {
        if let Ok(mut guard) = self.state.cancels.lock() {
            let tokens: Vec<_> = guard
                .remove(gremlin_id)
                .into_iter()
                .flat_map(|m| m.into_values())
                .collect();
            let count = tokens.len();
            log::debug!(
                "{}::reap_all: cancelling {count} in-flight token(s) for gremlin_id={gremlin_id} (model={})",
                self.state.log_label,
                self.state.model,
            );
            for token in &tokens {
                token.cancel();
            }
        }
    }

    fn total_cost_usd(&self) -> Option<f64> {
        None
    }

    fn resolve_params(raw: &indexmap::IndexMap<String, String>) -> Result<ResolvedParams, String> {
        // Validate the typed flags strictly (so a malformed value errors at
        // build time), then resolve temperature/max_tokens and passthrough.
        parse_extra_params(raw)?;

        let mut additional = serde_json::Map::new();
        let mut temperature = None;
        let mut max_tokens = None;

        for (k, v) in raw {
            match k.as_str() {
                "temperature" => temperature = Some(params::parse_f64("copilot", k, v)?),
                "max_tokens" => max_tokens = Some(params::parse_u64("copilot", k, v)?),
                "intent" | "strict_tools" | "tool_result_array_content" => {}
                _ => {
                    additional.insert(k.clone(), params::coerce(v));
                }
            }
        }

        Ok(ResolvedParams {
            temperature,
            max_tokens,
            additional,
        })
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::super::agent_loop::CancelToken;
    use super::*;
    use crate::test_support::EnvGuard;

    const FAKE_API_KEY: &str = "tid=1;exp=9999999999";

    fn scrub_copilot_env(guard: &mut EnvGuard) {
        guard.remove("GITHUB_COPILOT_API_KEY");
        guard.remove("COPILOT_API_KEY");
        guard.remove("GITHUB_TOKEN");
        guard.remove("GH_TOKEN");
        guard.remove("XDG_CONFIG_HOME");
    }

    /// Set up an isolated sandbox with no providers.yaml, no `gh` on `PATH`,
    /// and no GitHub credentials in the environment. Returns the guard.
    fn isolated_env() -> EnvGuard {
        let mut guard = EnvGuard::lock();
        scrub_copilot_env(&mut guard);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        guard.set("HOME", tmp.path());
        // Keep any real `gh` binary away from `resolve_auth`.
        guard.set("PATH", tmp.path());
        guard
    }

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "copilot");
        assert_eq!(DEFAULT_MODEL, "gpt-4o");
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let client = Copilot::new("tid=1;exp=9999999999");
        let backend = CopilotBackend {
            state: CopilotRunState {
                client,
                model: "gpt-4o".into(),
                tool_filter: None,
                intent: None,
                strict_tools: false,
                tool_result_array_content: false,
                resolved: ResolvedParams::default(),
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "test".into(),
            },
        };
        let a = CancelToken::new();
        let b = CancelToken::new();
        let sibling = CancelToken::new();
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-test".to_string())
            .or_default()
            .insert(1, a.clone());
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-test".to_string())
            .or_default()
            .insert(2, b.clone());
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-sibling".to_string())
            .or_default()
            .insert(3, sibling.clone());
        backend.reap_all("gr-test");
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(backend
            .state
            .cancels
            .lock()
            .unwrap()
            .get("gr-test")
            .is_none());
        assert!(!sibling.is_cancelled());
        assert!(backend
            .state
            .cancels
            .lock()
            .unwrap()
            .get("gr-sibling")
            .is_some());
    }

    #[test]
    fn build_rejects_missing_credentials() {
        let _guard = isolated_env();

        let result = CopilotBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("no API key for provider") || err.contains("no credentials for provider"),
            "got: {err}"
        );
    }

    // ── auth tests ───────────────────────────────────────────────────

    #[test]
    fn auth_github_copilot_api_key() {
        let mut guard = isolated_env();
        guard.set("GITHUB_COPILOT_API_KEY", FAKE_API_KEY);
        assert!(resolve_auth().is_ok());
    }

    #[test]
    fn auth_copilot_api_key_fallback() {
        let mut guard = isolated_env();
        guard.set("COPILOT_API_KEY", FAKE_API_KEY);
        assert!(resolve_auth().is_ok());
    }

    #[test]
    fn auth_providers_yaml() {
        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            format!(r#"{{"copilot": {{"api-key": "{FAKE_API_KEY}"}}}}"#),
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        assert!(resolve_auth().is_ok());
    }

    #[test]
    fn auth_session_token_beats_github_token() {
        let mut guard = isolated_env();
        guard.set("GITHUB_COPILOT_API_KEY", FAKE_API_KEY);
        guard.set("GITHUB_TOKEN", "should-not-be-exchanged");
        assert!(resolve_auth().is_ok());
    }

    #[test]
    fn auth_providers_yaml_beats_github_token() {
        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            format!(r#"{{"copilot": {{"api-key": "{FAKE_API_KEY}"}}}}"#),
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        guard.set("GITHUB_TOKEN", "should-not-be-exchanged");
        assert!(resolve_auth().is_ok());
    }

    #[test]
    fn auth_github_token_env_precedence() {
        let mut guard = isolated_env();
        guard.set("GH_TOKEN", "from-gh-token");
        guard.set("GITHUB_TOKEN", "from-github-token");
        assert_eq!(github_access_token().as_deref(), Some("from-github-token"));
    }

    #[test]
    fn auth_gh_token_env_fallback() {
        let mut guard = isolated_env();
        guard.set("GH_TOKEN", "from-gh-token");
        assert_eq!(github_access_token().as_deref(), Some("from-gh-token"));
    }

    #[cfg(unix)]
    #[test]
    fn auth_gh_cli_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();
        let gh = tmp.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\necho from-gh-cli\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        guard.set("PATH", tmp.path());
        assert_eq!(github_access_token().as_deref(), Some("from-gh-cli"));
    }


    // ── parse_extra_params tests ─────────────────────────────────────

    #[test]
    fn extra_params_intent_edits() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("intent".into(), "edits".into());
        let (intent, _, _) = parse_extra_params(&ep).unwrap();
        assert_eq!(intent, Some(CopilotIntent::Edits));
    }

    #[test]
    fn extra_params_intent_panel() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("intent".into(), "panel".into());
        let (intent, _, _) = parse_extra_params(&ep).unwrap();
        assert_eq!(intent, Some(CopilotIntent::Panel));
    }

    #[test]
    fn extra_params_intent_bogus_is_error() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("intent".into(), "bogus".into());
        let err = parse_extra_params(&ep).unwrap_err();
        assert!(err.contains("intent"), "{err}");
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn extra_params_intent_missing_is_none() {
        let (intent, _, _) = parse_extra_params(&indexmap::IndexMap::new()).unwrap();
        assert_eq!(intent, None);
    }

    #[test]
    fn extra_params_strict_tools_true() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "true".into());
        let (_, strict_tools, _) = parse_extra_params(&ep).unwrap();
        assert!(strict_tools);
    }

    #[test]
    fn extra_params_strict_tools_one() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "1".into());
        let (_, strict_tools, _) = parse_extra_params(&ep).unwrap();
        assert!(strict_tools);
    }

    #[test]
    fn extra_params_strict_tools_malformed_is_error() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "yes".into());
        let err = parse_extra_params(&ep).unwrap_err();
        assert!(err.contains("strict_tools"), "{err}");
    }

    #[test]
    fn extra_params_strict_tools_false_by_default() {
        let (_, strict_tools, _) = parse_extra_params(&indexmap::IndexMap::new()).unwrap();
        assert!(!strict_tools);
    }

    #[test]
    fn extra_params_tool_result_array_content_true() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("tool_result_array_content".into(), "true".into());
        let (_, _, tool_result_array_content) = parse_extra_params(&ep).unwrap();
        assert!(tool_result_array_content);
    }

    #[test]
    fn extra_params_tool_result_array_content_one() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("tool_result_array_content".into(), "1".into());
        let (_, _, tool_result_array_content) = parse_extra_params(&ep).unwrap();
        assert!(tool_result_array_content);
    }

    #[test]
    fn extra_params_tool_result_array_content_false_by_default() {
        let (_, _, tool_result_array_content) =
            parse_extra_params(&indexmap::IndexMap::new()).unwrap();
        assert!(!tool_result_array_content);
    }
}