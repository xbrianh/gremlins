//! state.json I/O, flock-guarded updates, and StateData.
//!
//! Every `state.json` mutation goes through [`write_state`] or [`locked_update`],
//! both of which hold the flock. Reads ([`read_str`], [`read_field`], [`get_field`],
//! [`stage_error`]) are lock-free snapshot reads, safe because
//! every mutation is rename-atomic.

use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// A general handle for named state-directory files.
#[allow(dead_code)]
pub(crate) trait StateBlob: std::io::Read + std::io::Write + std::io::Seek + Send {}
impl<T: std::io::Read + std::io::Write + std::io::Seek + Send> StateBlob for T {}

/// How to open a named blob.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobMode {
    /// Open an existing file for reading and writing. Fails if absent.
    ReadWrite,
    /// Create or truncate for writing.
    Write,
    /// Create or append.
    Append,
}

// ---------------------------------------------------------------------------
// StateStore trait — the storage backend seam
// ---------------------------------------------------------------------------

pub(crate) trait StateStore: Send + Sync + Debug {
    /// Create a store for the given gremlin identity.
    fn new(gremlin_id: Option<String>) -> Self
    where
        Self: Sized;

    /// Lock-free snapshot of the full state tree.
    fn state_tree(&self) -> Map<String, Value>;

    /// Write `data` as `state.json` into the store's directory.
    fn seed(&mut self, data: &Map<String, Value>) -> Result<(), StateError>;

    /// Open a named blob in the state directory. Creates parent directories
    /// as needed. The returned handle supports Read + Write + Seek.
    #[allow(dead_code)]
    fn open(&self, name: &str, mode: BlobMode) -> Result<Box<dyn StateBlob>, StateError>;

    /// Check whether a named blob exists without creating it.
    #[allow(dead_code)]
    fn exists(&self, name: &str) -> bool;

    /// Remove a stale bail file for the given attempt.
    fn clear_stage_error(&self, attempt: &str);

    // --- reads ---

    /// Lock-free snapshot read. Falsy values read as `""`.
    fn read_str(&self, field: &str) -> String;

    /// Present, non-null value — `None` when absent or null.
    fn read_field(&self, field: &str) -> Option<Value>;

    /// Value with fallback to its default.
    fn get_field(&self, field: &str) -> Option<Value>;

    /// The bail record for the current attempt, if any.
    fn stage_error(&self) -> Option<Map<String, Value>>;

    /// Read `parallel_worktrees[group_name]` as `(base_head, {child_key: path})`.
    fn parallel_worktrees(&self, group_name: &str) -> (String, HashMap<String, String>);

    // --- writes ---

    fn patch(&self, delete: &[String], fields: &Map<String, Value>);

    /// Write a bail record with first-writer-wins semantics.
    fn record_stage_error(&self, class: &str, detail: &str);

    fn accumulate_token_usage(&self, usage: &HashMap<String, i64>);

    /// Create the `finished` marker and patch terminal fields.
    fn write_terminal_state(&self, exit_code: i32);

    /// Write `data` to `state_dir/state.json` and update the store's path.
    fn persist(&mut self, state_dir: &Path, data: &Map<String, Value>) -> Result<(), StateError>;

    fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<&str>,
        paths: Option<&HashMap<String, String>>,
    );

    fn add_subprocess_cost(&self, amount: f64);

    fn patch_parallel_attempt(&self, child_key: &str, attempt: &str);
}

// ---------------------------------------------------------------------------
// FileStateStore — filesystem-backed implementation
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct FileStateStore {
    state_file: Option<PathBuf>,
}

impl StateStore for FileStateStore {
    fn new(gremlin_id: Option<String>) -> Self {
        Self {
            state_file: resolve_state_file(gremlin_id.as_deref()),
        }
    }

    fn state_tree(&self) -> Map<String, Value> {
        read_state_json(self.state_file.as_deref())
    }

    fn seed(&mut self, data: &Map<String, Value>) -> Result<(), StateError> {
        let state_dir = self
            .state_file
            .as_ref()
            .and_then(|sf| sf.parent().map(|p| p.to_path_buf()))
            .ok_or_else(|| StateError::Other("no state directory".into()))?;
        write_state(&state_dir, data)?;
        self.state_file = Some(state_dir.join("state.json"));
        Ok(())
    }

