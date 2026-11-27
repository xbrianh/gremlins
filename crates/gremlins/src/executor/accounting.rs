//! Durable native request accounting. Start before backend execution, register each
//! invocation, then await `flush` after the backend returns (including errors).
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use gremlins_magic_bus::{CorrId, DispatchEvent, EventKind, ModelCallUsage};
use serde::{Deserialize, Serialize};

use super::state::{StateError, StateStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    #[serde(with = "corr_serde")]
    pub corr_id: CorrId,
    pub stage_path: String,
    pub usage: ModelCallUsage,
}

mod corr_serde {
    use super::*;
    pub(super) fn serialize<S: serde::Serializer>(
        corr: &CorrId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&corr.to_string())
    }
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<CorrId, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Only reported tokens contribute; completed requests count normalized agent turns.
pub(crate) fn counters(record: &UsageRecord) -> Result<BTreeMap<String, u64>, StateError> {
    let mut counters = BTreeMap::new();
    if record.usage.reported {
        let value = serde_json::to_value(&record.usage.usage)?;
        counters.extend(
            value
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, _)| key.as_str() != "turns")
                .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n))),
        );
    }
    counters.insert(
        "turns".into(),
        u64::from(record.usage.completed && record.usage.usage.turns > 0),
    );
    Ok(counters)
}

pub(crate) fn add(
    total: &mut BTreeMap<String, u64>,
    values: BTreeMap<String, u64>,
) -> Result<(), StateError> {
    for (key, value) in values {
        let entry = total.entry(key).or_default();
        *entry = entry
            .checked_add(value)
            .ok_or_else(|| StateError::Other("native usage counter overflow".into()))?;
    }
    Ok(())
}

type Registry = Mutex<HashMap<String, Weak<AccountingCollector>>>;
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// One lossless subscription per exact run. The task does not own this handle;
/// dropping the last handle unsubscribes. Keep it for the whole run if possible.
pub struct AccountingCollector {
    stages: Arc<Mutex<HashMap<String, String>>>,
    commands:
        tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<Result<(), StateError>>>,
    subscriber: gremlins_magic_bus::Subscriber<EventKind>,
}

impl AccountingCollector {
    pub fn start(
        run_id: &str,
        store: Arc<dyn StateStore + Send + Sync>,
    ) -> Result<Arc<Self>, StateError> {
        let mut registry = registry().lock().unwrap();
        registry.retain(|_, collector| collector.strong_count() > 0);
        if let Some(existing) = registry.get(run_id).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        store.validate_native_accounting()?;
        let (subscriber, mut receiver) = gremlins_magic_bus::subscribe_run(run_id);
        let (commands, mut requests) = tokio::sync::mpsc::unbounded_channel::<
            tokio::sync::oneshot::Sender<Result<(), StateError>>,
        >();
        let stages = Arc::new(Mutex::new(HashMap::new()));
        let handle = Arc::new(Self {
            stages: stages.clone(),
            commands,
            subscriber,
        });
        let run_id = run_id.to_owned();
        tokio::spawn(async move {
            let mut failure: Option<String> = None;
            recover(store.clone(), &mut failure).await;
            loop {
                tokio::select! {
                    request = requests.recv() => {
                        let Some(reply) = request else { break };
                        // Producers have returned before this barrier. Drain everything
                        // already queued, not just events selected before the command.
                        while let Ok(event) = receiver.try_recv() {
                            capture_blocking(&run_id, &stages, &store, event, &mut failure).await;
                        }
                        recover(store.clone(), &mut failure).await;
                        let result = failure.as_ref().map_or(Ok(()), |e| Err(StateError::Other(e.clone())));
                        let _ = reply.send(result);
                    }
                    event = receiver.recv() => {
                        match event {
                            gremlins_magic_bus::Recv::Event(event) => capture_blocking(&run_id, &stages, &store, event, &mut failure).await,
                            _ => { failure.get_or_insert_with(|| "native accounting subscription closed or lagged".into()); break; }
                        }
                    }
                }
            }
        });
        registry.insert(run_id_key(&handle.subscriber), Arc::downgrade(&handle));
        Ok(handle)
    }

    /// Register the exact invocation before it can publish any usage.
    pub fn register_execution(
        &self,
        stage_run_id: &str,
        stage_path: &str,
    ) -> Result<(), StateError> {
        if stage_run_id.is_empty() {
            return Err(StateError::Other("missing stage_run_id".into()));
        }
        let mut stages = self.stages.lock().unwrap();
        if stages
            .get(stage_run_id)
            .is_some_and(|old| old != stage_path)
        {
            return Err(StateError::Other(
                "stage_run_id registered with a different path".into(),
            ));
        }
        stages.insert(stage_run_id.into(), stage_path.into());
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), StateError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(tx)
            .map_err(|_| StateError::Other("accounting collector stopped".into()))?;
        rx.await
            .map_err(|_| StateError::Other("accounting barrier lost".into()))?
    }
}

