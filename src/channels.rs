//! External channel bindings (ADR-0028 §8, #1037): binding config, the
//! capability token per binding, and the `channel.hello` check.
//!
//! A binding is a `[[channels]]` entry in config. The node named in its
//! `owner_node` issues one capability token for it into
//! `<data dir>/channel-tokens/<name>.token` at startup and on every config
//! reload, and an adapter proves it may serve the binding by presenting that
//! token in `channel.hello`. The token never reaches a log, an event or a
//! response: the server compares SHA-256 digests in constant time and logs
//! only the binding name and the refusal reason.
//!
//! A token file is trusted only when it is a regular file owned by this user
//! with mode 0600 holding 64 lowercase hex digits. Issuing writes a temp file
//! in the same directory and fsyncs it, so a crash never leaves a partial
//! token behind a valid name. A first issue hard-links the temp into place,
//! which fails rather than overwrites when another issuer got there first. A
//! file that fails the check (a symlink, unreadable, empty, malformed, too
//! open, someone else's) is replaced on the next startup or reload by renaming
//! the temp over it, so the name is never missing, and the reason is logged.
//! Any other I/O error leaves the file untouched and is reported as
//! `channel_token_unavailable`.
//!
//! A temp file is named `.<pid>-<random>.tmp` after the process that wrote it.
//! Each issue sweeps the temp files whose writer is gone, the leftovers of a
//! crash, and keeps those of a live process: during a live handoff two
//! servers issue into the same directory at once (#1170). A recycled pid only
//! delays a sweep until that process exits too. A temp named without a pid
//! (`.<random>.tmp`, written before #1170) is swept at once, as it was then:
//! only an issuer of that older release, running during the one handoff that
//! upgrades from it, can still be writing one. The directory is
//! tightened to 0700 when this user owns it and refused otherwise.
//!
//! Known limits. Two issuers that find the same untrusted file both rename
//! over it, so the second replaces the first's fresh token, which a reader
//! may already hold. The pid check assumes every issuer shares this host's
//! pid namespace: a data dir on a network share or a server in a container
//! can sweep a live issuer's temp, which then logs
//! `channel.token.issue_failed` until the next reload. On a platform without
//! a process probe (`platform::fallback`) every pid counts as gone, so every
//! temp is swept as before #1170. The file is
//! read on every check, so deleting it revokes the binding at once, and a
//! reload then issues a new token.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One `[[channels]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelBinding {
    /// `[A-Za-z0-9_-]`, at most 64 bytes, unique: it names the token file and
    /// the inbound sender `channel:<name>/<external user>`.
    pub name: String,
    /// The node that serves this binding (ADR-0028 §8: one owner, no
    /// failover). Compared with this node's name (`name` in config, else
    /// the host name).
    pub owner_node: String,
    pub target: ChannelTarget,
    #[serde(default)]
    pub scopes: Vec<ChannelScope>,
}

/// What a binding is about: `"fleet"`, `"repo:<name>"` or `"agent:<id>"`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub enum ChannelTarget {
    Fleet,
    Repo(String),
    Agent(String),
}

impl TryFrom<String> for ChannelTarget {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, String> {
        match raw.split_once(':') {
            None if raw == "fleet" => Ok(Self::Fleet),
            Some(("repo", name)) if !name.is_empty() => Ok(Self::Repo(name.to_owned())),
            Some(("agent", id)) if !id.is_empty() => Ok(Self::Agent(id.to_owned())),
            _ => Err(format!(
                "invalid channel target {raw:?}: expected \"fleet\", \"repo:<name>\" or \"agent:<id>\""
            )),
        }
    }
}

impl From<ChannelTarget> for String {
    fn from(target: ChannelTarget) -> Self {
        match target {
            ChannelTarget::Fleet => "fleet".to_owned(),
            ChannelTarget::Repo(name) => format!("repo:{name}"),
            ChannelTarget::Agent(id) => format!("agent:{id}"),
        }
    }
}

/// What a binding may do (ADR-0028 §8). This slice only reports them in
/// `channel.hello`; the inbound methods that enforce them are #1153.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelScope {
    Notify,
    SendToAgent,
    Reply,
}

/// A capability token as it travels in a request. Its `Debug` never prints
/// the value, so a logged request cannot leak it.
#[derive(Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ChannelToken(pub String);

impl fmt::Debug for ChannelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChannelToken(<redacted>)")
    }
}

