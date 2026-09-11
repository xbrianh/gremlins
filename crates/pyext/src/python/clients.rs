use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use gremlins::clients::backend::{Backend, ClientError, RunParams};
use gremlins::clients::cmd_backend::CmdBackend;
use gremlins::clients::openai_backend::{OpenAiBackend, OpenAiProvider};
use gremlins::clients::protocol::CompletedRun;
use rig_core::providers::openai;

/// Python-exposed RustClient.
#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub struct RustClient {
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
///
/// Events are JSON-encoded strings (one per event emitted by the backend)
/// rather than parsed dicts — downstream consumers should call
/// ``json.loads()`` on each element if they need structured access.
#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub struct PyCompletedRun {
    #[pyo3(get)]
    exit_code: i32,
    #[pyo3(get)]
    text_result: Option<String>,
    _events: Option<Vec<String>>,
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
        events: Option<Vec<String>>,
        cost_usd: Option<f64>,
        token_usage: Option<PyUsageStats>,
    ) -> Self {
        Self {
            exit_code,
            text_result,
            _events: events,
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
impl RustClient {
    #[new]
    #[pyo3(signature = (provider, model, native_block=None, extra_params=None))]
    fn new(
        py: Python<'_>,
        provider: String,
        model: String,
        native_block: Option<HashMap<String, Vec<String>>>,
        extra_params: Option<IndexMap<String, String>>,
    ) -> PyResult<Self> {
        let native_block = native_block.unwrap_or_else(default_native_block);
        let extra_params = extra_params.unwrap_or_default();

        let known = matches!(provider.as_str(), "openai" | "xai" | "openrouter" | "cmd");
        if known {
            return Ok(RustClient {
                provider,
                model,
                extra_params,
                native_block,
                inner: Arc::new(Mutex::new(None)),
            });
        }

        // Unknown provider: look up CLIENT_FACTORIES and extract the inner backend.
        let factories: Bound<'_, PyDict> = py
            .import("_gremlins_core.clients")?
            .getattr("CLIENT_FACTORIES")?
            .cast_into()?;
        let factory = factories.get_item(&provider)?;
        match factory {
            Some(f) => {
                let args: (&str, &IndexMap<String, String>) = (&model, &extra_params);
                let result = f.call1(args)?;
                let delegate: Py<Self> = result.extract()?;
                let borrowed = delegate.borrow(py);
                let backend = borrowed.inner.lock().unwrap().clone();
                Ok(RustClient {
                    provider,
                    model,
                    extra_params,
                    native_block,
                    inner: Arc::new(Mutex::new(backend)),
                })
            }
            None => Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown provider '{provider}'"
            ))),
        }
    }

    #[staticmethod]
    fn parse(py: Python<'_>, s: &str) -> PyResult<Self> {
        let (provider, model, extra_params) = parse_spec(s)?;
        let known = matches!(provider.as_str(), "openai" | "xai" | "openrouter" | "cmd");
        if !known {
            let factories: Bound<'_, PyDict> = py
                .import("_gremlins_core.clients")?
                .getattr("CLIENT_FACTORIES")?
                .cast_into()?;
            if factories.get_item(&provider)?.is_none() {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown provider '{provider}'"
                )));
            }
        }
        Ok(RustClient {
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
    #[pyo3(signature = (prompt, label, model=None, raw_path=None, capture_events=false, on_timeout_prompt=None, max_retries=3, cwd=None, artifact_dir=None, idle_timeout=None, extra_env=None, expected_artifact_paths=None, artifact_reminder_count=0))]
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
    ) -> PyResult<Bound<'py, PyAny>> {
        let backend = self.get_or_build_backend(py)?;
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

impl RustClient {
    fn get_or_build_backend(&self, py: Python<'_>) -> PyResult<Arc<dyn Backend>> {
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
                // Fall back to CLIENT_FACTORIES for custom providers registered by
                // tests or user code (e.g. "fake" in conftest.py).
                let factories: Bound<'_, PyDict> = py
                    .import("_gremlins_core.clients")?
                    .getattr("CLIENT_FACTORIES")?
                    .cast_into()?;
                let factory = factories.get_item(other)?;
                match factory {
                    Some(f) => {
                        let args: (&str, &IndexMap<String, String>) =
                            (&self.model, &self.extra_params);
                        let result = f.call1(args)?;
                        let delegate: Py<Self> = result.extract()?;
                        let borrowed = delegate.borrow(py);
                        let backend = borrowed.inner.lock().unwrap().clone();
                        match backend {
                            Some(backend) => {
                                *self.inner.lock().unwrap() = Some(backend.clone());
                                return Ok(backend);
                            }
                            None => {
                                return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                                    "factory for provider '{other}' returned without \
                                         building a backend"
                                )));
                            }
                        }
                    }
                    None => {
                        return Err(pyo3::exceptions::PyValueError::new_err(format!(
                            "unknown provider '{other}'"
                        )));
                    }
                }
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
        let events: Option<Vec<String>> = r.events.as_ref().map(|evts| {
            evts.iter()
                .map(|e| serde_json::to_string(e).unwrap_or_default())
                .collect()
        });
        let instance = PyCompletedRun::new(
            r.exit_code,
            r.text_result.clone(),
            events,
            r.cost_usd,
            usage,
        );
        Ok(Py::new(py, instance)?.into_any())
    }
}

pub fn init_clients_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<RustClient>()?;
    m.add_class::<PyCompletedRun>()?;
    m.add_class::<PyUsageStats>()?;

    // Set Python-visible docstrings — Rust /// doc comments don't propagate.
    m.getattr("PyUsageStats")?
        .setattr("__doc__", "Token usage summary for a model invocation.")?;
    m.getattr("PyCompletedRun")?.setattr(
        "__doc__",
        concat!(
            "Result of a single model run.\n\n",
            "Events are JSON-encoded strings (one per event). ",
            "Call `json.loads()` on each element to get structured dicts.",
        ),
    )?;

    let tools = vec!["Bash", "Edit", "Read", "Write", "Grep", "Glob"];
    let py_tools = PyList::new(m.py(), &tools)?;
    m.add("_DEFAULT_ALLOWED_TOOLS", py_tools)?;

    let factories = PyDict::new(m.py());
    m.add("CLIENT_FACTORIES", factories)?;

    Ok(())
}