    fn open(&self, name: &str, mode: BlobMode) -> Result<Box<dyn StateBlob>, StateError> {
        // Reject traversal and absolute paths.
        if name.contains("..") || name.starts_with('/') || name.contains('\0') {
            return Err(StateError::Other(format!(
                "invalid blob name {name:?}: traversal or absolute path rejected"
            )));
        }
        let state_dir = self
            .state_file
            .as_ref()
            .and_then(|sf| sf.parent())
            .ok_or_else(|| StateError::Other("no state directory".into()))?;
        let path = state_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut opts = OpenOptions::new();
        match mode {
            BlobMode::ReadWrite => {
                opts.read(true).write(true).create(false);
            }
            BlobMode::Write => {
                opts.write(true).create(true).truncate(true);
            }
            BlobMode::Append => {
                opts.append(true).create(true);
            }
        }
        let file = opts.open(&path)?;
        Ok(Box::new(file))
    }

    fn exists(&self, name: &str) -> bool {
        let Some(state_dir) = self.state_file.as_ref().and_then(|sf| sf.parent()) else {
            return false;
        };
        // Reject traversal.
        if name.contains("..") || name.starts_with('/') || name.contains('\0') {
            return false;
        }
        state_dir.join(name).exists()
    }

    fn clear_stage_error(&self, attempt: &str) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        let Some(parent) = sf.parent() else {
            return;
        };
        let bail_path = parent.join(format!("bail_{attempt}.json"));
        let _ = std::fs::remove_file(&bail_path);
    }

    fn get_field(&self, name: &str) -> Option<Value> {
        let default = default_for(name)?;
        let data = read_state_json(self.state_file.as_deref());
        Some(data.get(name).cloned().unwrap_or(default))
    }

    fn read_field(&self, field: &str) -> Option<Value> {
        let sf = self.state_file.as_ref()?;
        if !sf.exists() {
            return None;
        }
        let data = read_state_json(Some(sf));
        match data.get(field) {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.clone()),
        }
    }

    fn read_str(&self, field: &str) -> String {
        let Some(sf) = self.state_file.as_ref() else {
            return String::new();
        };
        if !sf.exists() {
            return String::new();
        }
        match read_state_json(Some(sf)).get(field) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) if n.as_f64() != Some(0.0) => n.to_string(),
            Some(Value::Bool(true)) => "True".into(),
            _ => String::new(),
        }
    }

    fn persist(&mut self, state_dir: &Path, data: &Map<String, Value>) -> Result<(), StateError> {
        write_state(state_dir, data)?;
        self.state_file = Some(state_dir.join("state.json"));
        Ok(())
    }

    fn patch(&self, delete: &[String], fields: &Map<String, Value>) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() {
            return;
        }
        let fields = fields.clone();
        let delete = delete.to_vec();
        let _ = locked_update(sf, move |data| {
            for k in &delete {
                data.remove(k);
            }
            for (k, v) in fields {
                data.insert(k, v);
            }
        });
    }

    fn record_stage_error(&self, class: &str, detail: &str) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() || class.is_empty() {
            return;
        }
        let attempt = attempt_of(&read_state_json(Some(sf)));
        if attempt.is_empty() {
            return;
        }
        let Some(state_dir) = sf.parent() else {
            return;
        };
        let bail_path = state_dir.join(format!("bail_{attempt}.json"));
        let payload = serde_json::json!({
            "class": class,
            "detail": detail,
            "ts": now_iso(),
        });
        write_bail_atomically(state_dir, &bail_path, &attempt, &payload);
    }

    fn accumulate_token_usage(&self, usage: &HashMap<String, i64>) {
        if usage.is_empty() {
            return;
        }
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() {
            return;
        }
        let usage = usage.clone();
        let _ = locked_update(sf, move |data| {
            let mut total: Map<String, Value> = data
                .get("token_usage")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            for (k, v) in &usage {
                let cur = total.get(k).map(as_i64).unwrap_or(0);
                total.insert(k.clone(), Value::from(cur + v));
            }
            data.insert("token_usage".into(), Value::Object(total));
        });
    }

    fn stage_error(&self) -> Option<Map<String, Value>> {
        let sf = self.state_file.as_ref()?;
        if !sf.exists() {
            return None;
        }
        let attempt = attempt_of(&read_state_json(Some(sf)));
        if attempt.is_empty() {
            return None;
        }
        let bail_path = sf.parent()?.join(format!("bail_{attempt}.json"));
        serde_json::from_str(&std::fs::read_to_string(bail_path).ok()?).ok()
    }

    fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<&str>,
        paths: Option<&HashMap<String, String>>,
    ) {
        if group_name.is_empty() {
            return;
        }
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() {
            return;
        }
        let group_name = group_name.to_string();
        let base_head = base_head.map(String::from);
        let paths = paths.cloned();
        let _ = locked_update(sf, move |data| {
            let mut groups = data
                .get("parallel_worktrees")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            if base_head.is_none() && paths.is_none() {
                groups.remove(&group_name);
            } else {
                let mut entry = Map::new();
                entry.insert(
                    "base_head".into(),
                    Value::String(base_head.unwrap_or_default()),
                );
                let mut ps = Map::new();
                for (k, v) in paths.unwrap_or_default() {
                    ps.insert(k, Value::String(v));
                }
                entry.insert("paths".into(), Value::Object(ps));
                groups.insert(group_name, Value::Object(entry));
            }
            if groups.is_empty() {
                data.remove("parallel_worktrees");
            } else {
                data.insert("parallel_worktrees".into(), Value::Object(groups));
            }
        });
    }

    fn add_subprocess_cost(&self, amount: f64) {
        if amount == 0.0 || !amount.is_finite() || amount < 0.0 {
            return;
        }
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() {
            return;
        }
        let _ = locked_update(sf, move |data| {
            let current = data
                .get("subprocess_cost_usd")
                .map(as_i64_f64)
                .unwrap_or(0.0);
            data.insert("subprocess_cost_usd".into(), Value::from(current + amount));
        });
    }

    fn patch_parallel_attempt(&self, child_key: &str, attempt: &str) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if !sf.exists() || attempt.is_empty() {
            return;
        }
        let child_key = child_key.to_string();
        let attempt = attempt.to_string();
        let _ = locked_update(sf, move |data| {
            let mut pa = data
                .get("parallel_attempts")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            pa.insert(child_key, Value::String(attempt));
            data.insert("parallel_attempts".into(), Value::Object(pa));
        });
    }

    fn parallel_worktrees(&self, group_name: &str) -> (String, HashMap<String, String>) {
        let Some(sf) = self.state_file.as_ref() else {
            return (String::new(), HashMap::new());
        };
        if !sf.exists() {
            return (String::new(), HashMap::new());
        }
        let data = read_state_json(Some(sf));
        let Some(entry) = data
            .get("parallel_worktrees")
            .and_then(|v| v.as_object())
            .and_then(|o| o.get(group_name))
            .and_then(|v| v.as_object())
        else {
            return (String::new(), HashMap::new());
        };
        let base_head = entry
            .get("base_head")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let paths = entry
            .get("paths")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        (base_head, paths)
    }

    fn write_terminal_state(&self, exit_code: i32) {
        let Some(sf) = self.state_file.as_ref() else {
            return;
        };
        if let Some(state_dir) = sf.parent() {
            let _ = File::create(state_dir.join("finished"));
        }
        let mut fields = Map::new();
        fields.insert(
            "status".into(),
            Value::String(if exit_code == 0 { "done" } else { "stopped" }.into()),
        );
        fields.insert("ended_at".into(), Value::String(now_stamp()));
        fields.insert("exit_code".into(), Value::from(exit_code));
        self.patch(&[], &fields);
    }
}

