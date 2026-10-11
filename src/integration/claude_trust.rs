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
//! bytes. It is re-read just before the atomic replace and recomputed once if
//! Claude changed it in between.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
    let Some(config) = config_path(spawn_env, |key| std::env::var(key).ok()) else {
        return;
    };
    let config_text = config.display().to_string();
    let (outcome, path, detail) = match pretrust_at(&config, cwd, &mut || {}) {
        Ok(Outcome::Wrote(target)) => ("wrote", target, String::new()),
        Ok(outcome) => (outcome.label(), cwd.to_path_buf(), String::new()),
        Err(err) => ("error", cwd.to_path_buf(), err.to_string()),
    };
    crate::logging::claude_pretrust(outcome, &config_text, &path.display().to_string(), &detail);
}

/// The `.claude.json` the child reads: `$CLAUDE_CONFIG_DIR/.claude.json`, else
/// `~/.claude.json`. The carried spawn environment wins over the server's own,
/// because that is what the child's environment is built from.
fn config_path(
    spawn_env: &[(String, String)],
    server_env: impl Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    let carried = spawn_env
        .iter()
        .rev()
        .find(|(key, _)| key == super::CLAUDE_CONFIG_DIR_ENV_VAR)
        .map(|(_, value)| value.clone());
    match carried
        .or_else(|| server_env(super::CLAUDE_CONFIG_DIR_ENV_VAR))
        .filter(|dir| !dir.is_empty())
    {
        Some(dir) => Some(PathBuf::from(dir).join(".claude.json")),
        None => server_env("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".claude.json")),
    }
}

/// A linked worktree and the main checkout it shares a repository with, both
/// as real paths.
#[derive(Debug, PartialEq, Eq)]
struct LinkedWorktree {
    root: PathBuf,
    main_root: PathBuf,
}

/// The linked worktree `dir` sits in, read from git's own files rather than a
/// `git` subprocess: the nearest `.git` above `dir` must be a FILE pointing at
/// a gitdir that carries a `commondir`. A `.git` directory is a main checkout,
/// and a gitdir without `commondir` is a submodule; neither is a worktree.
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
        let gitdir = root.join(pointer.trim().strip_prefix("gitdir:")?.trim());
        let commondir = fs::read_to_string(gitdir.join("commondir")).ok()?;
        let common = fs::canonicalize(gitdir.join(commondir.trim())).ok()?;
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

/// Pre-trust `cwd` in the `.claude.json` at `config`. `before_write` runs
/// between the decision and the re-read, which is where a concurrent Claude
/// rewrite lands.
fn pretrust_at(config: &Path, cwd: &Path, before_write: &mut dyn FnMut()) -> io::Result<Outcome> {
    let cwd = fs::canonicalize(cwd)?;
    let Some(worktree) = linked_worktree(&cwd) else {
        return Ok(Outcome::NotLinkedWorktree);
    };
    if super::mcp_config::externally_owned_at(config) {
        return Ok(Outcome::ExternallyOwned);
    }
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
        before_write();
        let latest = match fs::read_to_string(config) {
            Ok(latest) => latest,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Outcome::NoConfig),
            Err(err) => return Err(err),
        };
        if latest != content {
            continue;
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
        let outcome = pretrust_at(&fx.config, &fx.worktree, &mut || {}).unwrap();
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
            pretrust_at(&fx.config, &fx.worktree, &mut || {}).unwrap(),
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
            pretrust_at(&fx.config, &fx.worktree, &mut || {}).unwrap(),
            Outcome::SourceUntrusted
        );
        // #365: a plain directory, even one whose parent is trusted, is never
        // a target.
        assert_eq!(
            pretrust_at(&fx.config, &elsewhere, &mut || {}).unwrap(),
            Outcome::NotLinkedWorktree
        );
        // The main checkout is not a linked worktree either.
        assert_eq!(
            pretrust_at(&fx.config, &fx.repo, &mut || {}).unwrap(),
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
            pretrust_at(&fx.config, &fx.worktree.join(sub), &mut || {}).unwrap(),
            Outcome::Wrote(fx.worktree.join("pkg"))
        );
        assert!(fx.trusted(&fx.worktree.join("pkg")));
        assert!(!fx.trusted(&fx.worktree));
        assert_eq!(
            pretrust_at(&fx.config, &fx.worktree, &mut || {}).unwrap(),
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
            pretrust_at(&fx.config, &wt_link, &mut || {}).unwrap(),
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
            pretrust_at(&fx.config, &fx.worktree, &mut || {}).unwrap(),
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
        pretrust_at(&fx.config, &fx.worktree, &mut || {}).unwrap();
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
        let outcome = pretrust_at(&fx.config, &fx.worktree, &mut || {
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
        let err = pretrust_at(&fx.config, &fx.worktree, &mut || {
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
    fn config_path_prefers_the_carried_profile() {
        let server = |key: &str| match key {
            "HOME" => Some("/fixture-home".to_string()),
            "CLAUDE_CONFIG_DIR" => Some("/fixture-server-profile".to_string()),
            _ => None,
        };
        let carried = vec![("CLAUDE_CONFIG_DIR".to_string(), "/p/work".to_string())];
        assert_eq!(
            config_path(&carried, server),
            Some(PathBuf::from("/p/work/.claude.json"))
        );
        assert_eq!(
            config_path(&[], server),
            Some(PathBuf::from("/fixture-server-profile/.claude.json"))
        );
        let home_only = |key: &str| (key == "HOME").then(|| "/fixture-home".to_string());
        assert_eq!(
            config_path(&[], home_only),
            Some(PathBuf::from("/fixture-home/.claude.json"))
        );
        assert_eq!(config_path(&[], |_| None), None);
    }
}
