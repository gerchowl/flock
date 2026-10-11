//! Pre-trust a linked worktree for Claude Code before flock launches `claude`
//! in it (#605).
//!
//! Claude Code asks "Do you trust this folder?" the first time it starts in a
//! directory, and a headless dispatcher cannot see or answer that dialog, so a
//! delegate started in a fresh worktree sits idle until a human notices. Its
//! lookup (`projects[<path>].hasTrustDialogAccepted` in the profile's
//! `.claude.json`) walks parent directories only up to the git root, and every
//! linked worktree is its own git root, so no setting outside the worktree
//! covers it. Writing the exact path is the only route.
//!
//! The rule that keeps this from widening trust: a worktree is marked trusted
//! only when the MIRROR of the launch directory in the repository it was
//! checked out from is trusted by that same lookup. A linked worktree holds the
//! same repository as its main checkout, so this hands Claude the answer the
//! operator already gave. A directory that is not inside a linked worktree is
//! never touched, which is what keeps `$HOME` prompting (#365).
//!
//! The file belongs to Claude, which rewrites it while it runs, so the edit is
//! a byte splice rather than a re-serialisation: every other key keeps its
//! bytes. It is made under the same lock Claude Code takes for its own config
//! writes (see [`ConfigLock`]), and re-read just before the atomic replace in
//! case a writer ignored the lock.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::value::RawValue;
use serde_json::Value;

const TRUST_KEY: &str = "hasTrustDialogAccepted";

/// What a pre-trust attempt did. Only [`Outcome::Wrote`] changed anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Wrote(PathBuf),
    AlreadyTrusted,
    SourceUntrusted,
    NotLinkedWorktree,
    NoConfig,
    ExternallyOwned,
    LockHeld,
    LockLost,
}

impl Outcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Wrote(_) => "wrote",
            Self::AlreadyTrusted => "already_trusted",
            Self::SourceUntrusted => "source_untrusted",
            Self::NotLinkedWorktree => "not_linked_worktree",
            Self::NoConfig => "no_config",
            Self::ExternallyOwned => "externally_owned",
            Self::LockHeld => "lock_held",
            Self::LockLost => "lock_lost",
        }
    }
}

/// Pre-trust `cwd` for a `claude` argv about to be spawned there, against the
/// profile the child will run under. Failures are logged and swallowed: a
/// trust write that did not happen leaves the dialog for the operator, which
/// is where things stood before, and never blocks the spawn.
pub(crate) fn pretrust_for_launch(
    enabled: bool,
    argv: &[String],
    cwd: &Path,
    spawn_env: &[(String, String)],
) {
    if !enabled || crate::spawn::env::agent_for_argv(argv) != Some(crate::detect::Agent::Claude) {
        return;
    }
    let Some(config) = config_path(spawn_env, |key| std::env::var(key).ok(), Path::is_file) else {
        return;
    };
    let config_text = config.display().to_string();
    let mut hooks = Hooks {
        before_write: &mut || {},
        sleep: &mut std::thread::sleep,
    };
    let (outcome, path, detail) = match pretrust_at(&config, cwd, &mut hooks) {
        Ok(Outcome::Wrote(target)) => ("wrote", target, String::new()),
        Ok(outcome) => (outcome.label(), cwd.to_path_buf(), String::new()),
        Err(err) => ("error", cwd.to_path_buf(), err.to_string()),
    };
    crate::logging::claude_pretrust(outcome, &config_text, &path.display().to_string(), &detail);
}

