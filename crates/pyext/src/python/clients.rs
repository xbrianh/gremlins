use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use pyo3::prelude::*;
use pyo3::types::PyList;

use gremlins::clients::backend::{Backend, ClientError, RunParams};
use gremlins::clients::cmd_backend::CmdBackend;
use gremlins::clients::openai_backend::{OpenAiBackend, OpenAiProvider};
use gremlins::clients::protocol::CompletedRun;
use rig_core::providers::openai;

/// Python-exposed Client.
#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub struct Client {
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    model: String,
    #[pyo3(get)]
    extra_params: IndexMap<String, String>,
    native_block: HashMap<String, Vec<String>>,
    inner: Arc<Mutex<Option<Arc<dyn Backend>>>>,
}

fn map_error(e: ClientError) -> PyErr {
    match e {
        ClientError::Timeout { message } => pyo3::exceptions::PyTimeoutError::new_err(message),
        ClientError::ApiServerError { message } => {
            pyo3::exceptions::PyRuntimeError::new_err(message)
        }
        ClientError::Runtime { message } => pyo3::exceptions::PyRuntimeError::new_err(message),
    }
}

/// Python-exposed token usage summary.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct PyUsageStats {
    #[pyo3(get)]
    prompt_tokens: u64,
    #[pyo3(get)]
    completion_tokens: u64,
    #[pyo3(get)]
    cached_input_tokens: u64,
    #[pyo3(get)]
    cache_creation_input_tokens: u64,
    #[pyo3(get)]
    reasoning_tokens: u64,
    #[pyo3(get)]
    turns: usize,
}

#[pymethods]
impl PyUsageStats {
    #[new]
    fn new(
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_input_tokens: u64,
        cache_creation_input_tokens: u64,
        reasoning_tokens: u64,
        turns: usize,
    ) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            reasoning_tokens,
            turns,
        }
    }
}

/// Python-exposed completed run result.
#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub struct PyCompletedRun {
    #[pyo3(get)]
    exit_code: i32,
    #[pyo3(get)]
    text_result: Option<String>,
    #[pyo3(get)]
    cost_usd: Option<f64>,
    #[pyo3(get)]
    token_usage: Option<PyUsageStats>,
}

#[pymethods]
impl PyCompletedRun {
    #[new]
    fn new(
        exit_code: i32,
        text_result: Option<String>,
        cost_usd: Option<f64>,
        token_usage: Option<PyUsageStats>,
    ) -> Self {
        Self {
            exit_code,
            text_result,
            cost_usd,
            token_usage,
        }
    }
}

fn parse_spec(s: &str) -> PyResult<(String, String, IndexMap<String, String>)> {
    if !s.contains(':') {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "invalid client specifier {s:?}: expected 'provider:model'"
        )));
    }
    let (provider, rest) = s.split_once(':').unwrap();
    if provider.is_empty() {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "invalid client specifier {s:?}: provider must not be empty"
        )));
    }
    if rest.is_empty() {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "invalid client specifier {s:?}: model must not be empty"
        )));
    }
    let mut extra_params = IndexMap::new();
    let model = if provider == "cmd" {
        rest.to_string()
    } else {
        let params_pattern = regex::Regex::new(
            r":([a-zA-Z_][a-zA-Z0-9_]*=[^,]+)(?:,([a-zA-Z_][a-zA-Z0-9_]*=[^,]+))*$",
        )
        .unwrap();
        if let Some(m) = params_pattern.find(rest) {
            let params_str = &m.as_str()[1..];
            for pair in params_str.split(',') {
                if let Some((k, v)) = pair.split_once('=') {
                    if extra_params.contains_key(k) {
                        return Err(pyo3::exceptions::PyValueError::new_err(format!(
                            "duplicate key {k:?} in client params {params_str:?}"
                        )));
                    }
                    extra_params.insert(k.to_string(), v.to_string());
                }
            }
            rest[..m.start()].to_string()
        } else {
            rest.to_string()
        }
    };
    if model.is_empty() {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "invalid client specifier {s:?}: model must not be empty"
        )));
    }
    Ok((provider.to_string(), model, extra_params))
}