/// Drop `[[channels]]` entries with an unusable or duplicate name, keeping
/// the first; each drop is a diagnostic.
pub(crate) fn validate(bindings: &mut Vec<ChannelBinding>, diagnostics: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    bindings.retain(|binding| {
        let name = &binding.name;
        let usable = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        let problem = if !usable {
            "a name of 1-64 characters from [A-Za-z0-9_-]"
        } else if binding.owner_node.trim().is_empty() {
            "an owner_node"
        } else if !seen.insert(name.clone()) {
            "a unique name"
        } else {
            return true;
        };
        diagnostics.push(format!(
            "invalid [[channels]] entry {name:?}: needs {problem}; entry ignored"
        ));
        false
    });
}

pub(crate) fn token_path(name: &str) -> PathBuf {
    crate::session::data_dir()
        .join("channel-tokens")
        .join(format!("{name}.token"))
}

/// Issue a token for every binding `node` owns that has no valid one yet. A
/// failure is logged and leaves that binding refusing every hello.
pub(crate) fn issue_tokens(bindings: &[ChannelBinding], node: &str) {
    for binding in bindings.iter().filter(|b| b.owner_node == node) {
        match issue_token(
            &token_path(&binding.name),
            read_token,
            crate::platform::process_exists,
        ) {
            Ok(Some(reason)) => crate::logging::channel_token_reissued(&binding.name, reason),
            Ok(None) => {}
            Err(err) => {
                crate::logging::channel_token_issue_failed(&binding.name, &err.to_string());
            }
        }
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no arguments or memory preconditions.
    unsafe { libc::geteuid() }
}

/// Why a token file gave no token.
#[derive(Debug)]
pub(crate) enum TokenFault {
    Missing,
    /// The file must not be trusted (`symlink`, `unreadable`, `not_private`,
    /// `malformed`), and is replaced at the next issue.
    Untrusted(&'static str),
    /// Any other I/O failure (EMFILE, EIO, ...): the file is left untouched
    /// and the caller reports it as transient.
    Io(io::Error),
}

/// The token in `path` if the file is trusted, else why not.
fn read_token(path: &Path) -> Result<String, TokenFault> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(TokenFault::Missing),
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
            return Err(TokenFault::Untrusted("symlink"))
        }
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            return Err(TokenFault::Untrusted("unreadable"))
        }
        Err(err) => return Err(TokenFault::Io(err)),
    };
    let meta = file.metadata().map_err(TokenFault::Io)?;
    if !meta.is_file() || meta.uid() != euid() || meta.mode() & 0o777 != 0o600 {
        return Err(TokenFault::Untrusted("not_private"));
    }
    let mut raw = Vec::new();
    file.read_to_end(&mut raw).map_err(TokenFault::Io)?;
    let valid = raw.len() == 64 && raw.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match String::from_utf8(raw) {
        Ok(token) if valid => Ok(token),
        _ => Err(TokenFault::Untrusted("malformed")),
    }
}

/// Whether a token directory entry is a temp file a crashed issuer left
/// behind: `.<pid>-<random>.tmp` whose `pid` no longer runs, or a
/// `.<random>.tmp` from before temp names carried a pid.
fn is_orphan_temp(name: &str, alive: impl Fn(u32) -> bool) -> bool {
    let Some(stem) = name
        .strip_prefix('.')
        .and_then(|rest| rest.strip_suffix(".tmp"))
    else {
        return false;
    };
    match stem.split_once('-') {
        Some((pid, _)) => pid.parse().is_ok_and(|pid| !alive(pid)),
        None => true,
    }
}

/// Make sure `path` holds a trusted token, as judged by `read`.
/// `Some(reason)` when an untrusted file was replaced. Temp files whose
/// writer is gone, as judged by `alive`, are swept first.
fn issue_token(
    path: &Path,
    read: impl Fn(&Path) -> Result<String, TokenFault>,
    alive: impl Fn(u32) -> bool,
) -> io::Result<Option<&'static str>> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by this user", dir.display()),
        ));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // symlink_metadata never follows a link, and only our own regular
        // files are removed.
        let ours =
            fs::symlink_metadata(entry.path()).is_ok_and(|m| m.is_file() && m.uid() == euid());
        if ours && is_orphan_temp(&name, &alive) {
            let _ = fs::remove_file(entry.path());
        }
    }
    let replaced = match read(path) {
        Ok(_) => return Ok(None),
        Err(TokenFault::Missing) => None,
        Err(TokenFault::Untrusted(reason)) => Some(reason),
        Err(TokenFault::Io(err)) => return Err(err),
    };
    let mut random = [0u8; 40];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let temp = dir.join(format!(
        ".{}-{}.tmp",
        std::process::id(),
        hex(&random[32..])
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    // The untrusted file is replaced only once the new token is safely on
    // disk, in one rename, so the name never goes missing.
    let placed = file
        .set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|()| file.write_all(hex(&random[..32]).as_bytes()))
        .and_then(|()| file.sync_all())
        .and_then(|()| match replaced {
            Some(_) => fs::rename(&temp, path),
            None => fs::hard_link(&temp, path),
        });
    if replaced.is_none() || placed.is_err() {
        let _ = fs::remove_file(&temp);
    }
    match placed {
        // Another issuer linked a complete token first.
        Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
        _ => {}
    }
    fs::File::open(dir)?.sync_all()?;
    Ok(replaced)
}