/// The global config file the child reads, resolved the way Claude Code
/// 2.1.295 does (`Tt()` in its bundle): `<claude home>/.config.json` when that
/// legacy file exists, where the claude home is `$CLAUDE_CONFIG_DIR` or
/// `~/.claude`; otherwise `.claude<suffix>.json` in `$CLAUDE_CONFIG_DIR`, or in
/// `$HOME` when it is unset. The suffix is `-custom-oauth` under
/// `CLAUDE_CODE_CUSTOM_OAUTH_URL` and empty for the production build; the
/// staging and local suffixes come from build-internal state no environment
/// shows, so those builds are not covered. A variable carried in the spawn
/// environment wins over the server's own, because that is what the child's
/// environment is built from.
fn config_path(
    spawn_env: &[(String, String)],
    server_env: impl Fn(&str) -> Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let lookup = |key: &str| {
        spawn_env
            .iter()
            .rev()
            .find(|(carried, _)| carried == key)
            .map(|(_, value)| value.clone())
            .or_else(|| server_env(key))
            .filter(|value| !value.is_empty())
    };
    let config_dir = lookup(super::CLAUDE_CONFIG_DIR_ENV_VAR).map(PathBuf::from);
    let home = server_env("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let claude_home = config_dir
        .clone()
        .or_else(|| home.as_ref().map(|home| home.join(".claude")))?;
    let legacy = claude_home.join(".config.json");
    if exists(&legacy) {
        return Some(legacy);
    }
    let suffix = if lookup("CLAUDE_CODE_CUSTOM_OAUTH_URL").is_some() {
        "-custom-oauth"
    } else {
        ""
    };
    Some(config_dir.or(home)?.join(format!(".claude{suffix}.json")))
}

/// A linked worktree and the main checkout it shares a repository with, both
/// as real paths.
#[derive(Debug, PartialEq, Eq)]
struct LinkedWorktree {
    root: PathBuf,
    main_root: PathBuf,
}

/// The linked worktree `dir` sits in, read from git's own files rather than a
/// `git` subprocess. A `.git` FILE is only a claim, and anyone can write one,
/// so it is believed only when git's own records agree from both ends: the
/// gitdir it names sits at `<common dir>/worktrees/<name>`, and that gitdir's
/// `gitdir` file points back at this very `.git`. A `.git` directory is a main
/// checkout, and a submodule's gitdir lives under `modules/`, not `worktrees/`;
/// neither is a worktree.
fn linked_worktree(dir: &Path) -> Option<LinkedWorktree> {
    for root in dir.ancestors() {
        let dotgit = root.join(".git");
        let Ok(metadata) = fs::metadata(&dotgit) else {
            continue;
        };
        if !metadata.is_file() {
            return None;
        }
        let pointer = fs::read_to_string(&dotgit).ok()?;
        let gitdir =
            fs::canonicalize(root.join(pointer.trim().strip_prefix("gitdir:")?.trim())).ok()?;
        let commondir = fs::read_to_string(gitdir.join("commondir")).ok()?;
        let common = fs::canonicalize(gitdir.join(commondir.trim())).ok()?;
        if gitdir.parent()? != common.join("worktrees") {
            return None;
        }
        let back = fs::read_to_string(gitdir.join("gitdir")).ok()?;
        if fs::canonicalize(gitdir.join(back.trim())).ok()? != fs::canonicalize(&dotgit).ok()? {
            return None;
        }
        let main_root = crate::worktree::main_root_from_common_dir(&common);
        if main_root == common {
            // A bare repository has no checkout anyone could have trusted.
            return None;
        }
        return Some(LinkedWorktree {
            root: root.to_path_buf(),
            main_root,
        });
    }
    None
}

/// The lock Claude Code takes around every write of its global config.
///
/// Verified against the Claude Code 2.1.295 bundle: its config save
/// (`saveConfigWithLock`) calls the vendored `proper-lockfile` with
/// `lockfilePath` set to `<file>.lock` and no other options, so the library
/// defaults apply: the lock is a DIRECTORY created with `mkdir`; an existing
/// one is stale only when its mtime is older than `stale` (10 s), in which
/// case it is removed and the `mkdir` tried once more; the holder refreshes the
/// mtime every `stale / 2` and treats a changed mtime as "Config lock
/// compromised". Claude retries a held lock with backoff
/// (`[200,400,800,1600,3200,4000]` ms) and never writes without it.
///
/// flock follows the same protocol with a shorter wait, since it runs on the
/// server's event loop, and gives up instead of writing unlocked: a skipped
/// write leaves the trust dialog, which is safe. Its own hold lasts one read
/// and one write, well inside `stale / 2`, so it does not refresh the mtime;
/// it re-checks the mtime before the write instead, and backs off if another
/// process took the lock over meanwhile.
struct ConfigLock {
    dir: PathBuf,
    mtime: SystemTime,
    acquired: std::time::Instant,
}

const LOCK_STALE: Duration = Duration::from_secs(10);
const LOCK_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
];

impl ConfigLock {
    fn path(config: &Path) -> PathBuf {
        let mut dir = config.as_os_str().to_owned();
        dir.push(".lock");
        PathBuf::from(dir)
    }

    /// Take the lock, waiting out a short hold. `None` means another process
    /// still has it.
    fn acquire(config: &Path, sleep: &mut dyn FnMut(Duration)) -> io::Result<Option<Self>> {
        let dir = Self::path(config);
        for delay in std::iter::once(Duration::ZERO).chain(LOCK_RETRY_DELAYS) {
            if !delay.is_zero() {
                sleep(delay);
            }
            if let Some(lock) = Self::try_acquire(&dir, SystemTime::now())? {
                return Ok(Some(lock));
            }
        }
        Ok(None)
    }