// ---------------------------------------------------------------------------
// StateData — the public handle that owns identity + store
// ---------------------------------------------------------------------------

pub struct StateData {
    pub gremlin_id: Option<String>,
    store: Box<dyn StateStore + Send + Sync>,
}

impl StateData {
    pub fn new(gremlin_id: Option<String>) -> Self {
        let store = Box::new(FileStateStore::new(gremlin_id.clone()));
        Self { gremlin_id, store }
    }

    // --- delegating reads ---

    pub fn get_field(&self, name: &str) -> Option<Value> {
        self.store.get_field(name)
    }

    pub fn read_field(&self, field: &str) -> Option<Value> {
        self.store.read_field(field)
    }

    pub fn read_str(&self, field: &str) -> String {
        self.store.read_str(field)
    }

    pub fn stage_error(&self) -> Option<Map<String, Value>> {
        self.store.stage_error()
    }

    pub fn parallel_worktrees(&self, group_name: &str) -> (String, HashMap<String, String>) {
        self.store.parallel_worktrees(group_name)
    }

    // --- delegating writes ---

    pub fn patch(&self, delete: &[String], fields: &Map<String, Value>) {
        self.store.patch(delete, fields);
    }

    pub fn record_stage_error(&self, class: &str, detail: &str) {
        self.store.record_stage_error(class, detail);
    }

    pub fn accumulate_token_usage(&self, usage: &HashMap<String, i64>) {
        self.store.accumulate_token_usage(usage);
    }

    pub fn persist(
        &mut self,
        state_dir: &Path,
        data: &Map<String, Value>,
    ) -> Result<(), StateError> {
        let Some(gid) = self.gremlin_id.clone() else {
            return Err(StateError::Other(
                "cannot persist StateData with no gremlin_id".into(),
            ));
        };
        let mut out = data.clone();
        out.insert("id".into(), Value::String(gid));
        self.store.persist(state_dir, &out)
    }

