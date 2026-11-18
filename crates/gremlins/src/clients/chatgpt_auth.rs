//! ChatGPT OAuth sign-in: shared store location, static-source lookup, and login.

use std::fmt;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;

use rig_core::error::BoxError;
use rig_core::http_client::DynHttpClient;
use rig_core::providers::chatgpt::auth::{AuthContext, AuthError};
use rig_core::providers::chatgpt::auth::{
    AuthSource, Authenticator, DeviceCodeHandler, FileTokenStore, SignInMethod, SignInPrompt,
    SignedOut, StoreLock, TokenStore,
};
use rig_core::wasm_compat::WasmBoxedFuture;

use crate::clients::config::{static_token, StaticSecret};

pub(crate) const ACCESS_TOKEN_ENV: &str = "CHATGPT_ACCESS_TOKEN";
const PROVIDER_NAME: &str = "chatgpt";

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
        true,
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

/// Which credential a ChatGPT run would use, by the shared precedence rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedSource {
    /// `CHATGPT_ACCESS_TOKEN`.
    Environment,
    /// `chatgpt.token` in `providers.yaml`.
    Config,
    /// The stored OAuth sign-in.
    OAuth,
    /// Nothing is configured.
    None,
}

/// Display-safe facts from a readable stored record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredSignIn {
    pub account_id: Option<String>,
    /// Access-token expiry (unix seconds); `None` when unknown.
    pub expires_at: Option<i64>,
}

/// The local OAuth record, independent of which source is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordState {
    Absent,
    Stored(StoredSignIn),
    /// Unreadable or malformed; the path and a short reason, never contents.
    Unreadable {
        path: PathBuf,
        reason: String,
    },
}

/// Local credential state. This is not provider validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptStatus {
    pub source: SelectedSource,
    pub record: RecordState,
}

#[derive(serde::Deserialize)]
struct RecordView {
    #[serde(rename = "access_token")]
    _access_token: Option<String>,
    #[serde(rename = "refresh_token")]
    _refresh_token: Option<String>,
    #[serde(rename = "id_token")]
    _id_token: Option<String>,
    expires_at: Option<i64>,
    account_id: Option<String>,
}

/// Read-only inspection: no auth context, network, refresh, prompt, or store write.
pub fn status() -> ChatGptStatus {
    let record = match stored_oauth() {
        Err(SignInError::Missing) => RecordState::Absent,
        Err(SignInError::Store { path, detail }) => RecordState::Unreadable {
            path,
            reason: detail,
        },
        Err(_) => RecordState::Unreadable {
            path: FileTokenStore::new(store_dir()).path(PROVIDER_NAME),
            reason: "credential store unavailable".into(),
        },
        Ok(store) => {
            let path = store.path(PROVIDER_NAME);
            match std::fs::read(&path) {
                Err(error) if error.kind() == ErrorKind::NotFound => RecordState::Absent,
                Err(error) => RecordState::Unreadable {
                    path,
                    reason: error.kind().to_string(),
                },
                Ok(bytes) => match serde_json::from_slice::<RecordView>(&bytes) {
                    Ok(view) => RecordState::Stored(StoredSignIn {
                        account_id: view.account_id,
                        expires_at: view.expires_at,
                    }),
                    Err(_) => RecordState::Unreadable {
                        path,
                        reason: "malformed record".into(),
                    },
                },
            }
        }
    };
    let source = match static_source() {
        Some(found) if found.source == ACCESS_TOKEN_ENV => SelectedSource::Environment,
        Some(_) => SelectedSource::Config,
        None if matches!(record, RecordState::Stored(_)) => SelectedSource::OAuth,
        None => SelectedSource::None,
    };
    ChatGptStatus { source, record }
}

/// What logging out of the stored sign-in did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogoutOutcome {
    NotSignedIn,
    /// The provider confirmed revocation and the record was removed.
    Revoked,
    /// The record was removed but revocation is unconfirmed.
    RemovedUnconfirmed,
    /// The store failed; nothing is claimed removed.
    StoreFailure {
        path: PathBuf,
    },
}

