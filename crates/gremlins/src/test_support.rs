//! Test-only scaffolding for the state gremlins keeps per process.
//!
//! The path resolvers read `GREMLINS_SANDBOX_ROOT`, `GREMLINS_PROJECT_ROOT`
//! and `GREMLINS_OVERLAY_DIR` straight from the environment, and cache the
//! parsed `config.json` in a process-global, so a test that changes either one
//! is racing every other test that resolves a path. One lock, taken by every
//! module that touches that state, turns the race into an ordering.
//!
//! [`EnvGuard`] owns that lock together with an undo log of the variables its
//! holder touched, so the environment is put back when the guard drops rather
//! than by a `clear_sandbox_env()` at the end of every test — a line that is
//! easy to forget on a new test and to misplace when one panics. [`Sandbox`]
//! and [`with_sandbox`] are the common case: a throwaway sandbox root wired
//! into `GREMLINS_SANDBOX_ROOT`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::config;

/// Environment variables the path resolvers read directly.
const OVERRIDES: [&str; 3] = [
    "GREMLINS_SANDBOX_ROOT",
    "GREMLINS_PROJECT_ROOT",
    "GREMLINS_OVERLAY_DIR",
];

/// Holds the process-state lock and undoes the environment changes its holder
/// made, newest first, when it drops.
///
/// The lock is the one thing every test that resolves a path or parses a
/// config has to share: `config::GLOBAL_CONFIG` caches a `config.json` that was
/// read against whatever `GREMLINS_SANDBOX_ROOT` said at the time, so two tests
/// swapping that variable must not overlap — in any module, not just the same
/// file.
///
/// Poisoning is ignored on purpose: a panicking holder still unwinds through
/// this `Drop`, so the environment is restored even when a test fails.
pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    undo: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    /// Take the lock, start from a clean slate, and keep it until drop.
    pub(crate) fn lock() -> Self {
        static LOCK: Mutex<()> = Mutex::new(());
        let mut guard = EnvGuard {
            _lock: LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            undo: Vec::new(),
        };
        for key in OVERRIDES {
            guard.remove(key);
        }
        config::clear_global();
        guard
    }

    pub(crate) fn set(&mut self, key: &'static str, value: impl AsRef<OsStr>) {
        self.remember(key);
        std::env::set_var(key, value);
    }

    pub(crate) fn remove(&mut self, key: &'static str) {
        self.remember(key);
        std::env::remove_var(key);
    }

    /// Record a variable's pre-test value once, however often it is rewritten.
    fn remember(&mut self, key: &'static str) {
        if !self.undo.iter().any(|(seen, _)| *seen == key) {
            self.undo.push((key, std::env::var_os(key)));
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.undo.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        // The cache is keyed to a root that no longer exists.
        config::clear_global();
    }
}

/// A throwaway `GREMLINS_SANDBOX_ROOT`, live for as long as the value is.
///
/// The lock is held until the sandbox drops: the root and the rule that no
/// other test may observe it are the same thing.
pub(crate) struct Sandbox {
    _env: EnvGuard,
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A sandbox with no `config.json`, so client and stage lookups see the
    /// empty config a fresh machine would.
    pub(crate) fn new() -> Self {
        Self::with_config(None)
    }

    /// A sandbox whose `config/config.json` holds `config_json`.
    pub(crate) fn with_config(config_json: Option<&str>) -> Self {
        Sandbox::with_config_file("config.json", config_json)
    }

    /// A sandbox whose `config/providers.json` holds `json` — the file
    /// [`crate::config::ApiKeys`] reads API keys from.
    pub(crate) fn with_providers(json: &str) -> Self {
        Sandbox::with_config_file("providers.json", Some(json))
    }

    fn with_config_file(name: &str, contents: Option<&str>) -> Self {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        if let Some(contents) = contents {
            let config_dir = dir.path().join("config");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::write(config_dir.join(name), contents).unwrap();
        }
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());

        Sandbox { _env: env, dir }
    }

    /// The sandbox root: `$GREMLINS_SANDBOX_ROOT` for the duration.
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A path inside the sandbox, for asserting where a run put its files.
    pub(crate) fn join(&self, sub: impl AsRef<Path>) -> PathBuf {
        self.dir.path().join(sub)
    }
}

/// Run `body` against a sandbox root that is gone — and an environment that is
/// restored — by the time it returns.
pub(crate) fn with_sandbox<T>(config_json: Option<&str>, body: impl FnOnce(&Sandbox) -> T) -> T {
    let sandbox = Sandbox::with_config(config_json);
    body(&sandbox)
}
