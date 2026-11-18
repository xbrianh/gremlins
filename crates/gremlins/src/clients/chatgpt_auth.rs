//! ChatGPT OAuth sign-in: shared store location, static-source lookup, and login.

use std::path::PathBuf;
use std::sync::Arc;

use rig_core::http_client::DynHttpClient;
use rig_core::providers::chatgpt::auth::{
    AuthSource, Authenticator, DeviceCodeHandler, FileTokenStore, SignInMethod, SignInPrompt,
};

use crate::clients::config::{static_token, StaticSecret};

pub(crate) const ACCESS_TOKEN_ENV: &str = "CHATGPT_ACCESS_TOKEN";
const PROVIDER_NAME: &str = "chatgpt";
/// Whether rig may refresh stored tokens automatically.
const AUTO_REFRESH: bool = true;

/// Directory holding stored OAuth credentials (`chatgpt.json` lives here).
pub(crate) fn store_dir() -> PathBuf {
    crate::config::user_config_root().join("credentials")
}

/// The static credential that currently wins, if any. `chatgpt.api-key` is not a token.
pub(crate) fn static_source() -> Option<StaticSecret> {
    static_token(ACCESS_TOKEN_ENV, PROVIDER_NAME)
}

/// Label of the static source that shadows OAuth login, if any (never the secret).
pub fn static_source_label() -> Option<String> {
    static_source().map(|found| found.source)
}

/// Something the operator must be told or shown while signing in.
#[derive(Debug, Clone)]
pub enum LoginEvent {
    /// Browser sign-in page; `launched` says whether the opener started.
    Browser { url: String, launched: bool },
    /// Both callback ports are occupied; a device code follows.
    BrowserUnavailable(String),
    /// Enter `code` at `url` on any device.
    DeviceCode { url: String, code: String },
}

/// The sign-in method to use, honoring a forced device code.
fn detect_method(device_code: bool) -> SignInMethod {
    SignInMethod::detect(device_code)
}

/// Sign in with `method` through `authenticator`, reporting prompts to `events`.
/// Returns the signed-in account ID, if any. Success implies the record was stored.
async fn sign_in(
    authenticator: &Authenticator,
    method: SignInMethod,
    events: Arc<dyn Fn(LoginEvent) + Send + Sync>,
) -> Result<Option<String>, String> {
    let http = DynHttpClient::new(rig_reqwest::shared());
    let prompt_events = Arc::clone(&events);
    let context = authenticator
        .sign_in(&http, method, move |prompt| {
            prompt_events(match prompt {
                SignInPrompt::Browser(page) => LoginEvent::Browser {
                    url: page.authorize_url,
                    launched: page.browser_launched,
                },
                SignInPrompt::BrowserUnavailable(reason) => LoginEvent::BrowserUnavailable(reason),
            })
        })
        .await
        .map_err(|error| error.to_string())?;
    Ok(context.account_id)
}

/// An authenticator over the shared credential store, showing device codes to `events`.
fn authenticator(events: Arc<dyn Fn(LoginEvent) + Send + Sync>) -> Authenticator {
    let handler = DeviceCodeHandler::new(move |prompt| {
        events(LoginEvent::DeviceCode {
            url: prompt.verification_uri,
            code: prompt.user_code,
        })
    });
    Authenticator::new(
        AuthSource::OAuth,
        FileTokenStore::new(store_dir()),
        handler,
        AUTO_REFRESH,
    )
}

/// Full login: pick the method, sign in, return the account ID.
/// Returns `Ok` only after rig has persisted the record; any storage failure is an `Err`.
/// Callers must check `static_source_label` first; login refuses nothing itself.
pub async fn login(
    device_code: bool,
    events: Arc<dyn Fn(LoginEvent) + Send + Sync>,
) -> Result<Option<String>, String> {
    let auth = authenticator(Arc::clone(&events));
    sign_in(&auth, detect_method(device_code), events).await
}