    pub fn state_tree(&self) -> Map<String, Value> {
        self.store.state_tree()
    }

    pub fn seed(&mut self, data: &Map<String, Value>) -> Result<(), StateError> {
        self.store.seed(data)
    }

    #[allow(dead_code)]
    pub(crate) fn open(
        &self,
        name: &str,
        mode: BlobMode,
    ) -> Result<Box<dyn StateBlob>, StateError> {
        self.store.open(name, mode)
    }

    #[allow(dead_code)]
    pub(crate) fn exists(&self, name: &str) -> bool {
        self.store.exists(name)
    }

    pub fn clear_stage_error(&self, attempt: &str) {
        self.store.clear_stage_error(attempt);
    }

    pub fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<&str>,
        paths: Option<&HashMap<String, String>>,
    ) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() {
            return;
        }
        self.store
            .patch_parallel_worktrees(group_name, base_head, paths);
    }

    pub fn add_subprocess_cost(&self, amount: f64) {
        self.store.add_subprocess_cost(amount);
    }

    pub fn patch_parallel_attempt(&self, child_key: &str, attempt: &str) {
        self.store.patch_parallel_attempt(child_key, attempt);
    }

    // --- methods with guards that stay on StateData ---

    pub fn set_stage(&self, stage: &str, sub_stage: Option<&Value>, parent_stage: &str) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() {
            return;
        }
        let scoped = !parent_stage.is_empty();
        let target_stage = if scoped { parent_stage } else { stage };
        if target_stage.is_empty() {
            return;
        }
        let mut fields = Map::new();
        fields.insert("stage".into(), Value::String(target_stage.to_string()));
        fields.insert("stage_updated_at".into(), Value::String(now_stamp()));
        match if scoped {
            Some(Value::String(stage.to_string()))
        } else {
            sub_stage.cloned()
        } {
            Some(sub) => {
                fields.insert("sub_stage".into(), sub);
                self.store.patch(&[], &fields);
            }
            None => self.store.patch(&["sub_stage".to_string()], &fields),
        }
    }

    /// Delete the whole `parallel_attempts` map.
    pub fn clear_parallel_attempts(&self) {
        self.store
            .patch(&["parallel_attempts".to_string()], &Map::new());
    }

    pub fn write_terminal_state(&self, exit_code: i32) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() {
            return;
        }
        self.store.write_terminal_state(exit_code);
    }
}

// ---------------------------------------------------------------------------
// Free functions (unchanged)
// ---------------------------------------------------------------------------

fn rand_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    let filled = File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !filled {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn token_hex(n_bytes: usize) -> String {
    rand_hex(n_bytes)
}

fn now_utc() -> libc::tm {
    let mut t: libc::time_t = 0;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::time(std::ptr::addr_of_mut!(t));
        libc::gmtime_r(std::ptr::addr_of!(t), std::ptr::addr_of_mut!(tm));
    }
    tm
}

/// `%Y-%m-%dT%H:%M:%SZ` — second precision, UTC.
pub fn now_stamp() -> String {
    let tm = now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

/// `[YYYY-MM-DDTHH:MM:SS.sssZ]` — millisecond precision, UTC.
/// Matches `env_logger`'s `format_timestamp_millis()` format.
pub fn now_stamp_millis() -> String {
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        libc::gmtime_r(std::ptr::addr_of!(t), std::ptr::addr_of_mut!(tm));
        tm
    };
    format!(
        "[{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z]",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        millis,
    )
}

/// ISO-8601 with microseconds and `+00:00`, matching Python `datetime.isoformat()`.
pub fn now_iso() -> String {
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs();
    let micros = since_epoch.subsec_micros();
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        libc::gmtime_r(std::ptr::addr_of!(t), std::ptr::addr_of_mut!(tm));
        tm
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}+00:00",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        micros,
    )
}

pub fn default_for(name: &str) -> Option<Value> {
    Some(match name {
        "attempt" | "kind" | "project_root" | "workdir" | "setup_kind" | "worktree_base"
        | "status" | "started_at" | "description" | "parent_id" | "client" | "definition_path"
        | "stage" | "group_name" | "child_key" => Value::String(String::new()),
        "definition_args" => Value::Array(Vec::new()),
        "stage_inputs" | "metadata" => Value::Object(Map::new()),
        "pid" | "exit_code" => Value::Null,
        _ => return None,
    })
}

