//! Pending-output scopes: a trusted, in-process way for the executor to let
//! one producer invocation ask whether its *declared but not yet registered*
//! outputs exist.
//!
//! A scope is opened with the exact URI/destination pairs the executor
//! resolved for the invocation and is identified by an opaque random token.
//! Scopes live only in executor memory and disappear when their guard drops.

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::executor::state::secure_token_hex;

/// Deliberately vague: scope failures must not disclose filesystem details.
const SCOPE_UNAVAILABLE: &str = "pending-output scope is unavailable";
const SCOPE_REJECTED: &str = "invalid or expired pending-output scope";

struct Scope {
    run_id: String,
    /// Checkout root descriptor opened when the scope was created, so later
    /// path swaps (including in ancestors) cannot redirect probes.
    root: OwnedFd,
    /// Declared destinations, relative to the checkout root.
    outputs: HashMap<String, PathBuf>,
}

/// Shared table of live scopes, keyed by token.
#[derive(Clone, Default)]
pub(crate) struct PendingScopes(Arc<Mutex<HashMap<String, Arc<Scope>>>>);

/// Lifetime guard of one scope; dropping it revokes the token.
#[must_use = "dropping the guard revokes the pending-output scope"]
pub(crate) struct PendingScopeGuard {
    token: String,
    scopes: PendingScopes,
}

impl PendingScopeGuard {
    /// The secret token; hand it only to the invocation's check subprocess.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

impl std::fmt::Debug for PendingScopeGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingScopeGuard").finish_non_exhaustive()
    }
}

impl Drop for PendingScopeGuard {
    fn drop(&mut self) {
        self.scopes.lock().remove(&self.token);
    }
}

impl PendingScopes {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Scope>>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Open a fresh scope for one invocation of `run_id`.
    pub(crate) fn open(
        &self,
        run_id: &str,
        checkout_root: PathBuf,
        outputs: impl IntoIterator<Item = (String, PathBuf)>,
    ) -> io::Result<PendingScopeGuard> {
        let root = open_root(&checkout_root)?;
        let mut destinations = HashMap::new();
        for (uri, dest) in outputs {
            let invalid = || io::Error::from(io::ErrorKind::InvalidInput);
            let rel = dest.strip_prefix(&checkout_root).map_err(|_| invalid())?;
            if rel.as_os_str().is_empty()
                || !rel.components().all(|c| matches!(c, Component::Normal(_)))
            {
                return Err(invalid());
            }
            if let Some(previous) = destinations.insert(uri, rel.to_path_buf()) {
                if previous != rel {
                    return Err(invalid());
                }
            }
        }
        let scope = Arc::new(Scope {
            run_id: run_id.to_string(),
            root,
            outputs: destinations,
        });
        let token = loop {
            let candidate = secure_token_hex(32)?;
            if let std::collections::hash_map::Entry::Vacant(slot) =
                self.lock().entry(candidate.clone())
            {
                slot.insert(scope);
                break candidate;
            }
        };
        Ok(PendingScopeGuard {
            token,
            scopes: self.clone(),
        })
    }

    /// The subset of `uris` whose declared destination holds no regular file,
    /// in request order. Consults only the scope's own destinations.
    pub(crate) fn missing(
        &self,
        run_id: &str,
        token: &str,
        uris: &[String],
    ) -> Result<Vec<String>, String> {
        let scope = self
            .lock()
            .get(token)
            .filter(|scope| scope.run_id == run_id)
            .cloned()
            .ok_or_else(|| SCOPE_REJECTED.to_string())?;

        let mut missing = Vec::new();
        for uri in uris {
            let dest = scope
                .outputs
                .get(uri)
                .ok_or_else(|| format!("artifact {uri:?} is not declared in this scope"))?;
            let present =
                present_within(&scope.root, dest).map_err(|_| SCOPE_UNAVAILABLE.to_string())?;
            if !present {
                missing.push(uri.clone());
            }
        }
        Ok(missing)
    }
}

