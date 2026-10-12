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
//! in the same directory, fsyncs it and hard-links it into place, so a crash
//! never leaves a partial token behind a valid name. A file that fails the
//! check (empty, malformed, too open, someone else's) is replaced the same way
//! on the next startup or reload, logged with the reason. The directory is
//! tightened to 0700 when this user owns it and refused otherwise. The file is
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
        match issue_token(&token_path(&binding.name)) {
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

/// The token in `path` if the file is trusted, else why not.
fn read_token(path: &Path) -> Result<String, &'static str> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err("missing"),
        Err(_) => return Err("unreadable"),
    };
    let meta = file.metadata().map_err(|_| "unreadable")?;
    if !meta.is_file() || meta.uid() != euid() || meta.mode() & 0o777 != 0o600 {
        return Err("not_private");
    }
    let mut token = String::new();
    file.read_to_string(&mut token).map_err(|_| "unreadable")?;
    let valid = token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    valid.then_some(token).ok_or("malformed")
}

/// Make sure `path` holds a trusted token. `Some(reason)` when an untrusted
/// file was replaced.
fn issue_token(path: &Path) -> io::Result<Option<&'static str>> {
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
    let replaced = match read_token(path) {
        Ok(_) => return Ok(None),
        Err("missing") => None,
        Err(reason) => {
            fs::remove_file(path)?;
            Some(reason)
        }
    };
    let mut random = [0u8; 40];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let temp = dir.join(format!(".{}.tmp", hex(&random[32..])));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    let linked = file
        .set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|()| file.write_all(hex(&random[..32]).as_bytes()))
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::hard_link(&temp, path));
    let _ = fs::remove_file(&temp);
    match linked {
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
        Some(_) if !token_matches(&token_path(name), token) => {
            (Some("bad_token"), Err(HelloRefusal::Unauthorized))
        }
        Some(b) => (None, Ok(b)),
    };
    crate::logging::channel_hello(name, refused);
    outcome
}

fn token_matches(path: &Path, presented: &ChannelToken) -> bool {
    let Ok(issued) = read_token(path) else {
        return false;
    };
    let issued = Sha256::digest(issued.as_bytes());
    let presented = Sha256::digest(presented.0.trim().as_bytes());
    let diff = issued
        .iter()
        .zip(presented.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(issue_token(&path).unwrap(), None);
        let issued = fs::read_to_string(&path).unwrap();
        assert_eq!(issued.len(), 64);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(issue_token(&path).unwrap(), None);
        assert_eq!(fs::read_to_string(&path).unwrap(), issued, "kept");
        let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temp file is left behind");

        assert!(token_matches(&path, &ChannelToken(issued.clone())));
        assert!(!token_matches(&path, &ChannelToken(issued[1..].to_owned())));
        assert!(!token_matches(&path, &ChannelToken(String::new())));
        fs::remove_dir_all(&dir).unwrap();
        assert!(
            !token_matches(&path, &ChannelToken(issued)),
            "a deleted file revokes"
        );
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
        assert!(!token_matches(&path, &ChannelToken(String::new())));
        assert_eq!(read_token(&path), Err("malformed"));

        assert_eq!(issue_token(&path).unwrap(), Some("malformed"));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(path.parent().unwrap()),
            0o700,
            "an owned dir is tightened"
        );
        let recovered = fs::read_to_string(&path).unwrap();
        assert!(token_matches(&path, &ChannelToken(recovered.clone())));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!token_matches(&path, &ChannelToken(recovered.clone())));
        assert_eq!(issue_token(&path).unwrap(), Some("not_private"));
        let rotated = fs::read_to_string(&path).unwrap();
        assert_ne!(
            rotated, recovered,
            "a token that was readable by others is rotated"
        );
        assert!(token_matches(&path, &ChannelToken(rotated)));
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
