//! Which ssh-agent socket flock's OWN peer dials hand to `ssh` (#418).
//!
//! A long-lived server inherits `SSH_AUTH_SOCK` once, at launch. On macOS that
//! path belongs to the launchd session it was started in, and launchd replaces
//! it across logins; the inherited value then points at a socket nobody
//! listens on. A passphrase-protected key under `BatchMode` is usable ONLY
//! through an agent, so every dial fails auth — for as long as the server
//! lives. hopper ran that way for five days, 2,229 failed dials a day.
//!
//! The fix has one hard constraint: no process spawn per dial. Asking
//! `launchctl getenv SSH_AUTH_SOCK` on every dial is the exec-storm class of
//! #294 that froze clients. So the steady state costs one `connect(2)` on the
//! cached path — a syscall, no process — and only a path that REFUSES the
//! connect triggers a re-resolve: scan the user's own launchd session
//! directories for a `Listeners` socket that accepts, and cache the live one.
//!
//! Linux keeps the inherited value. There is no launchd to ask, and a desktop
//! session's agent is not discoverable the same way — but a dead socket is
//! still DETECTED, so the dial failure reads "agent unreachable" rather than
//! the misleading "auth refused" ssh itself prints.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The agent socket a dial should use, with what `connect(2)` said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentSocket {
    /// No `SSH_AUTH_SOCK` inherited and nothing found: ssh runs without an
    /// agent, which is fine for a key without a passphrase.
    Unset,
    /// A socket that accepted a connection just now.
    Live(PathBuf),
    /// A socket that refused, with no live replacement found. The dial still
    /// gets it (ssh behaves identically either way), but a failure is then
    /// attributed to the agent rather than to the far side.
    Dead(PathBuf),
}

impl AgentSocket {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::Unset => None,
            Self::Live(path) | Self::Dead(path) => Some(path),
        }
    }
}

/// Whether anything is listening on the unix socket at `path`.
///
/// A connect is the whole test: an agent that accepts will answer ssh, and a
/// stale path left behind by a previous launchd session refuses at once.
pub(crate) fn socket_accepts(path: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Resolves and caches the agent socket for this process's peer dials.
///
/// `roots` are the directories holding launchd session dirs
/// (`com.apple.launchd.*/Listeners`); empty means "never re-resolve", which is
/// the Linux policy. Kept as data rather than a `cfg` so the re-resolve path
/// runs under test on every platform against a fixture directory.
pub(crate) struct AgentResolver {
    cached: Mutex<Cached>,
    roots: Vec<PathBuf>,
    uid: Option<u32>,
}

#[derive(Default)]
struct Cached {
    path: Option<PathBuf>,
    /// Whether this dead path has been reported, so a socket that stays dead
    /// is one WARN rather than one per dial.
    dead_reported: bool,
}

impl AgentResolver {
    pub(crate) fn new(inherited: Option<PathBuf>, roots: Vec<PathBuf>, uid: Option<u32>) -> Self {
        Self {
            cached: Mutex::new(Cached {
                path: inherited.filter(|path| !path.as_os_str().is_empty()),
                dead_reported: false,
            }),
            roots,
            uid,
        }
    }

    /// The socket to hand the next dial.
    ///
    /// Live cached path: one connect, done. Otherwise scan once for a live
    /// replacement and cache it, so the NEXT dial is back to one connect. A
    /// scan that finds nothing leaves the dead path cached and is repeated on
    /// the next dial — it is a directory listing and a few connects, never a
    /// process, and it is how a new login's socket is picked up without a
    /// server restart.
    pub(crate) fn resolve(&self) -> AgentSocket {
        let mut cached = match self.cached.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(path) = cached.path.as_ref() {
            if socket_accepts(path) {
                let live = AgentSocket::Live(path.clone());
                cached.dead_reported = false;
                return live;
            }
        }
        if let Some(found) = scan_launchd_listeners(&self.roots, self.uid, cached.path.as_deref()) {
            crate::logging::ssh_agent_resocketed(cached.path.as_deref(), &found);
            *cached = Cached {
                path: Some(found.clone()),
                dead_reported: false,
            };
            return AgentSocket::Live(found);
        }
        let Some(path) = cached.path.clone() else {
            return AgentSocket::Unset;
        };
        if !cached.dead_reported {
            cached.dead_reported = true;
            crate::logging::ssh_agent_unreachable(&path, !self.roots.is_empty());
        }
        AgentSocket::Dead(path)
    }
}

/// The process-wide resolver, seeded from the environment the server was
/// launched with.
fn resolver() -> &'static AgentResolver {
    static RESOLVER: OnceLock<AgentResolver> = OnceLock::new();
    RESOLVER.get_or_init(|| {
        AgentResolver::new(
            std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
            super::ssh_agent_rescan_roots()
                .iter()
                .map(PathBuf::from)
                .collect(),
            current_uid(),
        )
    })
}