pub fn field_names() -> [&'static str; 20] {
    [
        "attempt",
        "kind",
        "project_root",
        "workdir",
        "setup_kind",
        "worktree_base",
        "status",
        "started_at",
        "description",
        "parent_id",
        "definition_args",
        "client",
        "definition_path",
        "stage",
        "pid",
        "stage_inputs",
        "metadata",
        "group_name",
        "child_key",
        "exit_code",
    ]
}

/// The state directory for `gremlin_id`.
///
/// This is the single source of truth for where a gremlin's state directory
/// lives on disk. Every caller that needs to locate a gremlin's state dir
/// — whether to open its `state.json`, read its log, or merge its
/// artifacts — must go through this function, not through `config`.
pub fn state_dir_for(gremlin_id: &str) -> PathBuf {
    config::state_root().join(gremlin_id)
}

pub fn resolve_state_file(gremlin_id: Option<&str>) -> Option<PathBuf> {
    let id = gremlin_id.filter(|s| !s.is_empty())?;
    Some(state_dir_for(id).join("state.json"))
}

/// Enumerate `(id, state.json path)` pairs under the state root.
///
/// Only subdirectories that actually contain a `state.json` are returned.
/// Callers that need to skip closed gremlins check the marker file next to the
/// returned path themselves.
pub fn list_state_dirs() -> Vec<(String, PathBuf)> {
    let root = config::state_root();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_json_path = path.join("state.json");
        if !state_json_path.is_file() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        out.push((name, state_json_path));
    }
    out.sort();
    out
}