/// Sign out of the stored OAuth sign-in only, even when a static source wins.
pub async fn logout() -> LogoutOutcome {
    let store = match stored_oauth() {
        Ok(store) => store,
        Err(SignInError::Store { path, .. }) => return LogoutOutcome::StoreFailure { path },
        Err(SignInError::Missing) => return LogoutOutcome::NotSignedIn,
        Err(_) => {
            return LogoutOutcome::StoreFailure {
                path: FileTokenStore::new(store_dir()).path(PROVIDER_NAME),
            }
        }
    };
    let path = store.path(PROVIDER_NAME);
    let authenticator = Authenticator::new(
        AuthSource::OAuth,
        store,
        DeviceCodeHandler::default(),
        false,
    );
    let http = DynHttpClient::new(rig_reqwest::shared());
    match authenticator.sign_out(&http).await {
        Ok(SignedOut::NotSignedIn) => LogoutOutcome::NotSignedIn,
        Ok(SignedOut::Forgotten { revoked: Ok(()) }) => LogoutOutcome::Revoked,
        Ok(SignedOut::Forgotten { revoked: Err(_) }) => LogoutOutcome::RemovedUnconfirmed,
        Err(_) => LogoutOutcome::StoreFailure { path },
    }
}

/// Why a stored sign-in could not yield a credential, each with its own remedy.
#[derive(Debug)]
pub(crate) enum SignInError {
    /// No credential source of any kind is configured.
    Missing,
    /// No usable sign-in: removed or rejected by the provider.
    Required,
    /// The record could not be read, decoded, or written.
    Store { path: PathBuf, detail: String },
    /// A refresh failed for a reason that may pass.
    Transient,
}

impl fmt::Display for SignInError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "no credentials for provider '{PROVIDER_NAME}': set {ACCESS_TOKEN_ENV}, \
                 add {PROVIDER_NAME}.token in providers.yaml, or run `gremlins auth chatgpt login`"
            ),
            Self::Required => write!(
                f,
                "ChatGPT sign-in required: run `gremlins auth chatgpt login`"
            ),
            Self::Store { path, detail } => write!(
                f,
                "ChatGPT credential store failed at {}: {detail}",
                path.display()
            ),
            Self::Transient => write!(
                f,
                "ChatGPT token refresh failed; retry shortly \
                 (if it persists, check provider status and network configuration)"
            ),
        }
    }
}

impl std::error::Error for SignInError {}

/// The store holding the OAuth record, provided that a record file exists.
/// Only a missing entry counts as "no credentials"; anything else is a store error.
pub(crate) fn stored_oauth() -> Result<FileTokenStore, SignInError> {
    let store = FileTokenStore::new(store_dir());
    let path = store.path(PROVIDER_NAME);
    let store_error = |detail: String| SignInError::Store {
        path: path.clone(),
        detail,
    };
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(store),
        Ok(_) => Err(store_error("not a regular file".into())),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(SignInError::Missing),
        Err(error) => Err(store_error(error.kind().to_string())),
    }
}

/// Rig uses the same JSON error for store reads and refresh responses. Validate the
/// exact bytes rig will read, under its store lock, so local errors retain their origin.
struct CheckedStore(FileTokenStore);

impl TokenStore for CheckedStore {
    fn load<'a>(&'a self, key: &'a str) -> WasmBoxedFuture<'a, Result<Option<String>, BoxError>> {
        Box::pin(async move {
            let record = self.0.load(key).await?;
            if let Some(text) = &record {
                serde_json::from_str::<RecordView>(text).map_err(|_| "malformed record")?;
            }
            Ok(record)
        })
    }

    fn save<'a>(
        &'a self,
        key: &'a str,
        record: &'a str,
    ) -> WasmBoxedFuture<'a, Result<(), BoxError>> {
        self.0.save(key, record)
    }

    fn delete<'a>(&'a self, key: &'a str) -> WasmBoxedFuture<'a, Result<(), BoxError>> {
        self.0.delete(key)
    }

    fn lock<'a>(&'a self, key: &'a str) -> WasmBoxedFuture<'a, Result<StoreLock, BoxError>> {
        self.0.lock(key)
    }
}