fn default_native_block() -> HashMap<String, Vec<String>> {
    HashMap::from([(
        "allowed_tools".to_string(),
        vec![
            "Bash".to_string(),
            "Edit".to_string(),
            "Read".to_string(),
            "Write".to_string(),
            "Grep".to_string(),
            "Glob".to_string(),
        ],
    )])
}

fn resolve_api_key(kind: OpenAiProvider) -> Option<String> {
    gremlins::config::api_key(kind.api_key_env(), kind.name())
}

fn build_openai_backend(
    kind: OpenAiProvider,
    model: &str,
    native_block: &HashMap<String, Vec<String>>,
    extra_params: &IndexMap<String, String>,
) -> PyResult<Arc<dyn Backend>> {
    let api_key = resolve_api_key(kind).ok_or_else(|| {
        let path = gremlins::config::user_config_root().join("providers.json");
        pyo3::exceptions::PyRuntimeError::new_err(format!(
            "no API key for provider '{}': set {} or add an entry in {}",
            kind.name(),
            kind.api_key_env(),
            path.display(),
        ))
    })?;
    let client = openai::Client::builder()
        .api_key(rig_core::client::BearerAuth::from(api_key))
        .base_url(kind.base_url())
        .build()
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?
        .completions_api();
    let tool_filter = native_block.get("allowed_tools").cloned();
    Ok(Arc::new(OpenAiBackend::new(
        kind,
        client,
        model.to_string(),
        tool_filter,
        extra_params
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )))
}

#[pymethods]
impl Client {
    #[new]
    #[pyo3(signature = (provider, model, native_block=None, extra_params=None))]
    fn new(
        provider: String,
        model: String,
        native_block: Option<HashMap<String, Vec<String>>>,
        extra_params: Option<IndexMap<String, String>>,
    ) -> PyResult<Self> {
        let native_block = native_block.unwrap_or_else(default_native_block);
        let extra_params = extra_params.unwrap_or_default();

        if !matches!(provider.as_str(), "openai" | "xai" | "openrouter" | "cmd") {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown provider '{provider}'"
            )));
        }
        Ok(Client {
            provider,
            model,
            extra_params,
            native_block,
            inner: Arc::new(Mutex::new(None)),
        })
    }

    #[staticmethod]
    fn parse(s: &str) -> PyResult<Self> {
        let (provider, model, extra_params) = parse_spec(s)?;
        if !matches!(provider.as_str(), "openai" | "xai" | "openrouter" | "cmd") {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown provider '{provider}'"
            )));
        }
        Ok(Client {
            provider,
            model,
            extra_params,
            native_block: default_native_block(),
            inner: Arc::new(Mutex::new(None)),
        })
    }

    fn __str__(&self) -> String {
        let mut s = format!("{}:{}", self.provider, self.model);
        if !self.extra_params.is_empty() {
            let params: Vec<String> = self
                .extra_params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            s.push(':');
            s.push_str(&params.join(","));
        }
        s
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.model == other.model
            && self.extra_params == other.extra_params
    }

    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.provider.hash(&mut h);
        self.model.hash(&mut h);
        let mut keys: Vec<&String> = self.extra_params.keys().collect();
        keys.sort();
        for k in keys {
            k.hash(&mut h);
            self.extra_params[k].hash(&mut h);
        }
        h.finish()
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (prompt, label, model=None, raw_path=None, capture_events=false, on_timeout_prompt=None, max_retries=3, cwd=None, artifact_dir=None, idle_timeout=None, extra_env=None, expected_artifact_paths=None, artifact_reminder_count=0, system_prompt=None))]
    fn run<'py>(
        &self,
        py: Python<'py>,
        prompt: String,
        label: String,
        model: Option<String>,
        raw_path: Option<PathBuf>,
        capture_events: bool,
        on_timeout_prompt: Option<String>,
        max_retries: usize,
        cwd: Option<PathBuf>,
        artifact_dir: Option<PathBuf>,
        idle_timeout: Option<f64>,
        extra_env: Option<HashMap<String, String>>,
        expected_artifact_paths: Option<Vec<PathBuf>>,
        artifact_reminder_count: usize,
        system_prompt: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let backend = self.get_or_build_backend()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let params = RunParams {
                prompt,
                label,
                model,
                raw_path,
                capture_events,
                on_timeout_prompt,
                max_retries,
                cwd,
                artifact_dir,
                idle_timeout,
                extra_env,
                expected_artifact_paths: expected_artifact_paths.unwrap_or_default(),
                artifact_reminder_count,
                system_prompt,
            };
            let result = backend.run(params).await.map_err(map_error)?;
            Python::attach(|py| PyCompletedRun::from_rust(py, &result))
        })
    }

    fn reap_all(&self) {
        if let Some(ref backend) = *self.inner.lock().unwrap() {
            backend.reap_all();
        }
    }

    #[getter]
    fn total_cost_usd(&self) -> Option<f64> {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| b.total_cost_usd())
    }
}

