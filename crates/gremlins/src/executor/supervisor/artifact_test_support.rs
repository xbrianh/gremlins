//! In-process supervisor for CLI acceptance tests that need a trusted scope owner.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{accept_loop, socket, GremlinsDaemonLock, RunRegistry};

#[doc(hidden)]
pub struct ArtifactTestSupervisor {
    registry: Arc<RunRegistry>,
    task: tokio::task::JoinHandle<()>,
    state_root: PathBuf,
    _lock: GremlinsDaemonLock,
}

impl ArtifactTestSupervisor {
    pub fn start() -> Self {
        let state_root = crate::config::state_root();
        std::fs::create_dir_all(&state_root).unwrap();
        let lock = GremlinsDaemonLock::try_acquire(&state_root)
            .unwrap()
            .expect("test supervisor requires an isolated state root");
        let listener = socket::bind_socket(&state_root).unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let registry = Arc::new(RunRegistry::new(tx));
        // Keep the supervisor alive between one-shot CLI commands.
        registry.connection_up();
        let task = tokio::spawn(accept_loop(
            listener,
            state_root.clone(),
            registry.clone(),
            rx,
        ));
        Self {
            registry,
            task,
            state_root,
            _lock: lock,
        }
    }

    pub fn open(
        &self,
        id: &str,
        root: &Path,
        outputs: Vec<(String, PathBuf)>,
    ) -> (String, impl Send + use<>) {
        let guard = self
            .registry
            .open_pending_scope(id, root.to_owned(), outputs)
            .unwrap();
        (guard.token().to_owned(), guard)
    }
}

impl Drop for ArtifactTestSupervisor {
    fn drop(&mut self) {
        self.task.abort();
        socket::unlink_socket(&self.state_root);
    }
}