    /// One `proper-lockfile` acquisition attempt, judged against `now`.
    fn try_acquire(dir: &Path, now: SystemTime) -> io::Result<Option<Self>> {
        if let Some(lock) = Self::create(dir)? {
            return Ok(Some(lock));
        }
        let modified = match fs::metadata(dir).and_then(|metadata| metadata.modified()) {
            // Released between the mkdir and the stat: one more try, as
            // `proper-lockfile` does with `stale: 0`.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Self::create(dir),
            Err(err) => return Err(err),
            Ok(modified) => modified,
        };
        let stale = now
            .checked_sub(LOCK_STALE)
            .is_some_and(|cutoff| modified < cutoff);
        if !stale {
            return Ok(None);
        }
        match fs::remove_dir(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        Self::create(dir)
    }

    fn create(dir: &Path) -> io::Result<Option<Self>> {
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Ok(None),
            Err(err) => return Err(err),
        }
        match fs::metadata(dir).and_then(|metadata| metadata.modified()) {
            Ok(mtime) => Ok(Some(Self {
                dir: dir.to_path_buf(),
                mtime,
                acquired: std::time::Instant::now(),
            })),
            Err(err) => {
                let _ = fs::remove_dir(dir);
                Err(err)
            }
        }
    }

    /// Still ours: not past the point where a waiter may call it stale, and
    /// not replaced by someone who did.
    fn still_held(&self) -> bool {
        self.acquired.elapsed() < LOCK_STALE / 2
            && fs::metadata(&self.dir)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|mtime| mtime == self.mtime)
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_dir(&self.dir) {
            if err.kind() != io::ErrorKind::NotFound {
                crate::logging::claude_pretrust(
                    "error",
                    &self.dir.display().to_string(),
                    &self.dir.display().to_string(),
                    &format!("could not release the config lock: {err}"),
                );
            }
        }
    }
}

/// Seams for the moments a test needs to step into: `before_write` runs
/// between the decision and the re-read, which is where a writer that ignores
/// the lock lands, and `sleep` is the wait between lock attempts.
struct Hooks<'a> {
    before_write: &'a mut dyn FnMut(),
    sleep: &'a mut dyn FnMut(Duration),
}

/// Pre-trust `cwd` in the config file at `config`.
fn pretrust_at(config: &Path, cwd: &Path, hooks: &mut Hooks<'_>) -> io::Result<Outcome> {
    let cwd = fs::canonicalize(cwd)?;
    let Some(worktree) = linked_worktree(&cwd) else {
        return Ok(Outcome::NotLinkedWorktree);
    };
    if super::mcp_config::externally_owned_at(config) {
        return Ok(Outcome::ExternallyOwned);
    }
    if !config.is_file() {
        return Ok(Outcome::NoConfig);
    }
    if let Some(dir) = config.parent() {
        super::atomic_write::sweep_orphans(dir);
    }
    let Some(lock) = ConfigLock::acquire(config, hooks.sleep)? else {
        return Ok(Outcome::LockHeld);
    };
    for _ in 0..2 {
        let content = match fs::read_to_string(config) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Outcome::NoConfig),
            Err(err) => return Err(err),
        };
        let (target, updated) = match plan(&content, &cwd, &worktree)? {
            Plan::Done(outcome) => return Ok(outcome),
            Plan::Write { target, updated } => (target, updated),
        };
        (hooks.before_write)();
        let latest = match fs::read_to_string(config) {
            Ok(latest) => latest,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Outcome::NoConfig),
            Err(err) => return Err(err),
        };
        if latest != content {
            continue;
        }
        if !lock.still_held() {
            return Ok(Outcome::LockLost);
        }
        super::atomic_write::replace(config, updated.as_bytes())?;
        return Ok(Outcome::Wrote(target));
    }
    Err(io::Error::other(
        "Claude Code config changed twice while pre-trusting; left for the trust dialog",
    ))
}

enum Plan {
    Done(Outcome),
    Write { target: PathBuf, updated: String },
}