impl Client {
    fn get_or_build_backend(&self) -> PyResult<Arc<dyn Backend>> {
        {
            let guard = self.inner.lock().unwrap();
            if let Some(ref backend) = *guard {
                return Ok(backend.clone());
            }
        }
        let kind = match self.provider.as_str() {
            "cmd" => {
                let cmd = CmdBackend::new(&self.model)
                    .map_err(pyo3::exceptions::PyValueError::new_err)?;
                let backend: Arc<dyn Backend> = Arc::new(cmd);
                *self.inner.lock().unwrap() = Some(backend.clone());
                return Ok(backend);
            }
            "openai" => OpenAiProvider::OpenAi,
            "xai" => OpenAiProvider::Xai,
            "openrouter" => OpenAiProvider::OpenRouter,
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown provider '{other}'"
                )));
            }
        };
        let backend =
            build_openai_backend(kind, &self.model, &self.native_block, &self.extra_params)?;
        *self.inner.lock().unwrap() = Some(backend.clone());
        Ok(backend)
    }
}

impl PyCompletedRun {
    fn from_rust(py: Python<'_>, r: &CompletedRun) -> PyResult<Py<PyAny>> {
        let usage = r.token_usage.as_ref().map(|u| {
            PyUsageStats::new(
                u.prompt_tokens,
                u.completion_tokens,
                u.cached_input_tokens,
                u.cache_creation_input_tokens,
                u.reasoning_tokens,
                u.turns,
            )
        });
        let instance = PyCompletedRun::new(r.exit_code, r.text_result.clone(), r.cost_usd, usage);
        Ok(Py::new(py, instance)?.into_any())
    }
}