pub fn read_state_json(sf: Option<&Path>) -> Map<String, Value> {
    let Some(p) = sf else {
        return Map::new();
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn write_state(state_dir: &Path, data: &Map<String, Value>) -> Result<(), StateError> {
    let sf = state_dir.join("state.json");
    let _lock = acquire_lock(&sf)?;
    let tmp = state_dir.join(format!(
        "state.json.{}.{}.tmp",
        std::process::id(),
        rand_hex(4)
    ));
    std::fs::write(&tmp, serde_json::to_string(data)?)?;
    std::fs::rename(&tmp, &sf)?;
    Ok(())
}

fn lock_path(sf: &Path) -> PathBuf {
    let name = sf
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state.json");
    sf.with_file_name(format!("{name}.lock"))
}

pub fn acquire_lock(sf: &Path) -> Result<File, StateError> {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(lock_path(sf))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(StateError::Io(std::io::Error::last_os_error()));
    }
    Ok(f)
}

/// Async counterpart of [`acquire_lock`]: runs the flock on a blocking thread
/// so the tokio runtime is never blocked on filesystem locks.
pub async fn acquire_lock_async(sf: &Path) -> Result<File, StateError> {
    let sf = sf.to_path_buf();
    tokio::task::spawn_blocking(move || acquire_lock(&sf))
        .await
        .map_err(|e| StateError::Io(std::io::Error::other(e.to_string())))?
}

pub fn read_json_map(sf: &Path) -> Result<Map<String, Value>, StateError> {
    Ok(serde_json::from_str(&std::fs::read_to_string(sf)?)?)
}

pub fn atomic_write_json(sf: &Path, data: &Map<String, Value>) -> Result<(), StateError> {
    let name = sf
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state.json");
    let tmp = sf.with_file_name(format!("{name}.{}.{}.tmp", std::process::id(), rand_hex(8)));
    std::fs::write(&tmp, serde_json::to_string(data)?)?;
    std::fs::rename(&tmp, sf)?;
    Ok(())
}

/// Async counterpart of [`atomic_write_json`]: uses [`tokio::fs::write`] and
/// [`tokio::fs::rename`] so the write + rename never block the runtime.
pub async fn atomic_write_json_async(
    sf: &Path,
    data: &Map<String, Value>,
) -> Result<(), StateError> {
    let name = sf
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state.json");
    let tmp = sf.with_file_name(format!("{name}.{}.{}.tmp", std::process::id(), rand_hex(8)));
    tokio::fs::write(&tmp, serde_json::to_string(data)?).await?;
    tokio::fs::rename(&tmp, sf).await?;
    Ok(())
}

/// Exclusive-lock, read, apply, atomically replace.
pub fn locked_update(
    sf: &Path,
    apply: impl FnOnce(&mut Map<String, Value>),
) -> Result<(), StateError> {
    let _lock = acquire_lock(sf)?;
    let mut data = read_json_map(sf)?;
    apply(&mut data);
    atomic_write_json(sf, &data)
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn attempt_of(data: &Map<String, Value>) -> String {
    match data.get("attempt") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Write `payload` to `bail_path` with first-writer-wins semantics.
///
/// The destination is created with no-replace semantics via a hard link from a
/// uniquely named temporary file, so two concurrent writers can never clobber
/// each other's payload: the loser's link fails with `AlreadyExists` and its
/// temporary file is removed. This replaces the racy `exists()`-then-`rename`
/// check-then-act sequence.
fn write_bail_atomically(state_dir: &Path, bail_path: &Path, attempt: &str, payload: &Value) {
    let tmp = state_dir.join(format!(".bail_{attempt}_{}.tmp", rand_hex(4)));
    if std::fs::write(&tmp, payload.to_string()).is_err() {
        return;
    }
    // `hard_link` fails if the destination already exists, giving us an atomic
    // create-if-absent without a separate existence check. Either way the
    // temporary file is no longer needed.
    let _ = std::fs::hard_link(&tmp, bail_path);
    let _ = std::fs::remove_file(&tmp);
}

fn as_i64_f64(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_sandbox;

    fn seed(sandbox: &crate::test_support::Sandbox, gremlin_id: &str) -> PathBuf {
        let state_dir = sandbox.path().join("state").join(gremlin_id);
        std::fs::create_dir_all(&state_dir).unwrap();
        let sf = state_dir.join("state.json");
        std::fs::write(
            &sf,
            format!(r#"{{"id": "{gremlin_id}", "stage": "implement"}}"#),
        )
        .unwrap();
        sf
    }

    #[test]
    fn resolve_state_file_builds_path() {
        with_sandbox(None, |sandbox| {
            let p = resolve_state_file(Some("abc")).unwrap();
            assert!(p.ends_with("abc/state.json"), "{p:?}");
            assert!(resolve_state_file(None).is_none());
            assert!(resolve_state_file(Some("")).is_none());
            let _ = sandbox;
        });
    }

    #[test]
    fn field_reads_after_disk_write() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            assert_eq!(d.get_field("stage").unwrap(), "implement");
            let mut fields = Map::new();
            fields.insert("stage".into(), Value::String("review".into()));
            d.patch(&[], &fields);
            assert_eq!(d.get_field("stage").unwrap(), "review");
            let _ = sf;
        });
    }

    #[test]
    fn write_and_read_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut data = Map::new();
        data.insert("id".into(), Value::String("g1".into()));
        data.insert("stage".into(), Value::String("running".into()));
        write_state(dir.path(), &data).unwrap();
        let read = read_state_json(Some(&dir.path().join("state.json")));
        assert_eq!(read.get("stage").unwrap(), "running");
        assert!(read_state_json(None).is_empty());
        assert!(read_state_json(Some(dir.path())).is_empty());
    }

    #[test]
    fn defaults_for_every_field() {
        for name in field_names() {
            assert!(default_for(name).is_some(), "missing default for {name}");
        }
        assert!(default_for("nope").is_none());
        assert_eq!(
            default_for("definition_args").unwrap(),
            Value::Array(vec![])
        );
        assert_eq!(default_for("pid").unwrap(), Value::Null);
        assert_eq!(default_for("exit_code").unwrap(), Value::Null);
    }

    #[test]
    fn get_field_falls_back_to_default() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            assert_eq!(d.get_field("attempt").unwrap(), "");
            assert_eq!(d.get_field("stage").unwrap(), "implement");
            assert_eq!(
                d.get_field("definition_args").unwrap(),
                Value::Array(vec![])
            );
            assert!(d.get_field("bogus").is_none());
            let _ = sf;
        });
    }

    #[test]
    fn patch_merges_and_deletes() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);
            let mut fields = Map::new();
            fields.insert("stage".into(), Value::String("review".into()));
            d.patch(&["sub_stage".into()], &fields);
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("attempt").unwrap(), "a1");
            assert_eq!(raw.get("stage").unwrap(), "review");
            assert_eq!(raw.get("id").unwrap(), "gr-test");
            assert!(!raw.contains_key("sub_stage"));
        });
    }

    #[test]
    fn patch_noop_without_gremlin_id() {
        let d = StateData::new(None);
        d.patch(&[], &Map::new());
        d.record_stage_error("other", "x");
        d.set_stage("running", None, "");
    }

    #[test]
    fn set_stage_writes_stamp_and_deletes_sub_stage() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.set_stage("implement", Some(&serde_json::json!({"k": 1})), "");
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("stage").unwrap(), "implement");
            assert_eq!(raw.get("sub_stage").unwrap(), &serde_json::json!({"k": 1}));
            let ts = raw.get("stage_updated_at").unwrap().as_str().unwrap();
            assert!(ts.ends_with('Z'));
            assert_eq!(ts.len(), 20);

            d.set_stage("review-code", None, "");
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("stage").unwrap(), "review-code");
            assert!(!raw.contains_key("sub_stage"));
        });
    }

    #[test]
    fn set_stage_parent_pins_stage_and_sub_stage() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.set_stage("github-review-pull-request", None, "reviews");
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("stage").unwrap(), "reviews");
            assert_eq!(raw.get("sub_stage").unwrap(), "github-review-pull-request");
        });
    }

    #[test]
    fn record_stage_error_requires_attempt() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.record_stage_error("other", "no attempt yet");
            let state_dir = sf.parent().unwrap();
            assert!(std::fs::read_dir(state_dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("bail_")));

            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);
            d.record_stage_error("other", "boom");
            let bail = state_dir.join("bail_a1.json");
            assert!(bail.exists());
            let info = d.stage_error().unwrap();
            assert_eq!(info.get("class").unwrap().as_str(), Some("other"));
            assert_eq!(info.get("detail").unwrap().as_str(), Some("boom"));
            assert!(info.get("ts").unwrap().as_str().unwrap().len() > 20);

            // Second write must not clobber the existing bail file.
            d.record_stage_error("security", "second");
            assert_eq!(
                d.stage_error().unwrap().get("class"),
                Some(&Value::String("other".into()))
            );
        });
    }

    #[test]
    fn record_stage_error_is_first_writer_wins_under_concurrency() {
        // Many threads race to write the same bail file; exactly one payload
        // must win and no temporary files may be left behind.
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);

            let handles: Vec<_> = (0..16)
                .map(|i| {
                    std::thread::spawn(move || {
                        let data = StateData::new(Some("gr-test".into()));
                        data.record_stage_error("other", &format!("writer-{i}"));
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }

            let state_dir = sf.parent().unwrap();
            let bail = state_dir.join("bail_a1.json");
            assert!(bail.exists());
            // The file is valid JSON with a single winner's detail.
            let info = d.stage_error().unwrap();
            assert_eq!(info.get("class").unwrap().as_str(), Some("other"));
            let detail = info.get("detail").unwrap().as_str().unwrap();
            assert!(
                detail.starts_with("writer-"),
                "unexpected detail {detail:?}"
            );

            // No leftover temp files.
            let leftovers: Vec<String> = std::fs::read_dir(state_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with(".bail_"))
                .collect();
            assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        });
    }

    #[test]
    fn stage_error_keeps_non_string_values() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);
            let state_dir = sf.parent().unwrap();
            std::fs::write(
                state_dir.join("bail_a1.json"),
                r#"{"class": "other", "detail": "boom", "count": 3, "nested": {"k": [1, null]}}"#,
            )
            .unwrap();
            let info = d.stage_error().unwrap();
            assert_eq!(info.get("count"), Some(&Value::from(3)));
            assert_eq!(
                info.get("nested"),
                Some(&serde_json::json!({"k": [1, null]}))
            );

            std::fs::write(state_dir.join("bail_a1.json"), "[1, 2]").unwrap();
            assert!(d.stage_error().is_none());
        });
    }

    #[test]
    fn accumulate_token_usage_adds_integers() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.accumulate_token_usage(&HashMap::from([("prompt_tokens".to_string(), 5)]));
            d.accumulate_token_usage(&HashMap::from([
                ("prompt_tokens".to_string(), 3),
                ("turns".to_string(), 2),
            ]));
            let raw = read_state_json(Some(&sf));
            let usage = raw.get("token_usage").unwrap().as_object().unwrap();
            assert_eq!(usage.get("prompt_tokens").unwrap(), 8);
            assert_eq!(usage.get("turns").unwrap(), 2);
        });
    }

    #[test]
    fn parallel_worktrees_add_and_clear() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.patch_parallel_worktrees(
                "reviews",
                Some("abc123"),
                Some(&HashMap::from([("a".to_string(), "/wt/a".to_string())])),
            );
            let raw = read_state_json(Some(&sf));
            let entry = &raw.get("parallel_worktrees").unwrap()["reviews"];
            assert_eq!(entry["base_head"], "abc123");
            assert_eq!(entry["paths"]["a"], "/wt/a");

            d.patch_parallel_worktrees("reviews", None, None);
            assert!(!read_state_json(Some(&sf)).contains_key("parallel_worktrees"));
        });
    }

    #[test]
    fn subprocess_cost_accumulates_and_validates() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.add_subprocess_cost(0.25);
            d.add_subprocess_cost(0.5);
            d.add_subprocess_cost(-1.0);
            d.add_subprocess_cost(f64::NAN);
            d.add_subprocess_cost(0.0);
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("subprocess_cost_usd").unwrap(), 0.75);
        });
    }

    #[test]
    fn terminal_state_touches_finished_and_patches() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.write_terminal_state(0);
            let state_dir = sf.parent().unwrap();
            assert!(state_dir.join("finished").exists());
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("status").unwrap(), "done");
            assert_eq!(raw.get("exit_code").unwrap(), 0);

            d.write_terminal_state(3);
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw.get("status").unwrap(), "stopped");
            assert_eq!(raw.get("exit_code").unwrap(), 3);
        });
    }

    #[test]
    fn persist_writes_id_and_sets_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = StateData::new(Some("child".into()));
        let mut payload = Map::new();
        payload.insert("definition_path".into(), Value::String("/p.yaml".into()));
        d.persist(dir.path(), &payload).unwrap();
        let raw = d.state_tree();
        assert_eq!(raw.get("id").unwrap(), "child");
        assert_eq!(raw.get("definition_path").unwrap(), "/p.yaml");

        let mut none = StateData::new(None);
        assert!(none.persist(dir.path(), &Map::new()).is_err());
    }

    #[test]
    fn open_creates_and_reads_blob() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            // Write through open
            {
                let mut blob = d.open("test.txt", BlobMode::Write).unwrap();
                blob.write_all(b"hello").unwrap();
            }
            // Read back
            let state_dir = sf.parent().unwrap();
            let content = std::fs::read_to_string(state_dir.join("test.txt")).unwrap();
            assert_eq!(content, "hello");
        });
    }

    #[test]
    fn seed_writes_state_and_tree_reads_it() {
        with_sandbox(None, |sandbox| {
            let _sf = seed(sandbox, "gr-test");
            let mut d = StateData::new(Some("gr-test".into()));
            let mut data = Map::new();
            data.insert("stage".into(), Value::String("seeded".into()));
            d.seed(&data).unwrap();
            let tree = d.state_tree();
            assert_eq!(tree.get("stage").unwrap(), "seeded");
        });
    }

    #[test]
    fn clear_stage_error_removes_bail_file() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String("a1".into()));
            d.patch(&[], &fields);
            d.record_stage_error("other", "boom");
            let state_dir = sf.parent().unwrap();
            assert!(state_dir.join("bail_a1.json").exists());
            d.clear_stage_error("a1");
            assert!(!state_dir.join("bail_a1.json").exists());
        });
    }

    #[test]
    fn locked_update_read_modify_write() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            locked_update(&sf, |data| {
                let n = data.get("counter").map(as_i64).unwrap_or(0) + 1;
                data.insert("counter".into(), Value::from(n));
            })
            .unwrap();
            assert_eq!(read_state_json(Some(&sf)).get("counter").unwrap(), 1);
            let state_dir = sf.parent().unwrap();
            assert!(state_dir.join("state.json.lock").exists());
        });
    }

    #[test]
    fn parallel_attempt_patch() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.patch_parallel_attempt("bail-child", "attempt-bail");
            let raw = read_state_json(Some(&sf));
            assert_eq!(raw["parallel_attempts"]["bail-child"], "attempt-bail");
        });
    }

    #[test]
    fn parallel_worktrees_reads_back() {
        with_sandbox(None, |sandbox| {
            let _sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            let mut paths = HashMap::new();
            paths.insert("a".to_string(), "/wt/a".to_string());
            d.patch_parallel_worktrees("reviews", Some("abc123"), Some(&paths));
            let (base, read_paths) = d.parallel_worktrees("reviews");
            assert_eq!(base, "abc123");
            assert_eq!(read_paths.get("a").map(String::as_str), Some("/wt/a"));
            assert!(d.parallel_worktrees("missing").1.is_empty());
        });
    }

    #[test]
    fn clear_parallel_attempts_removes_key() {
        with_sandbox(None, |sandbox| {
            let sf = seed(sandbox, "gr-test");
            let d = StateData::new(Some("gr-test".into()));
            d.patch_parallel_attempt("a", "attempt-a");
            d.clear_parallel_attempts();
            assert!(read_state_json(Some(&sf))
                .get("parallel_attempts")
                .is_none());
        });
    }

    #[test]
    fn write_state_acquires_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut data = Map::new();
        data.insert("id".into(), Value::String("g1".into()));
        write_state(dir.path(), &data).unwrap();
        assert!(dir.path().join("state.json.lock").exists());
    }

    #[test]
    fn state_data_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<StateData>();
        assert_sync::<StateData>();
    }
}