/// Why a `channel.hello` was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HelloRefusal {
    /// An unknown binding or a token that does not match: one answer for
    /// both. `NotOwner` is checked first, so anyone who can reach the socket
    /// can learn binding names and owners, never a token.
    Unauthorized,
    NotOwner {
        owner: String,
    },
    /// The token file could not be read for a reason other than trust (see
    /// [`TokenFault::Io`]). Nothing changed; retry once it clears.
    TokenUnavailable(String),
}

/// Check one `channel.hello`, logging the outcome without the token.
pub(crate) fn hello<'a>(
    bindings: &'a [ChannelBinding],
    node: &str,
    name: &str,
    token: &ChannelToken,
) -> Result<&'a ChannelBinding, HelloRefusal> {
    let (refused, outcome) = match bindings.iter().find(|b| b.name == name) {
        None => (Some("unknown_binding"), Err(HelloRefusal::Unauthorized)),
        Some(b) if b.owner_node != node => (
            Some("not_owner"),
            Err(HelloRefusal::NotOwner {
                owner: b.owner_node.clone(),
            }),
        ),
        Some(b) => match token_matches(&token_path(name), token) {
            Ok(true) => (None, Ok(b)),
            Ok(false) => (Some("bad_token"), Err(HelloRefusal::Unauthorized)),
            Err(err) => (
                Some("token_unavailable"),
                Err(HelloRefusal::TokenUnavailable(err.to_string())),
            ),
        },
    };
    crate::logging::channel_hello(name, refused);
    outcome
}