fn run_id_key(subscriber: &gremlins_magic_bus::Subscriber<EventKind>) -> String {
    subscriber.prefix().run().unwrap_or_default().to_owned()
}

impl Drop for AccountingCollector {
    fn drop(&mut self) {
        gremlins_magic_bus::init_bus().unsubscribe(&self.subscriber);
    }
}

async fn recover(store: Arc<dyn StateStore + Send + Sync>, failure: &mut Option<String>) {
    let result = tokio::task::spawn_blocking(move || store.native_accounting(None)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            failure.get_or_insert_with(|| error.to_string());
        }
        Err(error) => {
            failure.get_or_insert_with(|| error.to_string());
        }
    }
}

async fn capture_blocking(
    run: &str,
    stages: &Arc<Mutex<HashMap<String, String>>>,
    store: &Arc<dyn StateStore + Send + Sync>,
    event: DispatchEvent<EventKind>,
    failure: &mut Option<String>,
) {
    if !matches!(event.kind(), EventKind::ModelCallUsage { .. }) {
        return;
    }
    let run = run.to_owned();
    let stages = stages.clone();
    let store = store.clone();
    match tokio::task::spawn_blocking(move || {
        let mut failure = None;
        capture(&run, &stages, &*store, &event, &mut failure);
        failure
    })
    .await
    {
        Ok(Some(error)) => {
            failure.get_or_insert(error);
        }
        Err(error) => {
            failure.get_or_insert_with(|| error.to_string());
        }
        Ok(None) => {}
    }
}