/// The agent socket for a peer dial made now. See [`AgentResolver::resolve`].
pub(crate) fn agent_for_dial() -> AgentSocket {
    resolver().resolve()
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    // SAFETY: getuid(2) cannot fail and touches no memory.
    Some(unsafe { libc::getuid() })
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

/// Launchd names each per-login session directory with this prefix.
const LAUNCHD_SESSION_PREFIX: &str = "com.apple.launchd.";

/// Find a launchd `Listeners` socket owned by `uid` that accepts a connection,
/// newest session first, skipping `known_dead`.
///
/// Ownership is checked on both the directory and the socket: another user's
/// agent would not hold this user's keys, and handing ssh a socket someone
/// else controls is exactly what a scan must never do. A `uid` of `None`
/// (a platform with no notion of one) finds nothing.
fn scan_launchd_listeners(
    roots: &[PathBuf],
    uid: Option<u32>,
    known_dead: Option<&Path>,
) -> Option<PathBuf> {
    let uid = uid?;
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let is_session = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(LAUNCHD_SESSION_PREFIX));
            if !is_session {
                continue;
            }
            let dir = entry.path();
            if !owned_by(&dir, uid, EntryKind::Dir) {
                continue;
            }
            let socket = dir.join("Listeners");
            if Some(socket.as_path()) == known_dead || !owned_by(&socket, uid, EntryKind::Socket) {
                continue;
            }
            let modified = std::fs::symlink_metadata(&socket)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            candidates.push((modified, socket));
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    candidates
        .into_iter()
        .map(|(_, socket)| socket)
        .find(|socket| socket_accepts(socket))
}

#[derive(Clone, Copy)]
enum EntryKind {
    Dir,
    Socket,
}

/// Whether `path` is a `kind` owned by `uid`, judged without following a
/// symlink — a link planted in a shared directory must not redirect the scan.
///
/// A session DIRECTORY must also be private (no group or other permission
/// bits, as launchd creates it: 0700). Ownership alone is not enough: in a
/// directory others can write to, someone else could swap the socket out from
/// under the check.
#[cfg(unix)]
fn owned_by(path: &Path, uid: u32, kind: EntryKind) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let kind_ok = match kind {
        EntryKind::Dir => meta.file_type().is_dir() && meta.mode() & PRIVATE_DIR_FORBIDDEN == 0,
        EntryKind::Socket => meta.file_type().is_socket(),
    };
    kind_ok && meta.uid() == uid
}

/// Permission bits a launchd session directory must not have: any group or
/// other access.
#[cfg(unix)]
const PRIVATE_DIR_FORBIDDEN: u32 = 0o077;

