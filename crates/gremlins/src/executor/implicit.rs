//! Harness-managed implicit artifacts.
//!
//! An implicit artifact is an artifact the harness creates, manages, and
//! injects into prompts without being declared in the pipeline YAML. The
//! scratchpad is the first (and currently only) implicit artifact kind, but
//! the machinery is generalized so additional kinds can be added without
//! further special-casing.

use std::io;
use std::path::{Path, PathBuf};

use crate::artifacts::uri::Uri;
use crate::executor::state::StateStore;

// ---------------------------------------------------------------------------
// ImplicitArtifact — a harness-created, harness-managed artifact
// ---------------------------------------------------------------------------

/// A harness-managed implicit artifact.
///
/// | Aspect            | Value                             |
/// |-------------------|-----------------------------------|
/// | URI scheme        | `artifact://_{kind}/{stage}.md`   |
/// | Model-writable    | `{artifact_dir}/_{kind}.md`       |
/// | Persist           | `{artifact_dir}/_{kind}/{stage}.md` |
/// | Bind key          | `__gml_{kind}`                    |
/// | Optional          | always true                       |
#[derive(Debug)]
pub(crate) struct ImplicitArtifact {
    /// The kind name, e.g. `"scratchpad"`.
    pub(crate) kind: String,
    /// The synthetic artifact URI, e.g. `"artifact://_scratchpad/stage-a.md"`.
    pub(crate) uri: String,
    /// Absolute path the model writes to (under the checkout artifact dir).
    pub(crate) model_path: PathBuf,
    /// Absolute path the harness writes the persisted copy to (under the same
    /// checkout artifact dir). Registration in the artifact store is handled
    /// by the bind loop in [`commit_agent`](super::agent_runner::commit_agent).
    pub(crate) persist_path: String,
    /// The synthetic bind key registered in `output_paths` / `output_uris`.
    pub(crate) bind_key: String,
    /// Always `true` for implicit artifacts.
    pub(crate) optional: bool,
}