/// Decide against one snapshot of the file. Pure apart from canonicalising the
/// paths Claude recorded, so a key written through a symlink still matches.
fn plan(content: &str, cwd: &Path, worktree: &LinkedWorktree) -> io::Result<Plan> {
    let doc: Value = serde_json::from_str(content).map_err(io::Error::other)?;
    let trusted: Vec<PathBuf> = doc
        .get("projects")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, entry)| entry.get(TRUST_KEY) == Some(&Value::Bool(true)))
        .map(|(key, _)| crate::worktree::canonical_or_original(Path::new(key)))
        .collect();
    let is_trusted = |path: &Path| trusted.iter().any(|known| known == path);

    // Claude's own walk from the launch directory up to the worktree root.
    if cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(&worktree.root))
        .any(&is_trusted)
    {
        return Ok(Plan::Done(Outcome::AlreadyTrusted));
    }
    let Ok(relative) = cwd.strip_prefix(&worktree.root) else {
        return Ok(Plan::Done(Outcome::NotLinkedWorktree));
    };
    // The same walk in the main checkout, from the mirrored directory. The
    // deepest trusted directory is the one mirrored, so the worktree gets
    // exactly the scope its source has and nothing above it.
    let source = worktree.main_root.join(relative);
    let Some(trusted_source) = source
        .ancestors()
        .take_while(|dir| dir.starts_with(&worktree.main_root))
        .find(|dir| is_trusted(dir))
    else {
        return Ok(Plan::Done(Outcome::SourceUntrusted));
    };
    // `join("")` would append a separator, and Claude keys paths without one.
    let target = match trusted_source.strip_prefix(&worktree.main_root) {
        Ok(suffix) if suffix.as_os_str().is_empty() => worktree.root.clone(),
        Ok(suffix) => worktree.root.join(suffix),
        Err(_) => return Ok(Plan::Done(Outcome::SourceUntrusted)),
    };
    let key = target
        .to_str()
        .ok_or_else(|| io::Error::other("worktree path is not UTF-8"))?;
    Ok(match splice_trust(content, key)? {
        Some(updated) => Plan::Write { target, updated },
        None => Plan::Done(Outcome::AlreadyTrusted),
    })
}

type Members<'a> = std::collections::BTreeMap<String, &'a RawValue>;

/// Set `projects[key].hasTrustDialogAccepted = true` by inserting or replacing
/// only the bytes that carry it. `None` when it is already true.
fn splice_trust(content: &str, key: &str) -> io::Result<Option<String>> {
    let offset = |raw: &RawValue| raw.get().as_ptr() as usize - content.as_ptr() as usize;
    let root: Members = serde_json::from_str(content).map_err(io::Error::other)?;
    let key_json = serde_json::to_string(key)?;
    let (at, insertion, replace_len) = match root.get("projects") {
        None => (
            append_point(content, 0, &root)?,
            format!(
                "{}\"projects\":{{{key_json}:{{\"{TRUST_KEY}\":true}}}}",
                separator(&root)
            ),
            0,
        ),
        Some(projects) => {
            let members: Members = serde_json::from_str(projects.get())
                .map_err(|_| io::Error::other("projects must be an object"))?;
            match members.get(key) {
                None => (
                    append_point(projects.get(), offset(projects), &members)?,
                    format!("{}{key_json}:{{\"{TRUST_KEY}\":true}}", separator(&members)),
                    0,
                ),
                Some(entry) => {
                    let fields: Members = serde_json::from_str(entry.get())
                        .map_err(|_| io::Error::other("project entry must be an object"))?;
                    match fields.get(TRUST_KEY) {
                        Some(value) if value.get() == "true" => return Ok(None),
                        Some(value) => (offset(value), "true".to_string(), value.get().len()),
                        None => (
                            append_point(entry.get(), offset(entry), &fields)?,
                            format!("{}\"{TRUST_KEY}\":true", separator(&fields)),
                            0,
                        ),
                    }
                }
            }
        }
    };
    Ok(Some(format!(
        "{}{insertion}{}",
        &content[..at],
        &content[at + replace_len..]
    )))
}

fn separator(members: &Members) -> &'static str {
    if members.is_empty() {
        ""
    } else {
        ","
    }
}