/// Whether `presented` is the issued token. An untrusted or missing file
/// matches nothing; any other read failure is an error.
fn token_matches(path: &Path, presented: &ChannelToken) -> io::Result<bool> {
    let issued = match read_token(path) {
        Ok(issued) => issued,
        Err(TokenFault::Io(err)) => return Err(err),
        Err(_) => return Ok(false),
    };
    let issued = Sha256::digest(issued.as_bytes());
    let presented = Sha256::digest(presented.0.trim().as_bytes());
    let diff = issued
        .iter()
        .zip(presented.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    Ok(diff == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(path: &Path, token: impl Into<String>) -> bool {
        token_matches(path, &ChannelToken(token.into())).unwrap()
    }

    fn binding(name: &str, owner: &str) -> ChannelBinding {
        ChannelBinding {
            name: name.into(),
            owner_node: owner.into(),
            target: ChannelTarget::Fleet,
            scopes: vec![ChannelScope::Notify],
        }
    }

    #[test]
    fn a_target_round_trips_and_a_bad_one_is_refused() {
        for raw in ["fleet", "repo:flock", "agent:agent_atlas_1"] {
            let target = ChannelTarget::try_from(raw.to_owned()).unwrap();
            assert_eq!(String::from(target), raw);
        }
        for raw in ["", "repo:", "agent", "host:atlas", "fleet:x"] {
            assert!(ChannelTarget::try_from(raw.to_owned()).is_err(), "{raw}");
        }
    }

    #[test]
    fn a_bindings_section_parses_and_unknown_keys_are_refused() {
        let parsed: Vec<ChannelBinding> = toml::from_str::<toml::Table>(
            "[[channels]]\nname = \"phone\"\nowner_node = \"atlas\"\ntarget = \"repo:flock\"\nscopes = [\"send_to_agent\", \"reply\"]\n",
        )
        .unwrap()["channels"]
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(parsed[0].target, ChannelTarget::Repo("flock".into()));
        assert_eq!(
            parsed[0].scopes,
            [ChannelScope::SendToAgent, ChannelScope::Reply]
        );
        let unknown = toml::Value::try_from(toml::toml! {
            name = "phone"
            owner_node = "atlas"
            target = "fleet"
            token = "inline"
        })
        .unwrap()
        .try_into::<ChannelBinding>();
        assert!(unknown.is_err(), "a token is never config");
    }

    #[test]
    fn invalid_and_duplicate_bindings_are_dropped_with_a_diagnostic() {
        let mut bindings = vec![
            binding("phone", "atlas"),
            binding("phone", "atlas"),
            binding("../escape", "atlas"),
            binding("", "atlas"),
            binding("ownerless", " "),
            binding("ok_2-b", "kiln"),
        ];
        let mut diagnostics = Vec::new();
        validate(&mut bindings, &mut diagnostics);
        let names: Vec<_> = bindings.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["phone", "ok_2-b"]);
        assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
    }

    #[test]
    fn an_issued_token_is_private_kept_and_matched_only_exactly() {
        let dir = std::env::temp_dir().join(format!("flk-chan-{}", std::process::id()));
        let path = dir.join("tokens/phone.token");
        assert_eq!(issue_token(&path, read_token, |_| false).unwrap(), None);
        let issued = fs::read_to_string(&path).unwrap();
        assert_eq!(issued.len(), 64);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(issue_token(&path, read_token, |_| false).unwrap(), None);
        assert_eq!(fs::read_to_string(&path).unwrap(), issued, "kept");
        let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temp file is left behind");

        assert!(matches(&path, issued.clone()));
        assert!(!matches(&path, issued[1..].to_owned()));
        assert!(!matches(&path, String::new()));
        fs::remove_dir_all(&dir).unwrap();
        assert!(!matches(&path, issued), "a deleted file revokes");
    }

    /// A crash between create and write used to leave an empty token file
    /// that counted as issued and refused every hello forever.
    #[test]
    fn an_empty_or_too_open_token_file_is_never_trusted_and_is_reissued() {
        let dir = std::env::temp_dir().join(format!("flk-chan-bad-{}", std::process::id()));
        let path = dir.join("tokens/phone.token");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        assert!(!matches(&path, String::new()));
        assert!(matches!(
            read_token(&path),
            Err(TokenFault::Untrusted("malformed"))
        ));

        assert_eq!(
            issue_token(&path, read_token, |_| false).unwrap(),
            Some("malformed")
        );
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(path.parent().unwrap()),
            0o700,
            "an owned dir is tightened"
        );
        let recovered = fs::read_to_string(&path).unwrap();
        assert!(matches(&path, recovered.clone()));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!matches(&path, recovered.clone()));
        assert_eq!(
            issue_token(&path, read_token, |_| false).unwrap(),
            Some("not_private")
        );
        let rotated = fs::read_to_string(&path).unwrap();
        assert_ne!(
            rotated, recovered,
            "a token that was readable by others is rotated"
        );
        assert!(matches(&path, rotated));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// EMFILE, EIO and the like are not trust failures: the token stays as
    /// it is and the error is reported, rather than a valid token destroyed.
    #[test]
    fn a_non_trust_read_error_neither_deletes_nor_rotates_the_token() {
        let dir = std::env::temp_dir().join(format!("flk-chan-io-{}", std::process::id()));
        let path = dir.join("tokens/phone.token");
        issue_token(&path, read_token, |_| false).unwrap();
        let issued = fs::read_to_string(&path).unwrap();
        let eio = |_: &Path| Err(TokenFault::Io(io::Error::from_raw_os_error(libc::EIO)));
        let err = issue_token(&path, eio, |_| false).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
        assert_eq!(fs::read_to_string(&path).unwrap(), issued, "left untouched");
        assert!(matches(&path, issued));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlinked_token_is_replaced_and_stale_temp_files_are_swept() {
        let dir = std::env::temp_dir().join(format!("flk-chan-ln-{}", std::process::id()));
        let tokens = dir.join("tokens");
        let path = tokens.join("phone.token");
        fs::create_dir_all(&tokens).unwrap();
        let elsewhere = dir.join("elsewhere");
        fs::write(&elsewhere, "a".repeat(64)).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        fs::write(tokens.join(".stale.tmp"), "partial").unwrap();
        let outside = dir.join("outside");
        fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, tokens.join(".link.tmp")).unwrap();

        assert!(!matches(&path, "a".repeat(64)));
        assert_eq!(
            issue_token(&path, read_token, |_| false).unwrap(),
            Some("symlink")
        );
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "a".repeat(64));
        assert!(
            !tokens.join(".stale.tmp").exists(),
            "a stale temp file without a pid is swept"
        );
        assert!(
            fs::symlink_metadata(tokens.join(".link.tmp")).is_ok(),
            "a symlink is never swept"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Two servers issue into one directory during a live handoff: a temp
    /// file whose writer still runs is kept, an orphan is swept (#1170).
    #[test]
    fn the_sweep_keeps_a_live_issuers_temp_and_removes_orphans() {
        let dir = std::env::temp_dir().join(format!("flk-chan-sw-{}", std::process::id()));
        let tokens = dir.join("tokens");
        let path = tokens.join("phone.token");
        fs::create_dir_all(&tokens).unwrap();
        let live = tokens.join(format!(".{}-ab12.tmp", std::process::id()));
        let dead = tokens.join(".4294967-cd34.tmp");
        let legacy = tokens.join(".ef56.tmp");
        let foreign = tokens.join(".not-a-pid.tmp");
        for temp in [&live, &dead, &legacy, &foreign] {
            fs::write(temp, "partial").unwrap();
        }

        let alive = |pid: u32| pid == std::process::id();
        assert_eq!(issue_token(&path, read_token, alive).unwrap(), None);
        assert!(
            live.exists(),
            "a live issuer's temp file survives the sweep"
        );
        assert!(!dead.exists(), "a dead issuer's temp file is swept");
        assert!(!legacy.exists(), "a temp file without a pid is swept");
        assert!(foreign.exists(), "a name flock never writes is left alone");
        let temps: Vec<_> = fs::read_dir(&tokens)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert_eq!(
            temps.len(),
            2,
            "issuing leaves no temp of its own: {temps:?}"
        );

        assert!(is_orphan_temp(
            ".4294967-cd34.tmp",
            crate::platform::process_exists
        ));
        assert!(!is_orphan_temp(
            &format!(".{}-ab12.tmp", std::process::id()),
            crate::platform::process_exists
        ));
        assert!(!is_orphan_temp("phone.token", |_| false));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Replacing an untrusted token renames over it, so a reader never finds
    /// the name missing (#1170). The old remove-then-link left a window.
    #[test]
    fn replacing_an_untrusted_token_never_leaves_the_name_missing() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("flk-chan-rn-{}", std::process::id()));
        let path = dir.join("tokens/phone.token");
        assert_eq!(issue_token(&path, read_token, |_| false).unwrap(), None);
        let done = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (done, path) = (done.clone(), path.clone());
            std::thread::spawn(move || {
                let mut missing = 0u32;
                while !done.load(Ordering::Relaxed) {
                    if fs::symlink_metadata(&path).is_err() {
                        missing += 1;
                    }
                }
                missing
            })
        };
        for _ in 0..100 {
            fs::write(&path, "malformed").unwrap();
            assert_eq!(
                issue_token(&path, read_token, |_| false).unwrap(),
                Some("malformed")
            );
        }
        done.store(true, Ordering::Relaxed);
        assert_eq!(watcher.join().unwrap(), 0, "the token name went missing");
        assert!(read_token(&path).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A first issue links rather than renames, so a token another issuer
    /// placed between the read and the link is kept, never overwritten.
    #[test]
    fn a_first_issue_never_overwrites_a_token_placed_meanwhile() {
        let dir = std::env::temp_dir().join(format!("flk-chan-lk-{}", std::process::id()));
        let path = dir.join("tokens/phone.token");
        assert_eq!(issue_token(&path, read_token, |_| false).unwrap(), None);
        let issued = fs::read_to_string(&path).unwrap();

        let missing = |_: &Path| Err(TokenFault::Missing);
        assert_eq!(issue_token(&path, missing, |_| false).unwrap(), None);
        assert_eq!(fs::read_to_string(&path).unwrap(), issued, "kept");
        let temps = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temps, 0, "the losing temp file is removed");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_token_never_prints_through_debug() {
        let params = crate::api::schema::ChannelHelloParams {
            binding: "phone".into(),
            token: ChannelToken("s3cret-value".into()),
        };
        assert!(!format!("{params:?}").contains("s3cret-value"));
    }

    #[test]
    fn hello_refuses_unknown_bindings_and_other_owners_before_reading_a_token() {
        let bindings = [binding("phone", "atlas"), binding("far", "kiln")];
        let token = ChannelToken("x".into());
        assert_eq!(
            hello(&bindings, "atlas", "nosuch", &token).unwrap_err(),
            HelloRefusal::Unauthorized
        );
        assert_eq!(
            hello(&bindings, "atlas", "far", &token).unwrap_err(),
            HelloRefusal::NotOwner {
                owner: "kiln".into()
            }
        );
    }
}