pub fn init_clients_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Client>()?;
    m.add_class::<PyCompletedRun>()?;
    m.add_class::<PyUsageStats>()?;

    // Set Python-visible docstrings — Rust /// doc comments don't propagate.
    m.getattr("PyUsageStats")?
        .setattr("__doc__", "Token usage summary for a model invocation.")?;
    m.getattr("PyCompletedRun")?.setattr(
        "__doc__",
        "Result of a single model run.\n\n\
         Exposes `exit_code`, `text_result`, `cost_usd` and `token_usage`.",
    )?;

    let tools = vec!["Bash", "Edit", "Read", "Write", "Grep", "Glob"];
    let py_tools = PyList::new(m.py(), &tools)?;
    m.add("_DEFAULT_ALLOWED_TOOLS", py_tools)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_basic() {
        let (p, m, e) = parse_spec("openai:gpt-4o").unwrap();
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");
        assert!(e.is_empty());
    }

    #[test]
    fn parse_spec_with_params() {
        let (p, m, e) = parse_spec("openrouter:deepseek/deepseek-v4-pro:reasoning=high").unwrap();
        assert_eq!(p, "openrouter");
        assert_eq!(m, "deepseek/deepseek-v4-pro");
        assert_eq!(e.get("reasoning").unwrap(), "high");
    }

    #[test]
    fn parse_spec_cmd_ignores_params() {
        let (p, m, e) = parse_spec("cmd:echo hello:world=foo").unwrap();
        assert_eq!(p, "cmd");
        assert_eq!(m, "echo hello:world=foo");
        assert!(e.is_empty());
    }

    #[test]
    fn parse_spec_rejects_malformed() {
        for (bad, expected) in [
            ("no-colon", "expected 'provider:model'"),
            (":model", "provider must not be empty"),
            ("provider:", "model must not be empty"),
            ("openai:gpt-4:reasoning=high,reasoning=low", "duplicate key"),
        ] {
            let err = parse_spec(bad).unwrap_err().to_string();
            assert!(err.contains(expected), "{bad:?} -> {err:?}");
        }
    }

    #[test]
    fn parse_spec_multiple_params() {
        let (_, _, e) = parse_spec(
            "openrouter:deepseek/deepseek-v4-pro:reasoning=high,thinking=deepseek,foo=bar",
        )
        .unwrap();
        assert_eq!(e.get("reasoning").unwrap(), "high");
        assert_eq!(e.get("thinking").unwrap(), "deepseek");
        assert_eq!(e.get("foo").unwrap(), "bar");
    }

    #[test]
    fn parse_spec_roundtrips() {
        for spec in [
            "openai:gpt-4o-mini",
            "xai:grok-4",
            "openrouter:openai/gpt-4o",
            "openrouter:deepseek/deepseek-v4-pro:reasoning=high",
            "openrouter:deepseek/deepseek-v4-pro:reasoning=high,thinking=deepseek",
            "xai:grok-4:reasoning=low,foo=bar",
        ] {
            let (p, m, e) = parse_spec(spec).unwrap();
            let mut s = format!("{p}:{m}");
            if !e.is_empty() {
                let params: Vec<String> = e.iter().map(|(k, v)| format!("{k}={v}")).collect();
                s.push(':');
                s.push_str(&params.join(","));
            }
            assert_eq!(s, spec);
        }
    }

    #[test]
    fn default_allowlist_has_six_tools() {
        let block = default_native_block();
        let tools = block.get("allowed_tools").unwrap();
        assert_eq!(tools.len(), 6);
        for name in ["Bash", "Edit", "Read", "Write", "Grep", "Glob"] {
            assert!(tools.contains(&name.to_string()));
        }
    }

    #[test]
    fn equality_and_hash_track_provider_model_params() {
        let high = IndexMap::from([("reasoning".to_string(), "high".to_string())]);
        let low = IndexMap::from([("reasoning".to_string(), "low".to_string())]);
        let a = Client::new("openai".into(), "gpt-4".into(), None, None).unwrap();
        let b = Client::new("openai".into(), "gpt-4".into(), None, None).unwrap();
        let c = Client::new("openai".into(), "gpt-4o".into(), None, None).unwrap();
        let d = Client::new("openai".into(), "gpt-4".into(), None, Some(high.clone())).unwrap();
        let e = Client::new("openai".into(), "gpt-4".into(), None, Some(high)).unwrap();
        let f = Client::new("openai".into(), "gpt-4".into(), None, Some(low)).unwrap();

        assert!(a.__eq__(&b));
        assert_eq!(a.__hash__(), b.__hash__());
        assert!(!a.__eq__(&c));
        assert_ne!(a.__hash__(), c.__hash__());

        assert!(!a.__eq__(&d));
        assert!(d.__eq__(&e));
        assert_eq!(d.__hash__(), e.__hash__());
        assert!(!d.__eq__(&f));
    }

    #[test]
    fn unknown_provider_rejected() {
        assert!(Client::new("fake".into(), "m".into(), None, None).is_err());
        assert!(Client::parse("fake:m").is_err());
    }
}