/// Where a new member of the object `raw` (at offset `start` in the file)
/// goes: straight after its last value, or after the opening brace when it is
/// empty, so the whitespace before its closing brace stays where it was.
fn append_point(raw: &str, start: usize, members: &Members) -> io::Result<usize> {
    let open = raw
        .find('{')
        .ok_or_else(|| io::Error::other("expected a JSON object"))?;
    let base = raw.as_ptr() as usize;
    Ok(members
        .values()
        .map(|value| value.get().as_ptr() as usize - base + value.get().len())
        .max()
        .map_or(start + open + 1, |end| start + end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_committed_repo, run_git, unique_temp_path};

    fn run(config: &Path, cwd: &Path) -> io::Result<Outcome> {
        run_with(config, cwd, &mut || {})
    }

    /// Lock waits are recorded, never slept, so a held lock costs no time.
    fn run_with(config: &Path, cwd: &Path, before_write: &mut dyn FnMut()) -> io::Result<Outcome> {
        pretrust_at(
            config,
            cwd,
            &mut Hooks {
                before_write,
                sleep: &mut |_| {},
            },
        )
    }

    fn age(path: &Path, by: Duration) {
        fs::File::open(path)
            .unwrap()
            .set_modified(SystemTime::now() - by)
            .unwrap();
    }

    struct Fixture {
        base: PathBuf,
        repo: PathBuf,
        worktree: PathBuf,
        config: PathBuf,
    }

    impl Fixture {
        /// A real repo, a real linked worktree, and a `.claude.json` beside
        /// them. Paths are canonical, since `/tmp` is a symlink on macOS.
        fn new(name: &str) -> Self {
            let repo = fs::canonicalize(create_committed_repo(name)).unwrap();
            let base = unique_temp_path(&format!("{name}-base"));
            fs::create_dir_all(&base).unwrap();
            let base = fs::canonicalize(base).unwrap();
            let worktree = base.join("wt");
            run_git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    "feature",
                    worktree.to_str().unwrap(),
                ],
            );
            Self {
                config: base.join(".claude.json"),
                base,
                repo,
                worktree,
            }
        }

        fn trust(&self, path: &Path) -> String {
            format!(
                "{{\n  \"numStartups\": 7,\n  \"projects\": {{\n    {}: {{\n      \"allowedTools\": [],\n      \"{TRUST_KEY}\": true\n    }}\n  }},\n  \"zLast\": \"keep\"\n}}\n",
                serde_json::to_string(path.to_str().unwrap()).unwrap()
            )
        }

        fn read(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(&self.config).unwrap()).unwrap()
        }

        fn trusted(&self, path: &Path) -> bool {
            self.read()["projects"][path.to_str().unwrap()][TRUST_KEY] == Value::Bool(true)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
            let _ = fs::remove_dir_all(&self.repo);
        }
    }

    #[test]
    fn trusted_source_writes_the_worktree_entry_and_keeps_every_other_byte() {
        let fx = Fixture::new("pretrust-trusted");
        let original = fx.trust(&fx.repo);
        fs::write(&fx.config, &original).unwrap();
        let outcome = run(&fx.config, &fx.worktree).unwrap();
        assert_eq!(outcome, Outcome::Wrote(fx.worktree.clone()));
        assert!(fx.trusted(&fx.worktree));
        let written = fs::read_to_string(&fx.config).unwrap();
        // The splice only ever adds bytes: removing the inserted member gives
        // the original back exactly.
        let inserted = format!(
            ",{}:{{\"{TRUST_KEY}\":true}}",
            serde_json::to_string(fx.worktree.to_str().unwrap()).unwrap()
        );
        assert_eq!(written.replacen(&inserted, "", 1), original);
        assert_eq!(
            run(&fx.config, &fx.worktree).unwrap(),
            Outcome::AlreadyTrusted
        );
    }

    #[test]
    fn untrusted_source_and_non_worktree_dirs_write_nothing() {
        let fx = Fixture::new("pretrust-untrusted");
        let elsewhere = fx.base.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        // Trusting a parent of the repo does not count: Claude's walk stops at
        // the git root, so the source itself would still prompt.
        let original = fx.trust(&fx.base);
        fs::write(&fx.config, &original).unwrap();
        assert_eq!(
            run(&fx.config, &fx.worktree).unwrap(),
            Outcome::SourceUntrusted
        );
        // #365: a plain directory, even one whose parent is trusted, is never
        // a target.
        assert_eq!(
            run(&fx.config, &elsewhere).unwrap(),
            Outcome::NotLinkedWorktree
        );
        // The main checkout is not a linked worktree either.
        assert_eq!(
            run(&fx.config, &fx.repo).unwrap(),
            Outcome::NotLinkedWorktree
        );
        assert_eq!(fs::read_to_string(&fx.config).unwrap(), original);
    }

    #[test]
    fn a_subdirectory_mirrors_the_deepest_trusted_source_scope() {
        let fx = Fixture::new("pretrust-subdir");
        let sub = Path::new("pkg/inner");
        fs::create_dir_all(fx.repo.join(sub)).unwrap();
        fs::create_dir_all(fx.worktree.join(sub)).unwrap();
        // Only `pkg` is trusted in the source, so only `pkg` is trusted in the
        // worktree: its root stays as untrusted as the repo root is.
        fs::write(&fx.config, fx.trust(&fx.repo.join("pkg"))).unwrap();
        assert_eq!(
            run(&fx.config, &fx.worktree.join(sub)).unwrap(),
            Outcome::Wrote(fx.worktree.join("pkg"))
        );
        assert!(fx.trusted(&fx.worktree.join("pkg")));
        assert!(!fx.trusted(&fx.worktree));
        assert_eq!(
            run(&fx.config, &fx.worktree).unwrap(),
            Outcome::SourceUntrusted
        );
    }

    #[test]
    fn symlinked_paths_resolve_to_the_real_worktree() {
        let fx = Fixture::new("pretrust-symlink");
        let repo_link = fx.base.join("repo-link");
        let wt_link = fx.base.join("wt-link");
        std::os::unix::fs::symlink(&fx.repo, &repo_link).unwrap();
        std::os::unix::fs::symlink(&fx.worktree, &wt_link).unwrap();
        // The source was trusted through a symlink, and the agent is launched
        // through one: the entry is still keyed on the real worktree path.
        fs::write(&fx.config, fx.trust(&repo_link)).unwrap();
        assert_eq!(
            run(&fx.config, &wt_link).unwrap(),
            Outcome::Wrote(fx.worktree.clone())
        );
        assert!(fx.trusted(&fx.worktree));
        assert!(fx.read()["projects"]
            .get(wt_link.to_str().unwrap())
            .is_none());
    }

    #[test]
    fn a_symlinked_or_read_only_config_is_left_to_its_owner() {
        let fx = Fixture::new("pretrust-owned");
        let real = fx.base.join("real.json");
        fs::write(&real, fx.trust(&fx.repo)).unwrap();
        std::os::unix::fs::symlink(&real, &fx.config).unwrap();
        assert_eq!(
            run(&fx.config, &fx.worktree).unwrap(),
            Outcome::ExternallyOwned
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), fx.trust(&fx.repo));
    }

    #[test]
    fn file_mode_survives_the_replace() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new("pretrust-mode");
        fs::write(&fx.config, fx.trust(&fx.repo)).unwrap();
        fs::set_permissions(&fx.config, fs::Permissions::from_mode(0o600)).unwrap();
        run(&fx.config, &fx.worktree).unwrap();
        let mode = fs::metadata(&fx.config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_concurrent_rewrite_is_merged_by_one_retry() {
        let fx = Fixture::new("pretrust-race");
        fs::write(&fx.config, fx.trust(&fx.repo)).unwrap();
        let mut claude_writes = Some(
            fx.trust(&fx.repo)
                .replace("\"numStartups\": 7", "\"numStartups\": 8"),
        );
        let outcome = run_with(&fx.config, &fx.worktree, &mut || {
            if let Some(content) = claude_writes.take() {
                fs::write(&fx.config, content).unwrap();
            }
        })
        .unwrap();
        assert_eq!(outcome, Outcome::Wrote(fx.worktree.clone()));
        assert_eq!(fx.read()["numStartups"], 8);
        assert!(fx.trusted(&fx.worktree));

        // A writer that changes the file on every attempt wins: flock backs
        // off and leaves the file as Claude last wrote it.
        let fx = Fixture::new("pretrust-race-twice");
        fs::write(&fx.config, fx.trust(&fx.repo)).unwrap();
        let mut startups = 10;
        let err = run_with(&fx.config, &fx.worktree, &mut || {
            startups += 1;
            let content = fx.trust(&fx.repo).replace(
                "\"numStartups\": 7",
                &format!("\"numStartups\": {startups}"),
            );
            fs::write(&fx.config, content).unwrap();
        })
        .unwrap_err();
        assert!(err.to_string().contains("changed twice"));
        assert_eq!(fx.read()["numStartups"], 12);
        assert!(!fx.trusted(&fx.worktree));
    }

    #[test]
    fn splice_covers_every_shape_of_existing_entry() {
        let key = "/w/t";
        let cases = [
            (
                r#"{}"#,
                r#"{"projects":{"/w/t":{"hasTrustDialogAccepted":true}}}"#,
            ),
            (
                r#"{"a":1}"#,
                r#"{"a":1,"projects":{"/w/t":{"hasTrustDialogAccepted":true}}}"#,
            ),
            (
                r#"{"projects":{}}"#,
                r#"{"projects":{"/w/t":{"hasTrustDialogAccepted":true}}}"#,
            ),
            (
                r#"{"projects":{"/w/t":{"x":[1]}}}"#,
                r#"{"projects":{"/w/t":{"x":[1],"hasTrustDialogAccepted":true}}}"#,
            ),
            (
                r#"{"projects":{"/w/t":{"hasTrustDialogAccepted":false,"x":2}}}"#,
                r#"{"projects":{"/w/t":{"hasTrustDialogAccepted":true,"x":2}}}"#,
            ),
        ];
        for (before, after) in cases {
            assert_eq!(splice_trust(before, key).unwrap().as_deref(), Some(after));
        }
        assert_eq!(
            splice_trust(
                r#"{"projects":{"/w/t":{"hasTrustDialogAccepted":true}}}"#,
                key
            )
            .unwrap(),
            None
        );
        assert!(splice_trust(r#"{"projects":[]}"#, key).is_err());
        assert!(splice_trust("not json", key).is_err());
    }

    #[test]
    fn launch_gate_honours_opt_out_and_harness() {
        let fx = Fixture::new("pretrust-gate");
        fs::write(&fx.config, fx.trust(&fx.repo)).unwrap();
        let profile = fx.config.parent().unwrap().to_str().unwrap().to_string();
        let env = vec![(super::super::CLAUDE_CONFIG_DIR_ENV_VAR.to_string(), profile)];
        let claude = vec!["claude".to_string(), "--model".into(), "m".into()];
        pretrust_for_launch(false, &claude, &fx.worktree, &env);
        pretrust_for_launch(true, &["codex".to_string()], &fx.worktree, &env);
        assert!(!fx.trusted(&fx.worktree));
        pretrust_for_launch(true, &claude, &fx.worktree, &env);
        assert!(fx.trusted(&fx.worktree));
    }

    #[test]
    fn config_path_follows_claude_codes_lookup() {
        let none = |_: &Path| false;
        let server = |key: &str| match key {
            "HOME" => Some("/fixture-home".to_string()),
            "CLAUDE_CONFIG_DIR" => Some("/fixture-server-profile".to_string()),
            _ => None,
        };
        let carried = vec![("CLAUDE_CONFIG_DIR".to_string(), "/p/work".to_string())];
        assert_eq!(
            config_path(&carried, server, none),
            Some(PathBuf::from("/p/work/.claude.json"))
        );
        assert_eq!(
            config_path(&[], server, none),
            Some(PathBuf::from("/fixture-server-profile/.claude.json"))
        );
        let home_only = |key: &str| (key == "HOME").then(|| "/fixture-home".to_string());
        assert_eq!(
            config_path(&[], home_only, none),
            Some(PathBuf::from("/fixture-home/.claude.json"))
        );
        assert_eq!(config_path(&[], |_| None, none), None);

        // The legacy file wins wherever it exists: in the profile, or in
        // `~/.claude` when no profile is selected.
        let legacy = |path: &Path| path.ends_with(".config.json");
        assert_eq!(
            config_path(&carried, server, legacy),
            Some(PathBuf::from("/p/work/.config.json"))
        );
        assert_eq!(
            config_path(&[], home_only, legacy),
            Some(PathBuf::from("/fixture-home/.claude/.config.json"))
        );

        let custom = |key: &str| match key {
            "HOME" => Some("/fixture-home".to_string()),
            "CLAUDE_CODE_CUSTOM_OAUTH_URL" => Some("https://auth.example.invalid".to_string()),
            _ => None,
        };
        assert_eq!(
            config_path(&[], custom, none),
            Some(PathBuf::from("/fixture-home/.claude-custom-oauth.json"))
        );
    }

    #[test]
    fn a_forged_dotgit_file_is_not_a_worktree() {
        let fx = Fixture::new("pretrust-forged");
        fs::write(&fx.config, fx.trust(&fx.repo)).unwrap();
        let gitdir = fs::read_to_string(fx.worktree.join(".git")).unwrap();
        // A directory claiming the real worktree's gitdir: git's back-pointer
        // names the real worktree, not this one.
        let forged = fx.base.join("forged");
        fs::create_dir_all(&forged).unwrap();
        fs::write(forged.join(".git"), &gitdir).unwrap();
        assert_eq!(
            run(&fx.config, &forged).unwrap(),
            Outcome::NotLinkedWorktree
        );
        // One claiming the main repository's own git dir, which has a
        // `commondir`-less layout and is not under `worktrees/`.
        let forged_main = fx.base.join("forged-main");
        fs::create_dir_all(&forged_main).unwrap();
        fs::write(
            forged_main.join(".git"),
            format!("gitdir: {}\n", fx.repo.join(".git").display()),
        )
        .unwrap();
        assert_eq!(
            run(&fx.config, &forged_main).unwrap(),
            Outcome::NotLinkedWorktree
        );
        // A hand-built gitdir that is not under `<common>/worktrees/`, with a
        // matching back-pointer and a `commondir` aimed at the trusted repo.
        let fake_gitdir = fx.base.join("fake-gitdir");
        let forged_full = fx.base.join("forged-full");
        fs::create_dir_all(&fake_gitdir).unwrap();
        fs::create_dir_all(&forged_full).unwrap();
        fs::write(
            fake_gitdir.join("commondir"),
            fx.repo.join(".git").display().to_string(),
        )
        .unwrap();
        fs::write(
            fake_gitdir.join("gitdir"),
            forged_full.join(".git").display().to_string(),
        )
        .unwrap();
        fs::write(
            forged_full.join(".git"),
            format!("gitdir: {}\n", fake_gitdir.display()),
        )
        .unwrap();
        assert_eq!(
            run(&fx.config, &forged_full).unwrap(),
            Outcome::NotLinkedWorktree
        );
        assert_eq!(fs::read_to_string(&fx.config).unwrap(), fx.trust(&fx.repo));
    }

    #[test]
    fn a_held_lock_is_waited_on_and_then_left_alone() {
        let fx = Fixture::new("pretrust-held");
        let original = fx.trust(&fx.repo);
        fs::write(&fx.config, &original).unwrap();
        let lock = ConfigLock::path(&fx.config);
        fs::create_dir(&lock).unwrap();
        let mut waits = Vec::new();
        let outcome = pretrust_at(
            &fx.config,
            &fx.worktree,
            &mut Hooks {
                before_write: &mut || {},
                sleep: &mut |delay| waits.push(delay),
            },
        )
        .unwrap();
        assert_eq!(outcome, Outcome::LockHeld);
        assert_eq!(waits, LOCK_RETRY_DELAYS);
        assert!(lock.is_dir(), "another holder's lock must not be removed");
        assert_eq!(fs::read_to_string(&fx.config).unwrap(), original);
    }

    #[test]
    fn a_stale_lock_is_taken_over_and_released_after_the_write() {
        let fx = Fixture::new("pretrust-stale");
        fs::write(&fx.config, fx.trust(&fx.repo)).unwrap();
        let lock = ConfigLock::path(&fx.config);
        fs::create_dir(&lock).unwrap();
        age(&lock, LOCK_STALE + Duration::from_secs(5));
        assert_eq!(
            run(&fx.config, &fx.worktree).unwrap(),
            Outcome::Wrote(fx.worktree.clone())
        );
        assert!(!lock.exists(), "the lock is released after the write");
        // A lock just under the stale age is still a live holder's.
        fs::create_dir(&lock).unwrap();
        age(&lock, LOCK_STALE - Duration::from_secs(2));
        assert!(ConfigLock::try_acquire(&lock, SystemTime::now())
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_lock_taken_over_mid_write_is_not_written_through() {
        let fx = Fixture::new("pretrust-lost");
        let original = fx.trust(&fx.repo);
        fs::write(&fx.config, &original).unwrap();
        let lock = ConfigLock::path(&fx.config);
        let outcome = run_with(&fx.config, &fx.worktree, &mut || {
            age(&lock, Duration::from_secs(30));
        })
        .unwrap();
        assert_eq!(outcome, Outcome::LockLost);
        assert_eq!(fs::read_to_string(&fx.config).unwrap(), original);
    }

    #[test]
    fn orphaned_temp_files_of_dead_writers_are_swept() {
        let fx = Fixture::new("pretrust-orphans");
        fs::write(&fx.config, fx.trust(&fx.repo)).unwrap();
        // Above every platform's pid ceiling, so no process can hold it.
        let dead = fx.base.join(".flock-write-4194305-0");
        let young = fx.base.join(".flock-write-4194305-1");
        let mine = fx
            .base
            .join(format!(".flock-write-{}-9", std::process::id()));
        for path in [&dead, &young, &mine] {
            fs::write(path, "partial").unwrap();
        }
        age(&dead, Duration::from_secs(120));
        age(&mine, Duration::from_secs(120));
        run(&fx.config, &fx.worktree).unwrap();
        assert!(!dead.exists());
        assert!(young.exists(), "a fresh temp may belong to a live write");
        assert!(mine.exists());
    }
}