fn capture(
    run: &str,
    stages: &Mutex<HashMap<String, String>>,
    store: &dyn StateStore,
    event: &DispatchEvent<EventKind>,
    failure: &mut Option<String>,
) {
    if event.corr_id().run() != Some(run) {
        return;
    }
    let EventKind::ModelCallUsage { usage } = event.kind() else {
        return;
    };
    let result = (|| {
        let path = {
            let stages = stages.lock().unwrap();
            event
                .corr_id()
                .stage_run_id()
                .and_then(|id| stages.get(id))
                .cloned()
                .ok_or_else(|| StateError::Other("usage for unregistered stage_run_id".into()))?
        };
        store.native_accounting(Some(&UsageRecord {
            corr_id: event.corr_id().clone(),
            stage_path: path,
            usage: usage.clone(),
        }))
    })();
    if let Err(error) = result {
        failure.get_or_insert_with(|| error.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::FileSystemStateStore;
    use super::*;
    use gremlins_magic_bus::emit_corr_event;
    use gremlins_magic_bus::stats::UsageStats;
    use serde_json::{json, Value};
    use std::path::Path;

    fn seed(initial: Value) -> (tempfile::TempDir, FileSystemStateStore) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), initial.to_string()).unwrap();
        let store = FileSystemStateStore::from_path(dir.path().to_path_buf());
        (dir, store)
    }

    fn read_json(dir: &Path, name: &str) -> Value {
        serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap()
    }

    fn rows(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("token_usage.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn record(run: &str, execution: &str, request: &str, tokens: u64) -> UsageRecord {
        UsageRecord {
            corr_id: CorrId::new()
                .with_run(run)
                .with_stage("agent")
                .with_stage_run_id(execution)
                .with_turn(1),
            stage_path: "pipeline/agent".into(),
            usage: ModelCallUsage {
                request_id: request.into(),
                provider: "test".into(),
                model: "test".into(),
                agent_id: "agent".into(),
                request_kind: String::new(),
                reported: true,
                completed: true,
                raw: json!({}),
                usage: UsageStats {
                    prompt_tokens: Some(tokens),
                    completion_tokens: None,
                    cached_input_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                    turns: 1,
                },
            },
        }
    }

    #[derive(Debug)]
    struct LegacyStore;

    macro_rules! unused_store_methods {
        ($(fn $name:ident(&self $(, $arg:ident: $ty:ty)*) $(-> $result:ty)?;)*) => {
            $(fn $name(&self $(, $arg: $ty)*) $(-> $result)? {
                $(let _ = $arg;)*
                panic!("legacy recovery must not call {}", stringify!($name));
            })*
        };
    }

    #[async_trait::async_trait]
    impl StateStore for LegacyStore {
        unused_store_methods! {
            fn state_tree(&self) -> serde_json::Map<String, Value>;
            fn write_state(&self, data: &serde_json::Map<String, Value>) -> Result<(), super::super::state::StateError>;
            fn open(&self, name: &str, mode: super::super::state::BlobMode) -> Result<Box<dyn super::super::state::StateBlob>, super::super::state::StateError>;
            fn exists(&self, name: &str) -> bool;
            fn clear_stage_error(&self, attempt: &str);
            fn read_str(&self, field: &str) -> String;
            fn read_field(&self, field: &str) -> Option<Value>;
            fn get_field(&self, field: &str) -> Option<Value>;
            fn stage_error(&self) -> Option<serde_json::Map<String, Value>>;
            fn patch(&self, delete: &[String], fields: &serde_json::Map<String, Value>);
            fn record_stage_error(&self, class: &str, detail: &str);
            fn accumulate_token_usage(&self, usage: &HashMap<String, i64>);
            fn write_terminal_state(&self, exit_code: i32);
            fn add_subprocess_cost(&self, amount: f64);
            fn patch_parallel_attempt(&self, child_key: &str, attempt: &str);
            fn as_any(&self) -> Option<&dyn std::any::Any>;
        }

        async fn data_uri(&self, _: &str) -> Result<String, super::super::state::MissingArtifact> {
            unreachable!()
        }
        async fn content(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<String, Box<dyn std::error::Error>> {
            unreachable!()
        }
        async fn is_registered(&self, _: &str) -> bool {
            unreachable!()
        }
        async fn path_for_uri(
            &self,
            _: &crate::artifacts::uri::Uri,
        ) -> Result<String, Box<dyn std::error::Error>> {
            unreachable!()
        }
        async fn commit(&self, _: &str, _: &str) -> Result<(), Box<dyn std::error::Error>> {
            unreachable!()
        }
        async fn write_into_registry(
            &self,
            _: &crate::artifacts::uri::Uri,
            _: &str,
        ) -> Result<String, Box<dyn std::error::Error>> {
            unreachable!()
        }
        async fn keys(&self) -> Vec<String> {
            unreachable!()
        }
        async fn fork(
            &self,
            _: &str,
        ) -> Result<Box<dyn StateStore + Send + Sync>, Box<dyn std::error::Error>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn default_store_recovery_is_noop_but_native_writes_are_unsupported() {
        let store = Arc::new(LegacyStore);
        store.native_accounting(None).unwrap();
        assert!(store.validate_native_accounting().is_err());
        assert!(store
            .native_accounting(Some(&record("legacy", "execution", "request", 3)))
            .is_err());
        let mut failure = None;
        recover(store, &mut failure).await;
        assert!(failure.is_none());
    }

    #[tokio::test]
    async fn recovery_without_native_journal_preserves_legacy_files() {
        let initial = json!({"cmd": "legacy", "token_usage": {"prompt_tokens": 7}});
        let (dir, store) = seed(initial.clone());
        let legacy = "{\"stage\":\"cmd\",\"prompt_tokens\":7}\n";
        std::fs::write(dir.path().join("token_usage.jsonl"), legacy).unwrap();
        let collector = AccountingCollector::start("no-native-journal", Arc::new(store)).unwrap();
        collector.flush().await.unwrap();
        assert_eq!(read_json(dir.path(), "state.json"), initial);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("token_usage.jsonl")).unwrap(),
            legacy
        );
        assert!(!dir.path().join("native_accounting.json").exists());
        assert!(!dir.path().join("native_accounting.jsonl").exists());
    }

    #[tokio::test]
    async fn scoped_stage_append_is_preserved_by_native_recovery() {
        use super::super::state::BlobMode;
        use std::io::{Read, Write};
        let (_dir, store) = seed(json!({}));
        let checkout = store.checkout_registry(&[]).await.unwrap();
        let scoped = checkout.store;
        scoped
            .open("state.json", BlobMode::Write)
            .unwrap()
            .write_all(b"{}")
            .unwrap();
        scoped
            .native_accounting(Some(&record("scoped", "execution", "first", 3)))
            .unwrap();
        let command = HashMap::from([("prompt_tokens".into(), 6)]);
        scoped.accumulate_token_usage(&command);
        scoped
            .append_stage_usage("{\"stage\":\"cmd\",\"prompt_tokens\":6}")
            .unwrap();
        scoped
            .native_accounting(Some(&record("scoped", "execution", "next", 2)))
            .unwrap();
        scoped.native_accounting(None).unwrap();
        assert_eq!(
            scoped.read_field("token_usage"),
            Some(json!({"prompt_tokens": 11, "turns": 2}))
        );
        let mut projected = String::new();
        scoped
            .open("token_usage.jsonl", BlobMode::Read)
            .unwrap()
            .read_to_string(&mut projected)
            .unwrap();
        let projected: Vec<Value> = projected
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(projected.len(), 2);
        assert!(projected
            .iter()
            .any(|row| row["stage"] == "cmd" && row["prompt_tokens"] == 6));
        assert!(projected
            .iter()
            .any(|row| row["stage_run_id"] == "execution" && row["prompt_tokens"] == 5));
    }

    fn wal_line(entry: &UsageRecord) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(entry).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn recovery_replays_complete_wal_ahead_of_both_projections() {
        for partial_tail in [false, true] {
            let (dir, store) = seed(json!({"token_usage": {"prompt_tokens": 40}, "cmd": "keep"}));
            let legacy = "{\"stage\":\"cmd\",\"prompt_tokens\":40}\n";
            std::fs::write(dir.path().join("token_usage.jsonl"), legacy).unwrap();
            std::fs::write(
                dir.path().join("native_accounting.json"),
                json!({
                    "baseline": {"prompt_tokens": 40}, "legacy_rows": legacy, "records": []
                })
                .to_string(),
            )
            .unwrap();
            let pending = record("wal-recovery", "execution", "pending", 9);
            let complete = wal_line(&pending);
            let mut interrupted = complete.clone();
            if partial_tail {
                interrupted.extend_from_slice(b"{\"usage\":");
            }
            std::fs::write(dir.path().join("native_accounting.jsonl"), interrupted).unwrap();
            store.native_accounting(None).unwrap();
            assert_eq!(
                read_json(dir.path(), "state.json")["token_usage"],
                json!({"prompt_tokens": 49, "turns": 1})
            );
            assert_eq!(read_json(dir.path(), "state.json")["cmd"], "keep");
            assert_eq!(
                std::fs::read(dir.path().join("native_accounting.jsonl")).unwrap(),
                complete
            );
            assert_eq!(rows(dir.path()).len(), 2);
            assert_eq!(rows(dir.path())[1]["prompt_tokens"], 9);
            store.native_accounting(Some(&pending)).unwrap();
            store
                .native_accounting(Some(&record("wal-recovery", "execution", "next", 3)))
                .unwrap();
            store.native_accounting(None).unwrap();
            assert_eq!(
                read_json(dir.path(), "state.json")["token_usage"],
                json!({"prompt_tokens": 52, "turns": 2})
            );
            assert_eq!(rows(dir.path()).len(), 2);
            assert_eq!(
                read_json(dir.path(), "native_accounting.json")["records"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
    }

    #[test]
    fn recovery_discards_entire_partial_wal_tail_before_next_append() {
        let (dir, store) = seed(json!({}));
        std::fs::write(
            dir.path().join("native_accounting.json"),
            json!({
                "baseline": {}, "legacy_rows": "", "records": []
            })
            .to_string(),
        )
        .unwrap();
        let pending = record("partial", "execution", "pending", 9);
        let mut partial = wal_line(&pending);
        partial.pop();
        std::fs::write(dir.path().join("native_accounting.jsonl"), partial).unwrap();
        store.native_accounting(None).unwrap();
        assert_eq!(
            std::fs::metadata(dir.path().join("native_accounting.jsonl"))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            read_json(dir.path(), "state.json")["token_usage"],
            json!({})
        );
        store.native_accounting(Some(&pending)).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("native_accounting.jsonl")).unwrap(),
            wal_line(&pending)
        );
        assert_eq!(rows(dir.path())[0]["prompt_tokens"], 9);
    }

    #[test]
    fn malformed_complete_wal_line_does_not_rewrite_projections() {
        let (dir, store) = seed(json!({"cmd": "keep"}));
        store
            .native_accounting(Some(&record("malformed", "execution", "first", 3)))
            .unwrap();
        let files = ["state.json", "native_accounting.json", "token_usage.jsonl"];
        let before = files.map(|name| std::fs::read(dir.path().join(name)).unwrap());
        let mut wal = std::fs::read(dir.path().join("native_accounting.jsonl")).unwrap();
        wal.extend_from_slice(b"not-json\n");
        std::fs::write(dir.path().join("native_accounting.jsonl"), &wal).unwrap();
        assert!(store.native_accounting(None).is_err());
        for (name, bytes) in files.iter().zip(before) {
            assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), bytes);
        }
        assert_eq!(
            std::fs::read(dir.path().join("native_accounting.jsonl")).unwrap(),
            wal
        );
    }

    #[test]
    fn same_length_corrupt_wal_edit_invalidates_warm_cache() {
        let (dir, store) = seed(json!({}));
        store
            .native_accounting(Some(&record("warm-wal", "execution", "first", 3)))
            .unwrap();
        store.native_accounting(None).unwrap();
        let path = dir.path().join("native_accounting.jsonl");
        let mut wal = std::fs::read(&path).unwrap();
        let len = wal.len();
        wal[0] = b'!';
        std::fs::write(&path, &wal).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len as u64);
        assert!(store.native_accounting(None).is_err());
        assert_eq!(std::fs::read(path).unwrap(), wal);
    }

    #[test]
    fn corrupt_checkpoint_invalidates_warm_cache() {
        let (dir, store) = seed(json!({}));
        store
            .native_accounting(Some(&record("warm-checkpoint", "execution", "first", 3)))
            .unwrap();
        store.native_accounting(None).unwrap();
        let path = dir.path().join("native_accounting.json");
        let mut checkpoint = std::fs::read(&path).unwrap();
        checkpoint[0] = b'!';
        std::fs::write(&path, &checkpoint).unwrap();
        assert!(store.native_accounting(None).is_err());
        assert_eq!(std::fs::read(path).unwrap(), checkpoint);
    }

    #[test]
    fn outer_usage_keeps_failed_attempts_and_excludes_tasks_and_other_runs() {
        let (_dir, store) = seed(json!({}));
        let mut failed = record("session_chat", "outer", "failed", 3);
        failed.usage.agent_id.clear();
        failed.usage.completed = false;
        failed.usage.usage.turns = 0;
        let mut completed = record("session_chat", "outer", "completed", 7);
        completed.usage.agent_id.clear();
        let task = record("session_chat", "task", "task", 100);
        let mut foreign = record("other", "outer", "foreign", 100);
        foreign.usage.agent_id.clear();
        for record in [failed, completed, task, foreign] {
            store.native_accounting(Some(&record)).unwrap();
        }
        let usage = store.outer_conversational_usage("session_chat").unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.turns, 1);
    }

    #[test]
    fn durable_dedup_survives_store_reopen() {
        let (dir, store) = seed(json!({"token_usage": {"prompt_tokens": 7}}));
        let first = record("dedup", "execution", "request-1", 11);
        store.native_accounting(Some(&first)).unwrap();
        let before = std::fs::read(dir.path().join("token_usage.jsonl")).unwrap();
        drop(store);
        let reopened = FileSystemStateStore::from_path(dir.path().to_path_buf());
        reopened.native_accounting(Some(&first)).unwrap();
        reopened.native_accounting(None).unwrap();
        assert_eq!(
            read_json(dir.path(), "state.json")["token_usage"]["prompt_tokens"],
            18
        );
        assert_eq!(
            std::fs::read(dir.path().join("token_usage.jsonl")).unwrap(),
            before
        );
        assert_eq!(
            read_json(dir.path(), "native_accounting.json")["records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // Another request in the same model turn is not a duplicate.
        let second = record("dedup", "execution", "request-2", 3);
        reopened.native_accounting(Some(&second)).unwrap();
        assert_eq!(
            read_json(dir.path(), "state.json")["token_usage"]["prompt_tokens"],
            21
        );
        assert_eq!(rows(dir.path()).len(), 1);
    }

    #[test]
    fn request_id_conflicts_across_envelopes_without_charging_again() {
        let (dir, store) = seed(json!({}));
        let first = record("run", "execution", "unique-request", 11);
        store.native_accounting(Some(&first)).unwrap();
        drop(store);
        let store = FileSystemStateStore::from_path(dir.path().to_path_buf());
        let files = [
            "state.json",
            "native_accounting.json",
            "token_usage.jsonl",
            "native_accounting.jsonl",
        ];
        let before = files.map(|name| std::fs::read(dir.path().join(name)).unwrap());
        let mut changed_path = first.clone();
        changed_path.stage_path = "other/agent".into();
        for conflict in [
            record("other-run", "execution", "unique-request", 11),
            record("run", "other-execution", "unique-request", 11),
            record("run", "execution", "unique-request", 12),
            changed_path,
        ] {
            let error = store.native_accounting(Some(&conflict)).unwrap_err();
            assert!(error.to_string().contains("conflicting duplicate"));
            for (name, bytes) in files.iter().zip(&before) {
                assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), *bytes);
            }
        }
        store.native_accounting(Some(&first)).unwrap();
        assert_eq!(
            read_json(dir.path(), "state.json")["token_usage"]["prompt_tokens"],
            11
        );
    }

    #[test]
    fn retries_count_reported_partial_tokens_and_only_completed_turns() {
        let (dir, store) = seed(json!({}));
        let mut failed = record("retry", "execution", "failed", 3);
        failed.usage.completed = false;
        failed.usage.usage.turns = 99;
        let succeeded = record("retry", "execution", "succeeded", 7);
        let mut unreported = record("retry", "execution", "unreported-failed", 100);
        unreported.usage.completed = false;
        unreported.usage.reported = false;
        for entry in [&failed, &succeeded, &unreported] {
            store.native_accounting(Some(entry)).unwrap();
            store.native_accounting(Some(entry)).unwrap();
        }
        store.native_accounting(None).unwrap();
        let expected = json!({"prompt_tokens": 10, "turns": 1});
        assert_eq!(read_json(dir.path(), "state.json")["token_usage"], expected);
        let rows = rows(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["prompt_tokens"], 10);
        assert_eq!(rows[0]["turns"], 1);
        assert_eq!(
            read_json(dir.path(), "native_accounting.json")["records"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn completed_unreported_request_counts_turn_without_inventing_tokens() {
        let (dir, store) = seed(json!({}));
        let mut entry = record("unreported", "execution", "request", 100);
        entry.usage.reported = false;
        entry.usage.usage.turns = 42;
        store.native_accounting(Some(&entry)).unwrap();
        drop(store);
        let store = FileSystemStateStore::from_path(dir.path().to_path_buf());
        store.native_accounting(Some(&entry)).unwrap();
        assert_eq!(
            read_json(dir.path(), "state.json")["token_usage"],
            json!({"turns": 1})
        );
        let rows = rows(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["turns"], 1);
        assert!(rows[0].get("prompt_tokens").is_none());
    }

    #[test]
    fn recovery_replays_journal_before_summary_over_legacy_baseline() {
        let (dir, store) = seed(json!({
            "token_usage": {"prompt_tokens": 40, "completion_tokens": 2},
            "cmd": "keep-command", "subprocess_cost_usd": 1.25
        }));
        let legacy = "{\"stage\":\"legacy\",\"prompt_tokens\":40,\"completion_tokens\":2}\n";
        std::fs::write(dir.path().join("token_usage.jsonl"), legacy).unwrap();
        let pending = record("recovery", "execution", "pending", 9);
        // Simulate a crash after journal commit but before either derived file.
        std::fs::write(
            dir.path().join("native_accounting.json"),
            json!({
                "baseline": {"prompt_tokens": 40, "completion_tokens": 2},
                "legacy_rows": legacy, "records": [pending.clone()]
            })
            .to_string(),
        )
        .unwrap();
        store.native_accounting(None).unwrap();
        let summary = read_json(dir.path(), "state.json");
        assert_eq!(summary["token_usage"]["prompt_tokens"], 49);
        assert_eq!(summary["token_usage"]["completion_tokens"], 2);
        assert_eq!(summary["cmd"], "keep-command");
        assert_eq!(summary["subprocess_cost_usd"], 1.25);
        let materialized = std::fs::read_to_string(dir.path().join("token_usage.jsonl")).unwrap();
        assert!(materialized.starts_with(legacy));
        assert_eq!(rows(dir.path()).len(), 2);
        store.native_accounting(Some(&pending)).unwrap();
        store.native_accounting(None).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("token_usage.jsonl")).unwrap(),
            materialized
        );
        assert_eq!(read_json(dir.path(), "state.json"), summary);
    }

    #[test]
    fn interrupted_projection_duplicate_then_next_request_fresh_and_legacy() {
        for baseline in [0, 40] {
            let (dir, store) = seed(json!({"token_usage": {"prompt_tokens": baseline}}));
            let legacy = if baseline == 0 {
                String::new()
            } else {
                format!("{{\"stage\":\"legacy\",\"prompt_tokens\":{baseline}}}\n")
            };
            std::fs::write(dir.path().join("token_usage.jsonl"), &legacy).unwrap();
            let first = record("interrupted", "execution", "first", 9);
            std::fs::write(
                dir.path().join("native_accounting.json"),
                json!({
                    "baseline": {"prompt_tokens": baseline},
                    "legacy_rows": legacy, "records": [first.clone()]
                })
                .to_string(),
            )
            .unwrap();
            // Replay the duplicate directly, with neither projection committed.
            store.native_accounting(Some(&first)).unwrap();
            store
                .native_accounting(Some(&record("interrupted", "execution", "next", 3)))
                .unwrap();
            store.native_accounting(None).unwrap();
            let summary = read_json(dir.path(), "state.json");
            assert_eq!(
                summary["token_usage"],
                json!({"prompt_tokens": baseline + 12, "turns": 2})
            );
            let projected = rows(dir.path());
            assert_eq!(projected.len(), if baseline == 0 { 1 } else { 2 });
            let native = projected
                .iter()
                .find(|row| row["stage_run_id"] == "execution")
                .unwrap();
            assert_eq!(native["run_id"], "interrupted");
            assert_eq!(native["prompt_tokens"], 12);
            assert_eq!(native["turns"], 2);
        }
    }

    #[test]
    fn imported_native_rows_survive_local_projection() {
        let (dir, store) = seed(json!({"token_usage": {"prompt_tokens": 17}}));
        let imported = concat!(
            "{\"run_id\":\"foreign\",\"stage_run_id\":\"execution\",\"stage\":\"pipeline.agent\",\"prompt_tokens\":10}\n",
            "{\"run_id\":\"local\",\"stage_run_id\":\"imported\",\"stage\":\"pipeline.agent\",\"prompt_tokens\":7}\n"
        );
        std::fs::write(dir.path().join("token_usage.jsonl"), imported).unwrap();
        let first = record("local", "execution", "first", 3);
        store.native_accounting(Some(&first)).unwrap();
        store.native_accounting(Some(&first)).unwrap();
        store
            .native_accounting(Some(&record("local", "execution", "next", 2)))
            .unwrap();
        store.native_accounting(None).unwrap();
        assert!(
            std::fs::read_to_string(dir.path().join("token_usage.jsonl"))
                .unwrap()
                .starts_with(imported)
        );
        let projected = rows(dir.path());
        assert_eq!(projected.len(), 3);
        assert_eq!(projected[2]["prompt_tokens"], 5);
        assert_eq!(
            read_json(dir.path(), "state.json")["token_usage"]["prompt_tokens"],
            22
        );
    }

    #[test]
    fn first_native_write_captures_legacy_cmd_usage_without_rewriting_rows() {
        let (dir, store) = seed(json!({
            "token_usage": {"prompt_tokens": 12, "completion_tokens": 4}
        }));
        let legacy = "{\"stage\":\"pipeline.cmd\",\"prompt_tokens\":12,\"completion_tokens\":4}\n";
        std::fs::write(dir.path().join("token_usage.jsonl"), legacy).unwrap();
        store
            .native_accounting(Some(&record("legacy", "execution", "request", 8)))
            .unwrap();
        let journal = read_json(dir.path(), "native_accounting.json");
        assert_eq!(
            journal["baseline"],
            json!({"prompt_tokens": 12, "completion_tokens": 4})
        );
        assert_eq!(journal["legacy_rows"], legacy);
        let summary = read_json(dir.path(), "state.json");
        assert_eq!(summary["token_usage"]["prompt_tokens"], 20);
        assert_eq!(summary["token_usage"]["completion_tokens"], 4);
        assert!(
            std::fs::read_to_string(dir.path().join("token_usage.jsonl"))
                .unwrap()
                .starts_with(legacy)
        );
        assert_eq!(rows(dir.path()).len(), 2);
    }

    #[test]
    fn command_usage_between_native_executions_is_preserved() {
        use super::super::state::emit_stage_token_usage;
        let (dir, store) = seed(json!({"token_usage": {"prompt_tokens": 4}}));
        store
            .native_accounting(Some(&record("mixed", "first", "a", 3)))
            .unwrap();
        let command = HashMap::from([("prompt_tokens".into(), 6)]);
        store.accumulate_token_usage(&command);
        emit_stage_token_usage(&store, "pipeline/cmd", &command);
        store
            .native_accounting(Some(&record("mixed", "second", "b", 2)))
            .unwrap();
        store.native_accounting(None).unwrap();
        assert_eq!(
            read_json(dir.path(), "state.json")["token_usage"]["prompt_tokens"],
            15
        );
        let rows = rows(dir.path());
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .find(|row| row["stage"] == "pipeline.cmd")
                .unwrap()["prompt_tokens"],
            6
        );
    }

    #[tokio::test]
    async fn repeated_same_path_has_distinct_invocation_sums() {
        let (dir, store) = seed(json!({}));
        let collector = AccountingCollector::start("repeated-path", Arc::new(store)).unwrap();
        for execution in ["first", "second"] {
            collector
                .register_execution(execution, "pipeline/agent")
                .unwrap();
        }
        for (execution, request, tokens) in
            [("first", "a", 2), ("second", "b", 7), ("first", "c", 3)]
        {
            let event = record("repeated-path", execution, request, tokens);
            emit_corr_event(
                &event.corr_id,
                EventKind::ModelCallUsage { usage: event.usage },
            );
        }
        collector.flush().await.unwrap();
        let projected = rows(dir.path());
        assert_eq!(projected.len(), 2);
        for (execution, tokens, turns) in [("first", 5, 2), ("second", 7, 1)] {
            let row = projected
                .iter()
                .find(|row| row["stage_run_id"] == execution)
                .unwrap();
            assert_eq!(row["stage"], "pipeline.agent");
            assert_eq!(row["run_id"], "repeated-path");
            assert_eq!(row["prompt_tokens"], tokens);
            assert_eq!(row["turns"], turns);
        }
        assert_eq!(
            read_json(dir.path(), "state.json")["token_usage"],
            json!({"prompt_tokens": 12, "turns": 3})
        );
    }

    #[tokio::test]
    async fn accounting_write_failure_is_visible_and_sticky_at_flush() {
        let (dir, store) = seed(json!({}));
        let collector =
            AccountingCollector::start("accounting-write-failure", Arc::new(store)).unwrap();
        collector
            .register_execution("execution", "pipeline/agent")
            .unwrap();
        let journal = dir.path().join("native_accounting.json");
        if journal.exists() {
            std::fs::remove_file(&journal).unwrap();
        }
        std::fs::create_dir(&journal).unwrap();
        let event = record("accounting-write-failure", "execution", "request", 5);
        emit_corr_event(
            &event.corr_id,
            EventKind::ModelCallUsage { usage: event.usage },
        );
        assert!(collector.flush().await.is_err());
        std::fs::remove_dir(&journal).unwrap();
        assert!(collector.flush().await.is_err());
    }

    #[tokio::test]
    async fn corrupt_journal_failure_is_visible_at_barrier() {
        let (dir, store) = seed(json!({"cmd": "unchanged"}));
        std::fs::write(dir.path().join("native_accounting.json"), b"not-json").unwrap();
        let collector =
            AccountingCollector::start("accounting-corrupt-start", Arc::new(store)).unwrap();
        assert!(collector.flush().await.is_err());
        assert_eq!(
            read_json(dir.path(), "state.json"),
            json!({"cmd": "unchanged"})
        );
    }

    #[tokio::test]
    async fn interleaved_executions_keep_paths_and_preserve_cmd_state() {
        let initial = json!({"cmd": ["sh", "-c", "echo hello"], "stage": "cmd", "attempt": 3});
        let (dir, store) = seed(initial.clone());
        let store = Arc::new(store);
        let collector =
            AccountingCollector::start("accounting-interleaving", store.clone()).unwrap();
        collector
            .register_execution("left", "parallel/left/agent")
            .unwrap();
        collector
            .register_execution("right", "parallel/right/agent")
            .unwrap();
        assert!(collector.register_execution("left", "wrong/path").is_err());
        for (execution, request, tokens) in [
            ("left", "left-request", 2),
            ("right", "right-request", 3),
            ("left", "next-request", 5),
        ] {
            let event = record("accounting-interleaving", execution, request, tokens);
            emit_corr_event(
                &event.corr_id,
                EventKind::ModelCallUsage { usage: event.usage },
            );
        }
        // A child run is not part of the exact run's accounting.
        let (prefix_subscriber, mut prefix_receiver) =
            gremlins_magic_bus::subscribe_run("accounting-interleaving");
        let foreign = record("accounting-interleaving:child", "left", "foreign", 100);
        emit_corr_event(
            &foreign.corr_id,
            EventKind::ModelCallUsage {
                usage: foreign.usage,
            },
        );
        // The bus delivers colon descendants; accounting must reject exact-ID mismatch.
        assert_eq!(
            prefix_receiver.try_recv().unwrap().corr_id().run(),
            Some("accounting-interleaving:child")
        );
        gremlins_magic_bus::init_bus().unsubscribe(&prefix_subscriber);
        collector.flush().await.unwrap();
        let summary = read_json(dir.path(), "state.json");
        assert_eq!(summary["token_usage"]["prompt_tokens"], 10);
        for key in ["cmd", "stage", "attempt"] {
            assert_eq!(summary[key], initial[key]);
        }
        let journal = read_json(dir.path(), "native_accounting.json");
        let records = journal["records"].as_array().unwrap();
        assert_eq!(records.len(), 3);
        for entry in records {
            let entry: UsageRecord = serde_json::from_value(entry.clone()).unwrap();
            let expected = if entry.corr_id.stage_run_id() == Some("left") {
                "parallel/left/agent"
            } else {
                "parallel/right/agent"
            };
            assert_eq!(entry.stage_path, expected);
        }
        assert_eq!(rows(dir.path()).len(), 2);
    }
}