#[cfg(not(unix))]
fn owned_by(_path: &Path, _uid: u32, _kind: EntryKind) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A short fixture root under /tmp, not `temp_dir()`: unix socket paths
    /// are capped near 104 bytes on macOS, and `nix develop`'s TMPDIR segment
    /// alone blows that (same reason as `api::server`'s socket test).
    fn fixture_root(name: &str) -> PathBuf {
        let root = PathBuf::from("/tmp").join(format!("f418-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture root");
        root
    }

    /// A socket file nobody listens on: bind, then drop the listener. The
    /// path stays behind, exactly like a previous login's launchd socket.
    fn dead_socket(path: &Path) {
        drop(UnixListener::bind(path).expect("bind dead socket"));
    }

    /// A session dir shaped like launchd's: private to the user (0700).
    fn session_dir(root: &Path, id: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = root.join(format!("{LAUNCHD_SESSION_PREFIX}{id}"));
        std::fs::create_dir_all(&dir).expect("session dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        dir.join("Listeners")
    }

    fn uid() -> Option<u32> {
        current_uid()
    }

    #[test]
    fn a_live_cached_socket_is_used_without_a_scan() {
        let root = fixture_root("live");
        let path = root.join("agent.sock");
        let _listener = UnixListener::bind(&path).expect("bind");
        // The scan root holds a DIFFERENT live socket: returning the cached
        // one proves the steady state never looked at the directory.
        let other = session_dir(&root, "other");
        let _other = UnixListener::bind(&other).expect("bind other");
        let resolver = AgentResolver::new(Some(path.clone()), vec![root.clone()], uid());
        assert_eq!(resolver.resolve(), AgentSocket::Live(path));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The hopper outage: the inherited socket is dead, the current login's
    /// is alive in the launchd directory. One scan finds it, and the cache
    /// means every later dial is back to a single connect on the new path.
    #[test]
    fn a_dead_socket_is_replaced_by_the_live_launchd_listener_and_cached() {
        let root = fixture_root("resocket");
        let stale = session_dir(&root, "old");
        dead_socket(&stale);
        let current = session_dir(&root, "new");
        let listener = UnixListener::bind(&current).expect("bind current");

        let resolver = AgentResolver::new(Some(stale.clone()), vec![root.clone()], uid());
        assert!(
            !socket_accepts(&stale),
            "fixture: the old socket must refuse"
        );
        assert_eq!(resolver.resolve(), AgentSocket::Live(current.clone()));

        // Cached: remove the session dir from the scan root entirely, and the
        // resolver still answers with the path it adopted.
        let resolver_roots_gone = {
            let cached = resolver.cached.lock().expect("lock").path.clone();
            AgentResolver::new(cached, Vec::new(), uid())
        };
        assert_eq!(
            resolver_roots_gone.resolve(),
            AgentSocket::Live(current.clone())
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Linux policy: no scan roots. A dead socket is REPORTED dead rather
    /// than silently passed along as if it were fine.
    #[test]
    fn with_no_scan_roots_a_dead_socket_is_reported_dead() {
        let root = fixture_root("linux");
        let stale = root.join("agent.sock");
        dead_socket(&stale);
        let resolver = AgentResolver::new(Some(stale.clone()), Vec::new(), uid());
        assert_eq!(resolver.resolve(), AgentSocket::Dead(stale));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_inherited_and_nothing_found_is_unset() {
        let root = fixture_root("unset");
        let resolver = AgentResolver::new(None, vec![root.clone()], uid());
        assert_eq!(resolver.resolve(), AgentSocket::Unset);
        // An empty inherited value is the same as none.
        let empty = AgentResolver::new(Some(PathBuf::new()), Vec::new(), uid());
        assert_eq!(empty.resolve(), AgentSocket::Unset);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only launchd session dirs are candidates, and a symlinked session dir
    /// is never followed — a link in a shared directory must not be able to
    /// point this user's ssh at a socket someone else placed.
    #[test]
    fn the_scan_ignores_foreign_names_and_symlinks() {
        let root = fixture_root("strict");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("dir");
        let live = elsewhere.join("Listeners");
        let _listener = UnixListener::bind(&live).expect("bind");
        std::os::unix::fs::symlink(&elsewhere, root.join(format!("{LAUNCHD_SESSION_PREFIX}ln")))
            .expect("symlink");
        let not_launchd = root.join("some.other.dir");
        std::fs::create_dir_all(&not_launchd).expect("dir");
        let _other = UnixListener::bind(not_launchd.join("Listeners")).expect("bind");

        let resolver = AgentResolver::new(None, vec![root.clone()], uid());
        assert_eq!(resolver.resolve(), AgentSocket::Unset);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #418 review: a session dir others can write to is not trusted, even
    /// when this user owns it and the socket in it answers.
    #[test]
    fn a_session_dir_open_to_others_is_never_adopted() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture_root("mode");
        let current = session_dir(&root, "s");
        let _listener = UnixListener::bind(&current).expect("bind");
        let dir = current.parent().expect("session dir").to_path_buf();
        for mode in [0o770, 0o707, 0o755] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let resolver = AgentResolver::new(None, vec![root.clone()], uid());
            assert_eq!(resolver.resolve(), AgentSocket::Unset, "mode {mode:o}");
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let resolver = AgentResolver::new(None, vec![root.clone()], uid());
        assert_eq!(resolver.resolve(), AgentSocket::Live(current));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_socket_owned_by_another_uid_is_never_adopted() {
        let root = fixture_root("uid");
        let current = session_dir(&root, "s");
        let _listener = UnixListener::bind(&current).expect("bind");
        let someone_else = uid().map(|uid| uid.wrapping_add(1));
        let resolver = AgentResolver::new(None, vec![root.clone()], someone_else);
        assert_eq!(resolver.resolve(), AgentSocket::Unset);
        let _ = std::fs::remove_dir_all(&root);
    }
}