impl ImplicitArtifact {
    /// Create an implicit artifact for the given kind.
    ///
    /// The model-writable path is derived from `artifact_dir` (not `cwd`), so
    /// no scratchpad file is ever written into the gremlin's working directory.
    /// The persist path is resolved from the synthetic URI via
    /// `local_state.path_for_uri()`.
    pub(crate) async fn for_kind(
        kind: &str,
        stage_name: &str,
        loop_iter: &str,
        artifact_dir: &Path,
        local_state: &dyn StateStore,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Reject stage names that could escape the _kind directory.
        if stage_name.contains('/') || stage_name.contains('\\') || stage_name.contains("..") {
            return Err(Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "stage name {stage_name:?} contains path separators or '..' — \
                     cannot construct a safe implicit artifact URI"
                ),
            )));
        }
        let name = if loop_iter.is_empty() {
            format!("{stage_name}.md")
        } else {
            format!("{stage_name}-{loop_iter}.md")
        };
        let uri_str = format!("artifact://_{kind}/{name}");
        let uri = Uri::parse(&uri_str)?;
        let persist_path = local_state.path_for_uri(&uri).await?;
        let model_path = artifact_dir.join(format!("_{kind}.md"));
        Ok(Self {
            kind: kind.to_string(),
            uri: uri_str,
            model_path,
            persist_path,
            bind_key: format!("__gml_{kind}"),
            optional: true,
        })
    }

    /// Persist the artifact: read the model-writable file with containment
    /// checks (`O_NOFOLLOW` plus root-isolation), then copy it to the persist
    /// path. Absent or empty files are silently skipped.
    ///
    /// Registration in the artifact store is the caller's responsibility —
    /// the bind loop in [`commit_agent`](super::agent_runner::commit_agent)
    /// calls `local_state.commit()` for every output URI (including implicit
    /// ones) after the file has been copied.
    pub(crate) async fn persist(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let root = self.model_path.parent().unwrap_or_else(|| Path::new("."));
        let content = match super::agent_runner::read_scratchpad_contained(&self.model_path, root) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Box::new(e)),
        };
        if content.trim().is_empty() {
            return Ok(());
        }
        if let Some(parent) = Path::new(&self.persist_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&self.persist_path, &content).await?;
        Ok(())
    }

    /// Extract the stage name from an implicit artifact key of the given kind.
    /// Returns `None` when the key does not match.
    ///
    /// Keys are formatted as `artifact://_{kind}/{name}.md` or
    /// `artifact://_{kind}/{name}-{loop_iter}.md`.
    pub(crate) fn stage_name_from_key<'a>(key: &'a str, kind: &str) -> Option<&'a str> {
        let prefix = format!("artifact://_{kind}/");
        let rest = key
            .strip_prefix(&prefix)
            .or_else(|| key.strip_prefix(&format!("_{kind}/")))?;
        let rest = rest.strip_suffix(".md").unwrap_or(rest);
        // The stage name is everything before the first `-` (which separates
        // name from loop_iter), or the entire remainder.
        Some(rest.split('-').next().unwrap_or(rest))
    }

    /// The `{scratchpad_path}` placeholder value — the absolute model-writable
    /// path rendered for substitution into the system prompt.
    pub(crate) fn placeholder_path(&self) -> String {
        self.model_path.to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// Accumulation
// ---------------------------------------------------------------------------

/// Concatenate every persisted implicit artifact of the given kind from
/// `store`, in the order given by `stage_order`. Keys whose stage name does
/// not appear in `stage_order` are appended after the ordered entries in
/// lexical order. Empty artifacts are skipped.
pub(crate) async fn accumulate_implicit(
    kind: &str,
    store: &(dyn StateStore + Send + Sync),
    stage_order: &[String],
) -> String {
    let prefix = format!("artifact://_{kind}/");
    let alt_prefix = format!("_{kind}/");
    let mut keys: Vec<String> = store
        .keys()
        .await
        .into_iter()
        .filter(|k| k.starts_with(&prefix) || k.starts_with(&alt_prefix))
        .collect();
    keys.sort_by(|a, b| {
        let a_idx = stage_order
            .iter()
            .position(|name| ImplicitArtifact::stage_name_from_key(a, kind) == Some(name));
        let b_idx = stage_order
            .iter()
            .position(|name| ImplicitArtifact::stage_name_from_key(b, kind) == Some(name));
        match (a_idx, b_idx) {
            (Some(ai), Some(bi)) => ai.cmp(&bi),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.cmp(b),
        }
    });
    let mut accumulated = String::new();
    for key in &keys {
        if let Ok(content) = store.content(key, None).await {
            if !content.trim().is_empty() {
                accumulated.push_str(content.trim_end());
                accumulated.push_str("\n\n");
            }
        }
    }
    accumulated.trim().to_string()
}

// ---------------------------------------------------------------------------
// Flush live artifact to main state store (error path)
// ---------------------------------------------------------------------------

/// Best-effort flush of a live model-writable artifact into `main_state`
/// under its artifact URI.
///
/// Called from the agent error path (cancelled / failed turns), where the
/// stage checkout is dropped without ever being joined to the main store.
/// This ensures in-flight working memory is not lost before the supervisor
/// runs session-end persistence.
pub(crate) async fn flush_live(
    artifact: &ImplicitArtifact,
    main_state: &(dyn StateStore + Send + Sync),
) {
    let root = match artifact.model_path.parent() {
        Some(p) => p,
        None => return,
    };
    let content = match super::agent_runner::read_scratchpad_contained(&artifact.model_path, root) {
        Ok(c) => c,
        Err(_) => return,
    };
    if content.trim().is_empty() {
        return;
    }
    let uri = match Uri::parse(&artifact.uri) {
        Ok(uri) => uri,
        Err(_) => return,
    };
    let persist = match main_state.path_for_uri(&uri).await {
        Ok(p) => p,
        Err(_) => return,
    };
    if let Some(parent) = Path::new(&persist).parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Err(e) = tokio::fs::write(&persist, &content).await {
        log::warn!("flush_live: failed to write {}: {e}", persist);
        return;
    }
    let _ = main_state.commit(&artifact.uri, &persist).await;
}
