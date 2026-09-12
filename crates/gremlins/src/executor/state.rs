//! state.json I/O, flock-guarded updates, and StateData.

use std::collections::{HashMap, HashSet};
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

/// ISO-8601 with microseconds and `+00:00`, matching Python `datetime.isoformat()`.
pub fn now_iso() -> String {
    let tm = now_utc();
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_micros())
        .unwrap_or(0);
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
        | "status" | "started_at" | "description" | "parent_id" | "client" | "pipeline_path"
        | "stage" | "group_name" | "child_key" => Value::String(String::new()),
        "pipeline_args" => Value::Array(Vec::new()),
        "stage_inputs" => Value::Object(Map::new()),
        "pid" | "exit_code" => Value::Null,
        _ => return None,
    })
}

pub fn field_names() -> [&'static str; 19] {
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
        "pipeline_args",
        "client",
        "pipeline_path",
        "stage",
        "pid",
        "stage_inputs",
        "group_name",
        "child_key",
        "exit_code",
    ]
}

pub fn resolve_state_file(gremlin_id: Option<&str>) -> Option<PathBuf> {
    let id = gremlin_id.filter(|s| !s.is_empty())?;
    Some(config::state_root().join(id).join("state.json"))
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

pub struct StateData {
    pub gremlin_id: Option<String>,
    pub state_file: Option<PathBuf>,
    cache: std::cell::RefCell<Option<Map<String, Value>>>,
}

impl StateData {
    pub fn new(gremlin_id: Option<String>) -> Self {
        let state_file = resolve_state_file(gremlin_id.as_deref());
        StateData {
            gremlin_id,
            state_file,
            cache: std::cell::RefCell::new(None),
        }
    }

    fn sf(&self) -> Option<PathBuf> {
        self.state_file
            .clone()
            .or_else(|| resolve_state_file(self.gremlin_id.as_deref()))
    }

    /// Parsed state.json, read from disk on first use. Every writer must invalidate.
    fn loaded(&self) -> std::cell::Ref<'_, Map<String, Value>> {
        if self.cache.borrow().is_none() {
            let data = read_state_json(self.sf().as_deref());
            *self.cache.borrow_mut() = Some(data);
        }
        std::cell::Ref::map(self.cache.borrow(), |c| c.as_ref().unwrap())
    }

    pub fn invalidate(&self) {
        *self.cache.borrow_mut() = None;
    }

    pub fn get_field(&self, name: &str) -> Option<Value> {
        let default = default_for(name)?;
        Some(self.loaded().get(name).cloned().unwrap_or(default))
    }

    /// Present, non-null value — `None` when absent or null.
    pub fn read_field(&self, field: &str) -> Option<Value> {
        let sf = self.sf()?;
        if !sf.exists() {
            return None;
        }
        match self.loaded().get(field) {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.clone()),
        }
    }

    /// Fresh read, deliberately uncached: another process may have patched state.json
    /// since our cache was filled. Falsy values read as `""`, matching Python's
    /// `json.loads(...).get(field) or ""`.
    pub fn read_str(&self, field: &str) -> String {
        let Some(sf) = self.sf() else {
            return String::new();
        };
        if !sf.exists() {
            return String::new();
        }
        match read_state_json(Some(&sf)).get(field) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) if n.as_f64() != Some(0.0) => n.to_string(),
            Some(Value::Bool(true)) => "True".into(),
            _ => String::new(),
        }
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
        write_state(state_dir, &out)?;
        self.state_file = Some(state_dir.join("state.json"));
        self.invalidate();
        Ok(())
    }

    pub fn patch(&self, delete: &[String], fields: &Map<String, Value>) {
        let Some(sf) = self.sf() else { return };
        if !sf.exists() {
            return;
        }
        let fields = fields.clone();
        let delete = delete.to_vec();
        let _ = locked_update(&sf, move |data| {
            for k in &delete {
                data.remove(k);
            }
            for (k, v) in fields {
                data.insert(k, v);
            }
        });
        self.invalidate();
    }

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
                self.patch(&[], &fields);
            }
            None => self.patch(&["sub_stage".to_string()], &fields),
        }
    }

    pub fn write_bail_file(&self, bail_class: &str, bail_detail: &str) {
        let Some(sf) = self.sf() else { return };
        if !sf.exists() || bail_class.is_empty() {
            return;
        }
        let attempt = attempt_of(&read_state_json(Some(&sf)));
        if attempt.is_empty() {
            return;
        }
        let Some(state_dir) = sf.parent() else { return };
        let bail_path = state_dir.join(format!("bail_{attempt}.json"));
        if bail_path.exists() {
            return;
        }
        let payload = serde_json::json!({
            "class": bail_class,
            "detail": bail_detail,
            "ts": now_iso(),
        });
        let tmp = state_dir.join(format!(".bail_{attempt}_{}.tmp", rand_hex(4)));
        if std::fs::write(&tmp, payload.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &bail_path);
        }
    }

    pub fn accumulate_token_usage(&self, usage: &HashMap<String, i64>) {
        if usage.is_empty() {
            return;
        }
        let Some(sf) = self.sf() else { return };
        if !sf.exists() {
            return;
        }
        let usage = usage.clone();
        let _ = locked_update(&sf, move |data| {
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
        self.invalidate();
    }

    /// Bail records are untyped — any JSON object passes, non-objects read as `None`.
    pub fn read_bail_info(&self) -> Option<Map<String, Value>> {
        let sf = self.sf()?;
        if !sf.exists() {
            return None;
        }
        let attempt = attempt_of(&read_state_json(Some(&sf)));
        if attempt.is_empty() {
            return None;
        }
        let bail_path = sf.parent()?.join(format!("bail_{attempt}.json"));
        serde_json::from_str(&std::fs::read_to_string(bail_path).ok()?).ok()
    }

    pub fn patch_parallel_worktrees(
        &self,
        group_name: &str,
        base_head: Option<&str>,
        paths: Option<&HashMap<String, String>>,
    ) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() || group_name.is_empty() {
            return;
        }
        let Some(sf) = self.sf() else { return };
        if !sf.exists() {
            return;
        }
        let group_name = group_name.to_string();
        let base_head = base_head.map(String::from);
        let paths = paths.cloned();
        let _ = locked_update(&sf, move |data| {
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
        self.invalidate();
    }

    pub fn done_for(&self, path: &str) -> HashSet<String> {
        let Some(sf) = self.sf() else {
            return HashSet::new();
        };
        if !sf.exists() {
            return HashSet::new();
        }
        read_state_json(Some(&sf))
            .get("done_children")
            .and_then(|v| v.as_object())
            .and_then(|o| o.get(path))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn mark_done(&self, path: &str, child_name: &str) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() || path.is_empty() {
            return;
        }
        let Some(sf) = self.sf() else { return };
        if !sf.exists() {
            return;
        }
        let path = path.to_string();
        let child_name = child_name.to_string();
        let _ = locked_update(&sf, move |data| {
            let mut dc = data
                .get("done_children")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            let mut existing: Vec<Value> = dc
                .get(&path)
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            if !existing.iter().any(|v| v.as_str() == Some(&child_name)) {
                existing.push(Value::String(child_name));
            }
            dc.insert(path, Value::Array(existing));
            data.insert("done_children".into(), Value::Object(dc));
        });
        self.invalidate();
    }

    pub fn clear_done(&self, path: &str) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() || path.is_empty() {
            return;
        }
        let Some(sf) = self.sf() else { return };
        if !sf.exists() {
            return;
        }
        let path = path.to_string();
        let _ = locked_update(&sf, move |data| {
            let mut dc = data
                .get("done_children")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            dc.remove(&path);
            if dc.is_empty() {
                data.remove("done_children");
            } else {
                data.insert("done_children".into(), Value::Object(dc));
            }
        });
        self.invalidate();
    }

    pub fn add_subprocess_cost(&self, amount: f64) {
        if amount == 0.0 || !amount.is_finite() || amount < 0.0 {
            return;
        }
        let Some(sf) = self.sf() else { return };
        if !sf.exists() {
            return;
        }
        let _ = locked_update(&sf, move |data| {
            let current = data
                .get("subprocess_cost_usd")
                .map(as_i64_f64)
                .unwrap_or(0.0);
            data.insert("subprocess_cost_usd".into(), Value::from(current + amount));
        });
        self.invalidate();
    }

    pub fn patch_parallel_attempt(&self, child_key: &str, attempt: &str) {
        let Some(sf) = self.sf() else { return };
        if !sf.exists() || attempt.is_empty() {
            return;
        }
        let child_key = child_key.to_string();
        let attempt = attempt.to_string();
        let _ = locked_update(&sf, move |data| {
            let mut pa = data
                .get("parallel_attempts")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            pa.insert(child_key, Value::String(attempt));
            data.insert("parallel_attempts".into(), Value::Object(pa));
        });
        self.invalidate();
    }

    pub fn write_terminal_state(&self, exit_code: i32) {
        if self.gremlin_id.as_deref().unwrap_or("").is_empty() {
            return;
        }
        let Some(sf) = self.sf() else { return };
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
        self.invalidate();
    }
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

    fn seed(dir: &Path, gremlin_id: &str) -> PathBuf {
        let sf = dir.join("state.json");
        std::fs::write(
            &sf,
            format!(r#"{{"id": "{gremlin_id}", "stage": "implement"}}"#),
        )
        .unwrap();
        sf
    }

    fn data_with(sf: &Path) -> StateData {
        let mut d = StateData::new(Some("gr-test".into()));
        d.state_file = Some(sf.to_path_buf());
        d
    }

    #[test]
    fn resolve_state_file_builds_path() {
        let p = resolve_state_file(Some("abc")).unwrap();
        assert!(p.ends_with("abc/state.json"), "{p:?}");
        assert!(resolve_state_file(None).is_none());
        assert!(resolve_state_file(Some("")).is_none());
    }

    #[test]
    fn cached_field_sees_own_writes() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        assert_eq!(d.get_field("stage").unwrap(), "implement");
        let mut fields = Map::new();
        fields.insert("stage".into(), Value::String("review".into()));
        d.patch(&[], &fields);
        assert_eq!(d.get_field("stage").unwrap(), "review");
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
        assert_eq!(default_for("pipeline_args").unwrap(), Value::Array(vec![]));
        assert_eq!(default_for("pid").unwrap(), Value::Null);
        assert_eq!(default_for("exit_code").unwrap(), Value::Null);
    }

    #[test]
    fn get_field_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        assert_eq!(d.get_field("attempt").unwrap(), "");
        assert_eq!(d.get_field("stage").unwrap(), "implement");
        assert_eq!(d.get_field("pipeline_args").unwrap(), Value::Array(vec![]));
        assert!(d.get_field("bogus").is_none());
    }

    #[test]
    fn patch_merges_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
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
    }

    #[test]
    fn patch_noop_without_gremlin_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = StateData::new(None);
        d.state_file = Some(dir.path().join("missing.json"));
        d.patch(&[], &Map::new());
        d.write_bail_file("other", "x");
        d.set_stage("running", None, "");
        assert!(!dir.path().join("missing.json").exists());
    }

    #[test]
    fn set_stage_writes_stamp_and_deletes_sub_stage() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
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
    }

    #[test]
    fn set_stage_parent_pins_stage_and_sub_stage() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        d.set_stage("github-review-pull-request", None, "reviews");
        let raw = read_state_json(Some(&sf));
        assert_eq!(raw.get("stage").unwrap(), "reviews");
        assert_eq!(raw.get("sub_stage").unwrap(), "github-review-pull-request");
    }

    #[test]
    fn write_bail_file_requires_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        d.write_bail_file("other", "no attempt yet");
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("bail_")));

        let mut fields = Map::new();
        fields.insert("attempt".into(), Value::String("a1".into()));
        d.patch(&[], &fields);
        d.write_bail_file("other", "boom");
        let bail = dir.path().join("bail_a1.json");
        assert!(bail.exists());
        let info = d.read_bail_info().unwrap();
        assert_eq!(info.get("class").unwrap().as_str(), Some("other"));
        assert_eq!(info.get("detail").unwrap().as_str(), Some("boom"));
        assert!(info.get("ts").unwrap().as_str().unwrap().len() > 20);

        // Second write must not clobber the existing bail file.
        d.write_bail_file("security", "second");
        assert_eq!(
            d.read_bail_info().unwrap().get("class"),
            Some(&Value::String("other".into()))
        );
    }

    #[test]
    fn read_bail_info_keeps_non_string_values() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        let mut fields = Map::new();
        fields.insert("attempt".into(), Value::String("a1".into()));
        d.patch(&[], &fields);
        std::fs::write(
            dir.path().join("bail_a1.json"),
            r#"{"class": "other", "detail": "boom", "count": 3, "nested": {"k": [1, null]}}"#,
        )
        .unwrap();
        let info = d.read_bail_info().unwrap();
        assert_eq!(info.get("count"), Some(&Value::from(3)));
        assert_eq!(
            info.get("nested"),
            Some(&serde_json::json!({"k": [1, null]}))
        );

        std::fs::write(dir.path().join("bail_a1.json"), "[1, 2]").unwrap();
        assert!(d.read_bail_info().is_none());
    }

    #[test]
    fn accumulate_token_usage_adds_integers() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        d.accumulate_token_usage(&HashMap::from([("prompt_tokens".to_string(), 5)]));
        d.accumulate_token_usage(&HashMap::from([
            ("prompt_tokens".to_string(), 3),
            ("turns".to_string(), 2),
        ]));
        let raw = read_state_json(Some(&sf));
        let usage = raw.get("token_usage").unwrap().as_object().unwrap();
        assert_eq!(usage.get("prompt_tokens").unwrap(), 8);
        assert_eq!(usage.get("turns").unwrap(), 2);
    }

    #[test]
    fn done_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        assert!(d.done_for("p/seq").is_empty());
        d.mark_done("p/seq", "a");
        d.mark_done("p/seq", "a");
        d.mark_done("p/seq", "b");
        let done = d.done_for("p/seq");
        assert_eq!(done.len(), 2);
        assert!(done.contains("a") && done.contains("b"));
        d.clear_done("p/seq");
        assert!(d.done_for("p/seq").is_empty());
        assert!(!read_state_json(Some(&sf)).contains_key("done_children"));
    }

    #[test]
    fn parallel_worktrees_add_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
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
    }

    #[test]
    fn subprocess_cost_accumulates_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        d.add_subprocess_cost(0.25);
        d.add_subprocess_cost(0.5);
        d.add_subprocess_cost(-1.0);
        d.add_subprocess_cost(f64::NAN);
        d.add_subprocess_cost(0.0);
        let raw = read_state_json(Some(&sf));
        assert_eq!(raw.get("subprocess_cost_usd").unwrap(), 0.75);
    }

    #[test]
    fn terminal_state_touches_finished_and_patches() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        d.write_terminal_state(0);
        assert!(dir.path().join("finished").exists());
        let raw = read_state_json(Some(&sf));
        assert_eq!(raw.get("status").unwrap(), "done");
        assert_eq!(raw.get("exit_code").unwrap(), 0);

        d.write_terminal_state(3);
        let raw = read_state_json(Some(&sf));
        assert_eq!(raw.get("status").unwrap(), "stopped");
        assert_eq!(raw.get("exit_code").unwrap(), 3);
    }

    #[test]
    fn persist_writes_id_and_sets_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = StateData::new(Some("child".into()));
        let mut payload = Map::new();
        payload.insert("pipeline_path".into(), Value::String("/p.yaml".into()));
        d.persist(dir.path(), &payload).unwrap();
        assert_eq!(d.state_file, Some(dir.path().join("state.json")));
        let raw = read_state_json(Some(&dir.path().join("state.json")));
        assert_eq!(raw.get("id").unwrap(), "child");
        assert_eq!(raw.get("pipeline_path").unwrap(), "/p.yaml");

        let mut none = StateData::new(None);
        assert!(none.persist(dir.path(), &Map::new()).is_err());
    }

    #[test]
    fn locked_update_read_modify_write() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        locked_update(&sf, |data| {
            let n = data.get("counter").map(as_i64).unwrap_or(0) + 1;
            data.insert("counter".into(), Value::from(n));
        })
        .unwrap();
        assert_eq!(read_state_json(Some(&sf)).get("counter").unwrap(), 1);
        assert!(dir.path().join("state.json.lock").exists());
    }

    #[test]
    fn parallel_attempt_patch() {
        let dir = tempfile::tempdir().unwrap();
        let sf = seed(dir.path(), "gr-test");
        let d = data_with(&sf);
        d.patch_parallel_attempt("bail-child", "attempt-bail");
        let raw = read_state_json(Some(&sf));
        assert_eq!(raw["parallel_attempts"]["bail-child"], "attempt-bail");
    }
}