/// Open the checkout root directory once, at scope creation.
fn open_root(root: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let root_c = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    // SAFETY: `root_c` is a valid NUL-terminated path.
    let raw = unsafe { libc::open(root_c.as_ptr(), flags) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` was just returned by a successful `open`.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Whether the relative `rel` is a regular file beneath the `root` descriptor.
///
/// Walks one component at a time with `openat` and `O_NOFOLLOW`, so a
/// component swapped for a symlink can neither redirect the probe nor race
/// between a check and use. Inspect the leaf without opening it: read
/// permission is irrelevant, and a declared output might be a device or FIFO.
fn present_within(root: &OwnedFd, rel: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;

    // `open` validated every component before publishing the scope.
    let mut components = rel.iter();
    let leaf = components
        .next_back()
        .expect("validated nonempty destination");
    let mut dir = root.try_clone()?;
    for component in components {
        let name = CString::new(component.as_bytes())?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: `dir` is a live directory and `name` is one NUL-terminated component.
        let raw = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if raw < 0 {
            return absent_or_error(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a newly opened descriptor.
        dir = unsafe { OwnedFd::from_raw_fd(raw) };
    }

    let name = CString::new(leaf.as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `dir` and `name` are valid and `stat` is writable.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        return absent_or_error(io::Error::last_os_error());
    }
    // SAFETY: successful `fstatat` initialized `stat`.
    match unsafe { stat.assume_init() }.st_mode & libc::S_IFMT {
        libc::S_IFLNK => Err(io::ErrorKind::PermissionDenied.into()),
        kind => Ok(kind == libc::S_IFREG),
    }
}

fn absent_or_error(error: io::Error) -> io::Result<bool> {
    if error.kind() == io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri() -> String {
        "artifact://out.txt".to_string()
    }

    #[test]
    fn scopes_are_isolated_and_revoked_on_drop() {
        let scopes = PendingScopes::default();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let dest_a = a.path().join("out.txt");
        std::fs::write(&dest_a, "").unwrap();
        let guard_a = scopes
            .open("r", a.path().to_path_buf(), [(uri(), dest_a)])
            .unwrap();
        let guard_b = scopes
            .open(
                "r",
                b.path().to_path_buf(),
                [(uri(), b.path().join("out.txt"))],
            )
            .unwrap();

        let ask = |g: &PendingScopeGuard, run: &str| scopes.missing(run, g.token(), &[uri()]);
        assert_eq!(ask(&guard_a, "r").unwrap(), Vec::<String>::new());
        assert_eq!(ask(&guard_b, "r").unwrap(), vec![uri()]);
        assert!(ask(&guard_a, "other").is_err());
        assert!(scopes.missing("r", "guess", &[uri()]).is_err());
        assert!(scopes
            .missing("r", guard_a.token(), &["artifact://x".to_string()])
            .is_err());

        let token_a = guard_a.token().to_string();
        drop(guard_a);
        assert!(scopes.missing("r", &token_a, &[uri()]).is_err());
        let retry = scopes
            .open("r", a.path().into(), [(uri(), a.path().join("out.txt"))])
            .unwrap();
        assert_ne!(token_a, retry.token());
        assert!(scopes.missing("r", &token_a, &[uri()]).is_err());
        assert!(ask(&guard_b, "r").is_ok());
    }

    #[test]
    fn invalid_declarations_do_not_issue_a_token() {
        let scopes = PendingScopes::default();
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        for dest in [
            other.path().join("out"),
            root.path().join("../out"),
            root.path().into(),
        ] {
            assert!(scopes
                .open("r", root.path().into(), [(uri(), dest)])
                .is_err());
        }
        let link = root.path().join("checkout");
        std::os::unix::fs::symlink(other.path(), &link).unwrap();
        assert!(scopes
            .open("r", link.clone(), [(uri(), link.join("out"))])
            .is_err());
        assert!(scopes.lock().is_empty());
    }

    #[test]
    fn metadata_probe_does_not_open_the_output() {
        let scopes = PendingScopes::default();
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("marker");
        let guard = scopes
            .open("r", root.path().into(), [(uri(), output.clone())])
            .unwrap();
        std::fs::write(&output, "").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o0)).unwrap();
        assert!(scopes
            .missing("r", guard.token(), &[uri()])
            .unwrap()
            .is_empty());
        std::fs::remove_file(&output).unwrap();
        let _socket = std::os::unix::net::UnixDatagram::bind(&output).unwrap();
        assert_eq!(
            scopes.missing("r", guard.token(), &[uri()]).unwrap(),
            [uri()]
        );
    }

    #[test]
    fn replacing_root_with_symlink_does_not_redirect() {
        let scopes = PendingScopes::default();
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("out.txt"), "").unwrap();
        let root = base.path().join("co");
        std::fs::create_dir(&root).unwrap();
        let guard = scopes
            .open("r", root.clone(), [(uri(), root.join("out.txt"))])
            .unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        let reply = scopes.missing("r", guard.token(), &[uri()]);
        assert!(!matches!(reply, Ok(missing) if missing.is_empty()));
    }

    #[test]
    fn symlink_escape_is_an_error() {
        let scopes = PendingScopes::default();
        let (root, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(outside.path().join("out.txt"), "").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("d")).unwrap();
        let guard = scopes
            .open(
                "r",
                root.path().to_path_buf(),
                [(uri(), root.path().join("d/out.txt"))],
            )
            .unwrap();
        assert!(scopes.missing("r", guard.token(), &[uri()]).is_err());
    }

    #[test]
    fn symlinked_leaf_is_an_error_not_present() {
        let scopes = PendingScopes::default();
        let (root, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(outside.path().join("t"), "").unwrap();
        std::os::unix::fs::symlink(outside.path().join("t"), root.path().join("out.txt")).unwrap();
        let guard = scopes
            .open(
                "r",
                root.path().to_path_buf(),
                [(uri(), root.path().join("out.txt"))],
            )
            .unwrap();
        assert!(scopes.missing("r", guard.token(), &[uri()]).is_err());
    }

    #[test]
    fn directory_and_missing_parent_are_missing() {
        let scopes = PendingScopes::default();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        let other = "artifact://n/x".to_string();
        let guard = scopes
            .open(
                "r",
                root.path().to_path_buf(),
                [
                    ("artifact://d".to_string(), root.path().join("d")),
                    (other.clone(), root.path().join("n/x")),
                ],
            )
            .unwrap();
        let asked = ["artifact://d".to_string(), other];
        assert_eq!(scopes.missing("r", guard.token(), &asked).unwrap(), asked);
    }

    #[tokio::test]
    async fn cancelled_owner_revokes_token() {
        let scopes = PendingScopes::default();
        let root = tempfile::tempdir().unwrap();
        let guard = scopes
            .open(
                "r",
                root.path().to_path_buf(),
                [(uri(), root.path().join("o"))],
            )
            .unwrap();
        let token = guard.token().to_string();
        let task = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        assert!(scopes.missing("r", &token, &[uri()]).is_err());
    }
}