/// Resolve the stored sign-in through a fresh authenticator that never prompts.
pub(crate) async fn resolve_stored(
    store: &FileTokenStore,
    http: &DynHttpClient,
) -> Result<AuthContext, SignInError> {
    let authenticator = Authenticator::new(
        AuthSource::OAuth,
        CheckedStore(store.clone()),
        DeviceCodeHandler::default(),
        false,
    );
    let store_error = |detail: String| SignInError::Store {
        path: store.path(PROVIDER_NAME),
        detail,
    };
    authenticator
        .auth_context(http)
        .await
        .map_err(|error| match error {
            AuthError::Io(error) => store_error(error.kind().to_string()),
            AuthError::Store(error) => store_error(error.to_string()),
            AuthError::Json(_) | AuthError::Http(_) => SignInError::Transient,
            AuthError::Message(message) => classify_message(&message),
        })
}

/// Sort rig's textual refresh failures (`ChatGPT token refresh failed: <status> ...`).
/// This is the single place coupled to that wording. Rejected authorization
/// (4xx other than 408/429) needs a sign-in; anything else that is a refresh
/// failure may pass. Messages in any other form mean no usable sign-in.
fn classify_message(message: &str) -> SignInError {
    const REFRESH: &str = "ChatGPT token refresh failed: ";
    let Some(rest) = message.strip_prefix(REFRESH) else {
        return SignInError::Required;
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    match digits.parse::<u16>() {
        Ok(408 | 429) => SignInError::Transient,
        Ok(400..=499) => SignInError::Required,
        _ => SignInError::Transient,
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    fn kind(message: &str) -> &'static str {
        match classify_message(message) {
            SignInError::Required => "required",
            SignInError::Transient => "transient",
            _ => "other",
        }
    }

    #[tokio::test]
    async fn malformed_local_record_is_a_store_error_before_refresh() {
        use rig_core::test_utils::SequencedHttpClient;

        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::new(dir.path());
        let path = store.path(PROVIDER_NAME);
        let http = SequencedHttpClient::new([]);
        for record in ["{", r#"{"access_token":{"bad":"secret-marker"}}"#] {
            std::fs::write(&path, record).unwrap();
            let err = resolve_stored(&store, &DynHttpClient::new(http.clone()))
                .await
                .unwrap_err();
            assert!(
                matches!(err, SignInError::Store { path: ref p, ref detail } if p == &path && detail == "malformed record")
            );
        }
        assert!(http.requests().is_empty());
    }

    #[tokio::test]
    async fn malformed_refresh_response_is_not_a_store_error() {
        use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};

        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::new(dir.path());
        let path = store.path(PROVIDER_NAME);
        let record = r#"{"refresh_token":"refresh","expires_at":0}"#;
        std::fs::write(&path, record).unwrap();
        for response in ["invalid JSON", r#"{"refresh_token":"secret-marker"}"#] {
            let http = SequencedHttpClient::new([MockHttpResponse::success(response)]);
            let err = resolve_stored(&store, &DynHttpClient::new(http.clone()))
                .await
                .unwrap_err();
            assert!(matches!(err, SignInError::Transient));
            assert_eq!(http.requests().len(), 1);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), record);
        }
    }

    #[test]
    fn classifies_refresh_failures_by_status() {
        assert_eq!(
            kind("ChatGPT token refresh failed: 401 Unauthorized"),
            "required"
        );
        assert_eq!(kind("ChatGPT token refresh failed: 400"), "required");
        assert_eq!(kind("ChatGPT token refresh failed: 429"), "transient");
        assert_eq!(kind("ChatGPT token refresh failed: 408"), "transient");
        assert_eq!(kind("ChatGPT token refresh failed: 503"), "transient");
        assert_eq!(kind("ChatGPT token refresh failed: oops"), "transient");
        assert_eq!(kind("no refresh token"), "required");
    }
}
