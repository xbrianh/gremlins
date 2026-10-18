//! Per-run on-disk cache for directory landscape builds.
//!
//! A [`LandscapeCache`] pairs each file path with the `(mtime_ns, size, items)`
//! triple recorded when it was last outlined. Files whose `mtime` and `size`
//! are unchanged skip re-parsing on the next build. The whole cache lives in a
//! single `landscape.cache.json` blob, which is replaced via a rename-atomic
//! write so a reader can never observe a half-written file.
//!
//! The on-disk shape mirrors [`crate::landscape::OutlineItem`] but stores item
//! kinds as strings. The shape is deliberately backend-agnostic: once
//! tree-sitter lands behind the same outline interface, the cache format needs
//! no changes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{OutlineItem, CLASS, ENUM, FN, HEADING, IMPL, KEY, METHOD, MOD, STRUCT, TRAIT};

/// A cached outline entry: the file fingerprint plus its parsed items.
#[derive(Debug, Clone, Default)]
pub(crate) struct LandscapeCache {
    entries: BTreeMap<String, Entry>,
    /// Keys visited on the build that produced this cache. On save, entries no
    /// longer present in this set (deleted files) are pruned so the blob does
    /// not grow monotonically for the life of a state dir.
    visited: BTreeSet<String>,
    /// True when an entry was inserted or removed (a real content change),
    /// as opposed to a build that only served cache hits.
    dirty: bool,
}

#[derive(Debug, Clone)]
struct Entry {
    mtime_ns: u64,
    size: u64,
    items: Vec<OutlineItem>,
}

/// Serialization mirror for one cache entry.
#[derive(Debug, Serialize, Deserialize)]
struct RawEntry {
    mtime_ns: u64,
    size: u64,
    items: Vec<RawItem>,
}

/// Serialization mirror for one outline item. `kind` is a string on disk so
/// the cache stays valid even if the in-memory kind constants change.
#[derive(Debug, Serialize, Deserialize)]
struct RawItem {
    kind: String,
    name: String,
    line: usize,
}

impl LandscapeCache {
    /// Load a cache from `path`. A missing or malformed file yields an empty
    /// cache, which the next successful save replaces with a valid one.
    pub(crate) fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let Ok(entries) = serde_json::from_slice::<BTreeMap<String, RawEntry>>(&bytes) else {
            return Self::default();
        };

        let mut cache = Self::default();
        for (key, raw) in entries {
            if let Some(entry) = Entry::from_raw(raw) {
                cache.entries.insert(key, entry);
            }
        }
        cache
    }

    /// Atomically replace the cache file at `path` with the current contents.
    ///
    /// Deleted-file entries are pruned before serializing by keeping only the
    /// keys visited on the last build.
    pub(crate) fn save(&self, path: &Path) -> std::io::Result<()> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("landscape.cache.json");
        let tmp = path.with_file_name(format!("{name}.{}.tmp", unique_suffix()));

        let entries: BTreeMap<String, RawEntry> = self
            .entries
            .iter()
            .filter(|(key, _)| self.visited.contains(key.as_str()))
            .map(|(key, entry)| (key.clone(), Entry::from_entry(entry)))
            .collect();
        let json = serde_json::to_string(&entries)
            .map_err(|err| std::io::Error::other(err.to_string()))?;

        // A failed write or rename can leave the temporary blob behind; remove
        // it so the state dir doesn't accumulate `landscape.cache.json.*.tmp`.
        if let Err(error) = std::fs::write(&tmp, json) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        Ok(())
    }

    /// Whether `save` has anything to do for this build.
    ///
    /// True when an entry was inserted or removed, or when the cache contains
    /// entries for files that were not visited (deleted files that should be
    /// pruned). A build that serves only cache hits skips the write entirely.
    pub(crate) fn needs_save(&self) -> bool {
        self.dirty || self.entries.keys().any(|k| !self.visited.contains(k))
    }

    /// Record that `path` was visited in the current build.
    pub(crate) fn mark_visited(&mut self, path: &Path) {
        self.visited.insert(path_key(path));
    }

    /// Look up a path by its current fingerprint. Returns the cached items on
    /// a hit and `None` on a miss (or if the path is unknown to the cache).
    pub(crate) fn get(&self, path: &Path, mtime_ns: u64, size: u64) -> Option<Vec<OutlineItem>> {
        let entry = self.entries.get(&path_key(path))?;
        if entry.mtime_ns != mtime_ns || entry.size != size {
            return None;
        }
        Some(entry.items.clone())
    }

    /// Remove any cached entry for `path`.
    ///
    /// Called after a failed reparse so a stale entry whose fingerprint later
    /// matches can never resurrect items for content that no longer parses.
    pub(crate) fn remove(&mut self, path: &Path) {
        self.visited.insert(path_key(path));
        self.entries.remove(&path_key(path));
        self.dirty = true;
    }

    /// Record freshly parsed items for `path`, keyed by their fingerprint.
    pub(crate) fn insert(
        &mut self,
        path: &Path,
        mtime_ns: u64,
        size: u64,
        items: Vec<OutlineItem>,
    ) {
        let key = path_key(path);
        self.visited.insert(key.clone());
        self.entries.insert(
            key,
            Entry {
                mtime_ns,
                size,
                items,
            },
        );
        self.dirty = true;
    }
}

impl Entry {
    /// Convert a deserialized entry, dropping it if any item kind is unknown.
    /// A known-shape but unrecognized entry is treated as a cache miss rather
    /// than silently producing wrong `OutlineItem` kinds.
    fn from_raw(raw: RawEntry) -> Option<Self> {
        let items = raw
            .items
            .into_iter()
            .map(RawItem::into_outline)
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            mtime_ns: raw.mtime_ns,
            size: raw.size,
            items,
        })
    }

    fn from_entry(entry: &Entry) -> RawEntry {
        RawEntry {
            mtime_ns: entry.mtime_ns,
            size: entry.size,
            items: entry
                .items
                .iter()
                .map(|item| RawItem {
                    kind: item.kind.to_string(),
                    name: item.name.clone(),
                    line: item.line,
                })
                .collect(),
        }
    }
}

impl RawItem {
    fn into_outline(self) -> Option<OutlineItem> {
        let kind = kind_from_str(&self.kind)?;
        Some(OutlineItem::new(kind, self.name, self.line))
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn kind_from_str(kind: &str) -> Option<&'static str> {
    match kind {
        FN => Some(FN),
        STRUCT => Some(STRUCT),
        IMPL => Some(IMPL),
        TRAIT => Some(TRAIT),
        ENUM => Some(ENUM),
        MOD => Some(MOD),
        CLASS => Some(CLASS),
        METHOD => Some(METHOD),
        HEADING => Some(HEADING),
        KEY => Some(KEY),
        _ => None,
    }
}

/// A collision-resistant suffix for the temporary file written before the
/// rename-atomic replacement of the cache blob.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}.{}", std::process::id(), nanos)
}
