use std::path::{Path, PathBuf};

const DEFAULT_WORKTREE_PREFIX: &str = "worktree";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExistingWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
}

pub(crate) fn generated_branch_slug(seed: u64) -> String {
    let adjectives = [
        "brave", "calm", "clear", "green", "lucky", "quiet", "rapid", "silver",
    ];
    let nouns = [
        "river", "cloud", "field", "forest", "harbor", "meadow", "stone", "valley",
    ];
    let adjective = adjectives[(seed as usize) % adjectives.len()];
    let noun = nouns[((seed / adjectives.len() as u64) as usize) % nouns.len()];
    let suffix = seed & 0xffff;
    format!("{DEFAULT_WORKTREE_PREFIX}/{adjective}-{noun}-{suffix:04x}")
}

pub(crate) fn branch_to_path_slug(branch: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in branch.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        DEFAULT_WORKTREE_PREFIX.to_string()
    } else {
        trimmed
    }
}

pub(crate) fn expand_tilde_path(path: &str) -> PathBuf {
    if path == "~" {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(path));
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|_| PathBuf::from(path));
    }

    PathBuf::from(path)
}

pub(crate) fn expand_tilde_absolute_path(path: &str) -> PathBuf {
    let path = expand_tilde_path(path);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

pub(crate) fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The directory that namespaces one repo's worktrees, keyed on the repo's
/// **git common dir** — the same identity `worktree_space_here` compares, and
/// the same one the sidebar groups on.
///
/// A basename is not an identity (#212). `~/Projects/nucl-parquet` and the
/// `nucl-parquet` submodule inside `hyrr` both answer "nucl-parquet", so they
/// shared one base and their worktrees intermingled with only the branch slug
/// telling them apart. Parent-qualifying doesn't fix it either — `/a/x/repo`
/// and `/b/x/repo` still collide. Only the repo's own identity does.
///
/// The label is kept as a readable prefix; the suffix is what makes it
/// collision-free. Deriving both from `repo_key` keeps the name a pure function
/// of the repo, so it never depends on which repos happen to be open.
pub(crate) fn worktree_base_dir_name(repo_name: &str, repo_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let canonical = canonical_or_original(Path::new(repo_key));
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let short: String = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let label = branch_to_path_slug(repo_name);
    format!("{label}-{short}")
}

/// Where a new worktree for `repo_key` on `branch` is proposed to live.
///
/// Only ever a *proposal*: an existing worktree is found through its recorded
/// `checkout_path`, never by re-deriving this. That is why #212's rename is
/// safe — worktrees created under the old basename-only scheme keep working.
pub(crate) fn default_checkout_path(
    root: &Path,
    repo_name: &str,
    repo_key: &str,
    branch: &str,
) -> PathBuf {
    root.join(worktree_base_dir_name(repo_name, repo_key))
        .join(branch_to_path_slug(branch))
}

pub(crate) fn build_worktree_remove_command(
    repo_root: &Path,
    path: &Path,
    force: bool,
) -> WorktreeCommand {
    let mut args = vec![
        "-C".to_string(),
        repo_root.display().to_string(),
        "worktree".to_string(),
        "remove".to_string(),
    ];
    if force {
        args.push("--force".to_string());
    }
    args.push(path.display().to_string());

    WorktreeCommand {
        program: "git".to_string(),
        args,
    }
}

pub(crate) fn is_dirty_worktree_remove_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("contains modified or untracked files")
        && lower.contains("use --force to delete it")
}

pub(crate) fn build_worktree_add_new_branch_command(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> WorktreeCommand {
    WorktreeCommand {
        program: "git".to_string(),
        args: vec![
            "-C".to_string(),
            repo_root.display().to_string(),
            "worktree".to_string(),
            "add".to_string(),
            "-b".to_string(),
            branch.to_string(),
            path.display().to_string(),
            base.to_string(),
        ],
    }
}

/// Translate the `git worktree add` failures that read as a flock bug (#198,
/// #243) into git's own words plus the actual remedy.
///
/// A repo that has been `git init`-ed but never committed has an unborn HEAD,
/// so branching from the default base fails with `fatal: invalid reference:
/// HEAD`; a directory that is not a repo at all fails with `fatal: not a git
/// repository (…)`. Both name the symptom and neither names the fix.
///
/// The remedy goes on its own line, and each line is kept inside the create
/// dialog's width so the wrapped error area shows the whole thing — the
/// original two-paragraph phrasing was silently clipped to git's line alone,
/// which is exactly the half that doesn't help (#243).
///
/// Only the default base is rewritten: `invalid reference: <some-branch>` for
/// an explicit base is a genuinely different problem (a typo, or a branch that
/// isn't there) and git already names it.
pub(crate) fn explain_worktree_add_failure(base: &str, message: &str) -> String {
    if base == "HEAD" && message.contains("invalid reference: HEAD") {
        return format!("{message}\nno commits yet — make one, then branch a worktree.");
    }
    if message.contains("not a git repository") {
        // git's parenthetical ("or any of the parent directories") is dropped:
        // it doubles the length and the remedy is the same either way.
        return "fatal: not a git repository\nrun `git init` and commit, then branch a worktree."
            .to_string();
    }
    message.to_string()
}

pub(crate) fn run_worktree_command(command: &WorktreeCommand) -> Result<(), String> {
    let output = crate::process::TracedCommand::new(&command.program, "worktree")
        .args(&command.args)
        .output_traced()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        // A worktree add/remove/move relocates repo paths on disk, so every
        // memoized path canonicalization must be re-derived (#262).
        crate::workspace::git::invalidate_path_canonicalization();
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(if message.is_empty() {
        format!("{} failed with status {}", command.program, output.status)
    } else {
        message
    })
}

/// Evidence-gated decision for deleting a worktree's local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeMergeGate {
    /// The branch's work is recorded elsewhere; deleting it is safe.
    Merged { evidence: String },
    /// No merge evidence found; only the checkout should be removed.
    NotMerged,
}

fn run_command_capture(
    program: &str,
    args: &[&str],
    cwd: Option<&std::path::Path>,
) -> Result<String, String> {
    let mut command = crate::process::TracedCommand::new(program, "worktree");
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output_traced().map_err(|err| err.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("{program} failed with status {}", output.status)
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Main checkout root derived from a git common dir: "…/repo/.git" -> "…/repo";
/// bare-style common dirs are returned unchanged (git -C works there too).
pub(crate) fn main_root_from_common_dir(common_dir: &std::path::Path) -> std::path::PathBuf {
    if common_dir.file_name().and_then(|name| name.to_str()) == Some(".git") {
        common_dir
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| common_dir.to_path_buf())
    } else {
        common_dir.to_path_buf()
    }
}

/// Branch checked out in `checkout`, if any (detached HEAD yields None).
pub(crate) fn checkout_branch_name(checkout: &std::path::Path) -> Option<String> {
    let path = checkout.to_string_lossy().to_string();
    run_command_capture("git", &["-C", &path, "branch", "--show-current"], None)
        .ok()
        .filter(|branch| !branch.is_empty())
}

/// A branch the worktree-kill flow must never auto-delete (#121). Three tiers,
/// most authoritative first:
///   1. The `main`/`master` floor — hardcoded and config-INDEPENDENT, so a repo
///      with no config (or a failed default-branch probe) can never have these
///      pruned.
///   2. The repo's detected default branch (`origin/HEAD`, else main/master).
///   3. `extra_protected` — the `[worktrees] protected_branches` repo policy,
///      which EXTENDS the floor (long-lived `develop`, `release/*`, ...). It can
///      only add protection, never remove tier 1.
pub(crate) fn is_protected_branch(
    branch: &str,
    default_branch: Option<&str>,
    extra_protected: &[String],
) -> bool {
    matches!(branch, "main" | "master")
        || default_branch == Some(branch)
        || extra_protected.iter().any(|b| b == branch)
}

/// Whether `checkout` has uncommitted or untracked changes. `None` when git
/// can't be queried — callers treat unknown as "assume dirty" and skip rather
/// than risk a destructive remove on a guess.
pub(crate) fn checkout_is_dirty(checkout: &std::path::Path) -> Option<bool> {
    let path = checkout.to_string_lossy().to_string();
    run_command_capture("git", &["-C", &path, "status", "--porcelain"], None)
        .ok()
        .map(|status| !status.trim().is_empty())
}

/// What a kill would actually destroy, beyond the checkout folder (#325).
///
/// Every field distinguishes "nothing" from "could not tell". The dialog says
/// so out loud rather than rendering an unreadable repo as clean — the whole
/// point of showing this is that the user is authorising a destructive act
/// against a named set, and an unnamed set is what they had before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KillProbe {
    /// Uncommitted and untracked paths, as `git status --porcelain` name them.
    /// `Some(vec![])` is a clean checkout; `None` is "git could not be asked".
    pub dirty: Option<Vec<String>>,
    /// Commits on the branch that no remote ref holds — the other thing
    /// deleting a branch can lose. `None` when it could not be counted.
    pub unpushed: Option<usize>,
}

impl KillProbe {
    /// Is there anything here whose loss is worth a second look? Unknown counts
    /// as yes: it is the reading that makes the user check.
    pub fn has_stakes(&self) -> bool {
        !matches!(self.dirty.as_deref(), Some([])) || self.unpushed.is_none_or(|count| count > 0)
    }
}

/// Collect what a kill would destroy in `checkout`, for the branch it holds.
///
/// Runs on the caller's thread — the kill dialog calls it from the same worker
/// that resolves the merge gate, so it inherits that thread and never touches
/// the UI one. Every failure degrades to `None` (unknown) rather than an empty
/// answer; see [`KillProbe`].
///
/// `-uall` lists untracked files individually instead of collapsing a directory
/// to `dir/`, because "one untracked directory" is exactly the summary that
/// hides how much is about to go.
pub(crate) fn probe_kill_targets(checkout: &std::path::Path, branch: Option<&str>) -> KillProbe {
    let path = checkout.to_string_lossy().to_string();
    let dirty = run_command_capture(
        "git",
        &["-C", &path, "status", "--porcelain=v1", "-uall"],
        None,
    )
    .ok()
    .map(|status| {
        status
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    });
    // `--not --remotes` subtracts every remote-tracking ref, not just this
    // branch's upstream: work pushed to a fork, or landed on another branch's
    // remote, is recorded somewhere and is not what this warning is for.
    let unpushed = branch.and_then(|branch| {
        run_command_capture(
            "git",
            &[
                "-C",
                &path,
                "rev-list",
                "--count",
                &branch_ref(branch),
                "--not",
                "--remotes",
            ],
            None,
        )
        .ok()
        .and_then(|count| count.trim().parse::<usize>().ok())
    });
    KillProbe { dirty, unpushed }
}

/// What the all-worktrees sweep (#81) does to ONE worktree, decided from its
/// resolved state. Ordered safest → most aggressive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillTier {
    /// Main checkout, clean, no running agent: close its flk pane/workspace
    /// only — nothing is touched on disk.
    ClosePane,
    /// Linked worktree whose branch is merged: remove the checkout AND delete
    /// the local branch. `dirty` flags uncommitted scratch that will be lost
    /// (the committed work is recorded elsewhere, so it is safe).
    KillBranch { dirty: bool },
    /// Linked worktree, not merged, clean: remove the checkout, keep the branch.
    CheckoutOnly,
    /// Linked worktree, not merged, with uncommitted/untracked work: skipped on
    /// the default sweep; only force escalates it to a checkout-only removal.
    SkipUnmergedDirty,
    /// Main checkout with uncommitted changes: left alone.
    SkipMainDirty,
    /// A running/working agent lives here: never disturbed by the sweep.
    SkipAgent,
}

/// The resolved state of one worktree feeding [`classify_kill_tier`] (#81).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillFacts {
    /// This workspace is the repo's main checkout, not a linked worktree.
    pub is_main: bool,
    /// A pane in this workspace has a working agent.
    pub working_agent: bool,
    /// The checkout has uncommitted/untracked changes.
    pub dirty: bool,
    /// The branch merge gate found positive evidence.
    pub merged: bool,
}

/// Map a worktree's resolved facts to its sweep tier (#81). Pure so the tier
/// policy is exhaustively testable. A running agent wins over everything — the
/// sweep never disturbs active work, main or linked.
pub fn classify_kill_tier(facts: KillFacts) -> KillTier {
    if facts.working_agent {
        return KillTier::SkipAgent;
    }
    if facts.is_main {
        return if facts.dirty {
            KillTier::SkipMainDirty
        } else {
            KillTier::ClosePane
        };
    }
    if facts.merged {
        KillTier::KillBranch { dirty: facts.dirty }
    } else if facts.dirty {
        KillTier::SkipUnmergedDirty
    } else {
        KillTier::CheckoutOnly
    }
}

/// The concrete operation the sweep performs on a row this pass (#81).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillAction {
    /// Close the flk pane/workspace; nothing on disk.
    ClosePane,
    /// `git worktree remove` then `git branch -D`. `dirty` ⇒ force-remove.
    KillBranch { dirty: bool },
    /// `git worktree remove`, keep the branch.
    CheckoutOnly,
    /// #175 S2: `git worktree move <checkout> <quarantine_dst>`; branch is
    /// PRESERVED. This is what the SCHEDULED reap yields for tiers that
    /// today would `Skip` — never a delete without evidence. Human CLI
    /// paths never emit this variant (see [`scheduled_action`]).
    Quarantine,
    /// Do nothing this pass.
    Skip,
}

/// Resolve a tier to the action taken this pass, given whether the user has
/// engaged `force` (#81). Force only ever ESCALATES the otherwise-skipped
/// unmerged-dirty rows to a checkout-only removal — it never deletes a branch
/// without merge evidence, and never touches the protected skips.
pub fn planned_action(tier: KillTier, force_dirty: bool) -> KillAction {
    match tier {
        KillTier::ClosePane => KillAction::ClosePane,
        KillTier::KillBranch { dirty } => KillAction::KillBranch { dirty },
        KillTier::CheckoutOnly => KillAction::CheckoutOnly,
        KillTier::SkipUnmergedDirty => {
            if force_dirty {
                KillAction::CheckoutOnly
            } else {
                KillAction::Skip
            }
        }
        KillTier::SkipMainDirty | KillTier::SkipAgent => KillAction::Skip,
    }
}

/// #175 S2: scheduled-reap variant of [`planned_action`]. The scheduled
/// caller flag replaces the tiers that today `Skip`-because-unmerged/dirty
/// with [`KillAction::Quarantine`] — atomic move, branch preserved. Every
/// other tier resolves exactly as the human `planned_action` does; the
/// merge-gated `KillBranch` still requires positive evidence, and the
/// protected `SkipMainDirty` / `SkipAgent` tiers still skip.
///
/// The invariant this function encodes is: SCHEDULED code paths can never
/// delete a branch without merge evidence, and never touch main-dirty /
/// active-agent rows. Human `flk worktree kill --force` retains its
/// escalation-to-checkout-only semantics via [`planned_action`].
pub fn scheduled_action(tier: KillTier) -> KillAction {
    match tier {
        KillTier::ClosePane => KillAction::ClosePane,
        KillTier::KillBranch { dirty } => KillAction::KillBranch { dirty },
        KillTier::CheckoutOnly => KillAction::CheckoutOnly,
        // The skip-because-unmerged/dirty tier becomes Quarantine under
        // scheduled reap: preserve the checkout by moving it, keep the
        // branch alive. §8.5 adversarial rows land here.
        KillTier::SkipUnmergedDirty => KillAction::Quarantine,
        // Protected tiers stay protected. A running agent is never
        // disturbed; main-dirty is never quarantined either.
        KillTier::SkipMainDirty | KillTier::SkipAgent => KillAction::Skip,
    }
}

/// What the spoke found (and did) when preparing a branch for a cross-machine
/// checkout (#125). The spoke runs this on ITS OWN repo; the hub then fetches
/// the branch from origin — each node touches only its own git (hub-spoke).
// Driven by the peers.checkout_prepare RPC handler (#125).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PeerCheckoutReport {
    /// The peer's working tree had uncommitted changes. Only committed refs
    /// transfer — the hub gets the last commit, not live edits.
    pub was_dirty: bool,
    /// The branch had no upstream, or local commits origin lacked, before this
    /// ran (i.e. a push was needed to make it fetchable).
    pub was_unpushed: bool,
    /// A push to origin was performed by this call.
    pub pushed: bool,
}

/// Prepare a branch on the spoke for the hub to check out (#125, "defer to the
/// client"): probe the working-tree/upstream state, then — when `push` —
/// `git push -u origin <branch>` so a plain `git fetch origin <branch>` on the
/// hub brings it across. `push == false` is a read-only probe for the hub's
/// pre-action confirmations. The spoke owns its own git; the hub never reaches
/// into it.
pub(crate) fn prepare_peer_checkout(
    repo: &std::path::Path,
    branch: &str,
    push: bool,
) -> Result<PeerCheckoutReport, String> {
    let repo = repo.to_string_lossy().to_string();
    run_command_capture(
        "git",
        &[
            "-C",
            &repo,
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        None,
    )
    .map_err(|_| format!("branch '{branch}' not found on peer"))?;

    let status = run_command_capture("git", &["-C", &repo, "status", "--porcelain"], None)?;
    let was_dirty = !status.trim().is_empty();

    // `@{upstream}` resolves a branch NAME, not a ref — `refs/heads/x@{u}` is
    // rejected outright — so this one stays bare. That is safe: the suffix only
    // ever consults refs/heads, so a same-named tag cannot capture it.
    let upstream = run_command_capture(
        "git",
        &[
            "-C",
            &repo,
            "rev-parse",
            "--abbrev-ref",
            &format!("{branch}@{{upstream}}"),
        ],
        None,
    )
    .ok()
    .filter(|up| !up.is_empty());
    let was_unpushed = match &upstream {
        None => true,
        // The range endpoint IS a commit-ish, so it must be qualified: a tag
        // sharing the branch's name counts the wrong commits and reports a
        // fully-pushed branch as unpushed (#243).
        Some(up) => run_command_capture(
            "git",
            &[
                "-C",
                &repo,
                "rev-list",
                "--count",
                &format!("{up}..refs/heads/{branch}"),
            ],
            None,
        )
        .map(|ahead| ahead.trim() != "0")
        .unwrap_or(true),
    };

    let pushed = if push {
        run_command_capture("git", &["-C", &repo, "push", "-u", "origin", branch], None)
            .map_err(|err| format!("push to origin failed: {err}"))?;
        true
    } else {
        false
    };

    Ok(PeerCheckoutReport {
        was_dirty,
        was_unpushed,
        pushed,
    })
}

/// Bring a peer's branch across to this machine (#125, the hub's local leg):
/// `git fetch origin <branch>` into a local checkout of the project, then add a
/// linked worktree on it — reusing an existing local branch when present, else
/// creating one tracking `origin/<branch>`. Returns the new checkout path. The
/// spoke already pushed the branch (peers.checkout_prepare with push); this
/// only ever touches the hub's own git, so the model stays hub-spoke.
pub(crate) fn fetch_and_add_peer_worktree(
    repo_dir: &Path,
    worktree_directory: &Path,
    repo_name: &str,
    branch: &str,
) -> Result<PathBuf, String> {
    let dir = repo_dir.to_string_lossy().to_string();
    run_command_capture("git", &["-C", &dir, "fetch", "origin", branch], None)
        .map_err(|err| format!("git fetch origin {branch} failed: {err}"))?;

    // Identity for the namespace is the repo's common dir (#212); derive it
    // from the checkout we were handed rather than trusting the basename.
    let repo_key = crate::workspace::git_space_metadata(repo_dir)
        .map(|space| space.key)
        .unwrap_or_else(|| canonical_or_original(repo_dir).display().to_string());
    let checkout_path = default_checkout_path(worktree_directory, repo_name, &repo_key, branch);
    if let Some(parent) = checkout_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let path = checkout_path.display().to_string();

    // A local branch of this name may already exist (the hub worked on it
    // before): check it out directly. Otherwise create one tracking the freshly
    // fetched origin branch.
    let local_exists = run_command_capture(
        "git",
        &[
            "-C",
            &dir,
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        None,
    )
    .is_ok();
    let command = if local_exists {
        WorktreeCommand {
            program: "git".to_string(),
            args: vec![
                "-C".to_string(),
                dir,
                "worktree".to_string(),
                "add".to_string(),
                path,
                branch.to_string(),
            ],
        }
    } else {
        WorktreeCommand {
            program: "git".to_string(),
            args: vec![
                "-C".to_string(),
                dir,
                "worktree".to_string(),
                "add".to_string(),
                "--track".to_string(),
                "-b".to_string(),
                branch.to_string(),
                path,
                format!("origin/{branch}"),
            ],
        }
    };
    run_worktree_command(&command)?;
    Ok(checkout_path)
}

/// The repo's default branch: origin/HEAD when set, else main/master if present.
pub(crate) fn detect_default_branch(repo_root: &std::path::Path) -> Option<String> {
    let root = repo_root.to_string_lossy().to_string();
    if let Ok(full) = run_command_capture(
        "git",
        &[
            "-C",
            &root,
            "symbolic-ref",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        None,
    ) {
        if let Some(branch) = full.strip_prefix("origin/") {
            return Some(branch.to_string());
        }
    }
    for candidate in ["main", "master"] {
        let probe = format!("refs/heads/{candidate}");
        if run_command_capture(
            "git",
            &["-C", &root, "show-ref", "--verify", "--quiet", &probe],
            None,
        )
        .is_ok()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// GitHub PR state for a worktree branch, shown in the pane header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrStateInfo {
    pub state: PrState,
    pub number: u64,
}

/// The state mapping itself, shared by the `gh --json` shape and the batched
/// GraphQL shape (#294). Both wire formats spell the fields identically
/// (`state`, `number`, `isDraft`); keeping one mapping is what stops the two
/// transports drifting on, say, whether a draft counts as open.
pub(crate) fn parse_pr_state_fields(
    state: &str,
    number: u64,
    is_draft: Option<bool>,
) -> Option<PrStateInfo> {
    let state = match state {
        "OPEN" if is_draft == Some(true) => PrState::Draft,
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => return None,
    };
    Some(PrStateInfo { state, number })
}

/// Resolve `owner/repo` for a checkout's origin remote.
///
/// Split out of `query_pr_state` (#294): the batched poller resolves this once
/// per REPO per round instead of once per target, which removes the second of
/// the two process spawns each target used to cost. The value is static for a
/// session — a repo's origin does not move under us.
pub(crate) fn github_repo_for_root(repo_root: &std::path::Path) -> Option<String> {
    let root = repo_root.to_string_lossy().to_string();
    run_command_capture("git", &["-C", &root, "remote", "get-url", "origin"], None)
        .ok()
        .as_deref()
        .and_then(github_repo_from_remote_url)
}

/// Parse "owner/repo" out of a github remote URL (ssh or https).
fn github_repo_from_remote_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let repo = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = repo.splitn(3, '/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// gh matches PRs by branch NAME; commits added locally after the merge
/// would not be covered by that evidence. Only accept it when the PR's head
/// equals the local tip — otherwise fall through to the tip-exact checks.
fn gh_pr_merged_evidence(
    args: &[&str],
    cwd: &std::path::Path,
    local_tip: Option<&str>,
) -> Option<String> {
    let json = run_command_capture("gh", args, Some(cwd)).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&json).ok()?;
    if value.get("state").and_then(|v| v.as_str()) != Some("MERGED") {
        return None;
    }
    let head_oid = value.get("headRefOid").and_then(|v| v.as_str())?;
    if local_tip != Some(head_oid) {
        return None;
    }
    Some(match value.get("number").and_then(|v| v.as_u64()) {
        Some(number) => format!("PR #{number} merged"),
        None => "PR merged".to_string(),
    })
}

/// Does every commit reachable from `branch` also live on some *other* ref?
///
/// This is the "nothing to lose" case, not a merge: an accidental worktree that
/// was branched and never committed to has a tip identical to whatever it was
/// branched from, so deleting it discards no work at all. The three evidence
/// sources above it all miss that whenever the base is itself unpushed and not
/// on the default branch — branch a session off a local feature branch, change
/// your mind, and the kill dialog would refuse to clean up after itself (#243).
///
/// The branch's own refs are excluded on both sides: `refs/heads/<branch>`
/// (which would otherwise make the count trivially zero) and
/// `refs/remotes/*/<branch>` (its own tracking ref is not independent
/// evidence — the remote-containment check below deliberately ignores it too).
/// An unreadable count is not evidence: `false` keeps the branch.
fn branch_has_no_unique_commits(root: &str, branch: &str) -> bool {
    let own_remote = format!("*/{branch}");
    run_command_capture(
        "git",
        &[
            "-C",
            root,
            "rev-list",
            "--count",
            // Fully qualified: a same-named tag outranks the branch in git's
            // ambiguity order, and walking the tag's commit instead would make
            // the branch's own work look like it lives elsewhere (#243).
            &branch_ref(branch),
            "--not",
            // --exclude applies to the *next* ref glob, and its pattern is
            // relative to that glob's namespace: bare name for --branches,
            // remote-qualified for --remotes.
            "--exclude",
            branch,
            "--branches",
            "--exclude",
            &own_remote,
            "--remotes",
        ],
        None,
    )
    .ok()
    .and_then(|count| count.trim().parse::<u64>().ok())
    .is_some_and(|count| count == 0)
}

/// `refs/heads/<branch>` — the unambiguous spelling for any git argument that
/// takes a commit-ish.
///
/// A bare branch name is ambiguous: `refs/tags/<name>` is checked before
/// `refs/heads/<name>`, so with a release tag and a hotfix branch sharing a
/// name (`v1.0`), every commit-ish argument in the gate silently resolves to
/// the tag. That reads the wrong history and, because the tag usually sits on
/// the default branch, produces *positive* deletion evidence for a branch whose
/// work exists nowhere else (#243).
fn branch_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

/// PR-merged gate for deleting `branch`. Evidence sources, in order:
/// 1. `gh pr view` with gh's own repo resolution, then pinned to the origin
///    remote's repo (multi-remote checkouts resolve to upstream otherwise).
/// 2. `git branch --merged <default-branch>`.
/// 3. Remote containment: the branch tip is reachable from another pushed
///    remote ref (e.g. merged into a feature branch) — the work is recorded,
///    so deleting the local branch loses nothing.
/// 4. The branch holds no commits of its own — every commit on it is already
///    on some other ref, local or remote, so there is nothing to lose (#243).
///
/// Anything inconclusive is NotMerged — deletion needs positive evidence.
///
/// (4) is deliberately last even though it is the only purely-local check and
/// would short-circuit the `gh` call. A disposable branch is very often *also*
/// merged or remotely contained, and "merged into main" says more about where
/// the work went than (4) can. Latency is the cheaper thing to spend here —
/// the gate already runs off the UI thread behind "checking merge status…".
pub(crate) fn branch_merge_gate(
    repo_root: &std::path::Path,
    checkout: &std::path::Path,
    branch: &str,
) -> WorktreeMergeGate {
    let root = repo_root.to_string_lossy().to_string();
    let local_tip = run_command_capture(
        "git",
        &["-C", &root, "rev-parse", &branch_ref(branch)],
        None,
    )
    .ok();
    if let Some(evidence) = gh_pr_merged_evidence(
        &["pr", "view", branch, "--json", "state,number,headRefOid"],
        checkout,
        local_tip.as_deref(),
    ) {
        return WorktreeMergeGate::Merged { evidence };
    }
    if let Some(repo) =
        run_command_capture("git", &["-C", &root, "remote", "get-url", "origin"], None)
            .ok()
            .as_deref()
            .and_then(github_repo_from_remote_url)
    {
        if let Some(evidence) = gh_pr_merged_evidence(
            &[
                "pr",
                "view",
                branch,
                "--repo",
                &repo,
                "--json",
                "state,number,headRefOid",
            ],
            checkout,
            local_tip.as_deref(),
        ) {
            return WorktreeMergeGate::Merged { evidence };
        }
    }

    if let Some(default_branch) = detect_default_branch(repo_root) {
        if let Ok(merged) = run_command_capture(
            "git",
            &[
                "-C",
                &root,
                "branch",
                "--merged",
                // The *base* is a commit-ish too, and just as shadowable: a
                // tag named after the default branch answers this question
                // about the wrong commit (#243).
                &branch_ref(&default_branch),
                "--format",
                "%(refname:short)",
            ],
            None,
        ) {
            // The listed names are compared bare. When a tag shadows one of
            // them git shortens it to `heads/<name>` instead, which matches
            // nothing here — the branch is kept, which is the safe answer.
            if merged.lines().any(|line| line.trim() == branch) {
                return WorktreeMergeGate::Merged {
                    evidence: format!("merged into {default_branch}"),
                };
            }
        }
    }

    // Remote containment: any remote ref other than the branch's own
    // tracking ref that contains the tip.
    if let Some(remote_ref) = refs_containing(&root, branch, RefScope::Remote).first() {
        return WorktreeMergeGate::Merged {
            evidence: format!("contained in {remote_ref}"),
        };
    }

    if branch_has_no_unique_commits(&root, branch) {
        // Name the ref that holds them where one exists. The unnamed phrasing
        // is true but tells the user nothing about where their work went. For
        // the accidental-worktree case it is almost always the base branch the
        // session was cut from, which is worth saying out loud (#243).
        //
        // A zero count does not guarantee a single containing ref — the commits
        // may be spread over several — so the unnamed phrasing stays as the
        // fallback rather than being replaced.
        let evidence = match refs_containing(&root, branch, RefScope::All).first() {
            Some(holder) => format!("contained in {holder}"),
            None => "no commits of its own".to_string(),
        };
        return WorktreeMergeGate::Merged { evidence };
    }

    // Squash merges leave no ancestry to find (#287 landed that way and this
    // gate refused to clean up after it): the content is replayed as ONE new
    // commit, so no commit of the branch is ever an ancestor of the default
    // branch, and every source above asks an ancestry question. Ask the
    // question the gate actually cares about instead — would deleting this
    // lose work? — by merging the branch into the default branch in memory.
    if let Some(default_branch) = detect_default_branch(repo_root) {
        if branch_content_already_in(&root, branch, &default_branch) {
            return WorktreeMergeGate::Merged {
                evidence: format!("already in {default_branch} (squashed or rebased)"),
            };
        }
    }

    WorktreeMergeGate::NotMerged
}

/// Would merging `branch` into `base` change anything?
///
/// `git merge-tree --write-tree` computes the merged tree without touching the
/// index or the working tree. When it comes back equal to `base`'s own tree,
/// the branch contributes nothing base does not already have — which is true
/// of a squash-merged branch, a rebase-merged one, and a branch whose change
/// someone else landed by another route. In every one of those cases deleting
/// the local branch loses no work, which is the only thing this gate is for.
///
/// Chosen over the obvious tree comparison, which is wrong in a way that shows
/// up immediately: diffing the branch against `base` over the paths it touched
/// reports base's OWN later edits to those files as missing content, so the
/// check starts failing the moment the default branch moves on.
///
/// Anything unusable is NOT evidence, and the branch is kept: a merge that
/// conflicts (non-zero exit), a git too old for `--write-tree` (< 2.38), an
/// unreadable ref. Silence here costs a manual cleanup; a false positive costs
/// someone's work.
fn branch_content_already_in(root: &str, branch: &str, base: &str) -> bool {
    let Ok(base_tree) = run_command_capture(
        "git",
        &[
            "-C",
            root,
            "rev-parse",
            &format!("{}^{{tree}}", branch_ref(base)),
        ],
        None,
    ) else {
        return false;
    };
    let Ok(merged_tree) = run_command_capture(
        "git",
        &[
            "-C",
            root,
            "merge-tree",
            "--write-tree",
            &branch_ref(base),
            &branch_ref(branch),
        ],
        None,
    ) else {
        return false;
    };
    // `--write-tree` prints the tree oid on the first line; a conflicted merge
    // exits non-zero and is already handled above.
    let merged_tree = merged_tree.lines().next().unwrap_or_default().trim();
    !base_tree.is_empty() && merged_tree == base_tree
}

/// Which refs [`refs_containing`] considers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RefScope {
    /// Remote-tracking refs only — the work is recorded off this machine.
    Remote,
    /// Local branches as well; enough to say the commits are not unique to
    /// this branch, which is all the last evidence source claims.
    All,
}

/// Display names of the refs that contain `branch`'s tip, in git's own listing
/// order (local branches before remote-tracking ones), excluding refs that are
/// not independent evidence about it:
///
/// - the branch's own local ref and its tracking refs on every remote, since a
///   ref cannot vouch for itself;
/// - `refs/remotes/<remote>/HEAD`, which is a symbolic pointer at the remote's
///   default branch and duplicates the branch it names. It has to be filtered
///   on the *full* refname: `%(refname:short)` renders it as bare `origin`, so
///   a `contains("HEAD")` filter misses it and the gate reports the meaningless
///   `contained in origin` (#243).
fn refs_containing(root: &str, branch: &str, scope: RefScope) -> Vec<String> {
    let scope_flag = match scope {
        RefScope::Remote => "-r",
        RefScope::All => "-a",
    };
    let own_remote_suffix = format!("/{branch}");
    run_command_capture(
        "git",
        &[
            "-C",
            root,
            "branch",
            scope_flag,
            "--contains",
            &branch_ref(branch),
            "--format",
            // Full refnames: the short form is ambiguous exactly where the
            // filtering has to be precise.
            "%(refname)",
        ],
        None,
    )
    .map(|out| {
        out.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter(|line| !line.ends_with("/HEAD"))
            .filter(|line| {
                *line != branch_ref(branch)
                    && !(line.starts_with("refs/remotes/") && line.ends_with(&own_remote_suffix))
            })
            .map(|line| {
                line.strip_prefix("refs/remotes/")
                    .or_else(|| line.strip_prefix("refs/heads/"))
                    .unwrap_or(line)
                    .to_string()
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Upper bound on how long the kill dialog waits for the merge gate before it
/// gives up (#119). `branch_merge_gate` shells out to `gh pr view` — a network
/// call with no timeout of its own — so an offline/unauthenticated/slow `gh`
/// would otherwise wedge the dialog on "checking merge status…" forever.
pub(crate) const MERGE_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// Run the merge gate with a hard wall-clock bound. Returns `(gate, timed_out)`:
/// on timeout the gate degrades to the safe `NotMerged` (checkout-only, branch
/// kept) and `timed_out` is `true` so the dialog can label it honestly. The
/// worker keeps running in the background — a genuinely hung `gh` is orphaned
/// rather than killed, which is fine: nothing reads its result and it exits on
/// its own network timeout.
pub(crate) fn branch_merge_gate_with_timeout(
    repo_root: PathBuf,
    checkout: PathBuf,
    branch: String,
) -> (WorktreeMergeGate, bool) {
    resolve_gate_with_timeout(
        move || branch_merge_gate(&repo_root, &checkout, &branch),
        MERGE_GATE_TIMEOUT,
    )
}

/// Bound any gate computation by `timeout`. Extracted from
/// [`branch_merge_gate_with_timeout`] so the timeout policy is testable without
/// shelling out to `gh`/git.
fn resolve_gate_with_timeout<F>(work: F, timeout: std::time::Duration) -> (WorktreeMergeGate, bool)
where
    F: FnOnce() -> WorktreeMergeGate + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    match rx.recv_timeout(timeout) {
        Ok(gate) => (gate, false),
        Err(_) => (WorktreeMergeGate::NotMerged, true),
    }
}

/// `git branch -D <branch>` in `repo_root`. Only called once the merge gate
/// produced positive evidence; -D because -d judges merges against the
/// current HEAD, not the default branch.
pub(crate) fn delete_local_branch(repo_root: &std::path::Path, branch: &str) -> Result<(), String> {
    // Last-ditch floor (#121): the primary branch is never deletable by this
    // path, whatever a caller decided. Higher tiers (detected default + config
    // policy) are enforced upstream in `is_protected_branch`; this guarantees
    // main/master survive even a future caller bug.
    if matches!(branch, "main" | "master") {
        return Err(format!("refusing to delete protected branch '{branch}'"));
    }
    let root = repo_root.to_string_lossy().to_string();
    run_command_capture("git", &["-C", &root, "branch", "-D", branch], None).map(|_| ())
}

pub(crate) fn parse_worktree_list_porcelain(output: &str) -> Vec<ExistingWorktree> {
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch = None;
    let mut is_bare = false;
    let mut is_detached = false;
    let mut is_prunable = false;

    let finish = |entries: &mut Vec<ExistingWorktree>,
                  path: &mut Option<PathBuf>,
                  branch: &mut Option<String>,
                  is_bare: &mut bool,
                  is_detached: &mut bool,
                  is_prunable: &mut bool| {
        if let Some(path) = path.take() {
            entries.push(ExistingWorktree {
                path,
                branch: branch.take(),
                is_bare: *is_bare,
                is_detached: *is_detached,
                is_prunable: *is_prunable,
            });
        }
        *is_bare = false;
        *is_detached = false;
        *is_prunable = false;
    };

    for line in output.lines() {
        if line.trim().is_empty() {
            finish(
                &mut entries,
                &mut path,
                &mut branch,
                &mut is_bare,
                &mut is_detached,
                &mut is_prunable,
            );
            continue;
        }
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("branch ") {
            branch = Some(
                value
                    .strip_prefix("refs/heads/")
                    .unwrap_or(value)
                    .to_string(),
            );
        } else if line == "detached" {
            is_detached = true;
        } else if line == "bare" {
            is_bare = true;
        } else if line.starts_with("prunable") {
            is_prunable = true;
        }
    }

    finish(
        &mut entries,
        &mut path,
        &mut branch,
        &mut is_bare,
        &mut is_detached,
        &mut is_prunable,
    );
    entries
}

pub(crate) fn list_existing_worktrees(repo_root: &Path) -> Result<Vec<ExistingWorktree>, String> {
    let output = crate::process::TracedCommand::new("git", "worktree")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output_traced()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(parse_worktree_list_porcelain(&stdout));
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("git worktree list failed with status {}", output.status)
    } else {
        stderr
    })
}

/// #175 S2 — the atomic move a scheduled reap performs when the classified
/// tier is [`KillAction::Quarantine`]. `git worktree move` is atomic on
/// same filesystem (a plain `renameat`) and refuses cross-fs with a clear
/// error; we surface that error to the caller so the reap emits a `Skip`
/// with a diagnostic instead of a half-moved worktree.
///
/// Writes a `QUARANTINE.md` file in the moved checkout describing why the
/// worktree was quarantined and how to recover it (`git worktree move`
/// back). Returns the destination path on success.
///
/// The branch is NEVER deleted. That is the reap-scheduled invariant this
/// function encodes at the ONLY place scheduled reap touches the disk.
pub(crate) fn quarantine_worktree(
    repo_root: &Path,
    checkout: &Path,
    branch: Option<&str>,
    reason: &str,
) -> Result<PathBuf, String> {
    let quarantine_root = quarantine_root_dir();
    std::fs::create_dir_all(&quarantine_root).map_err(|err| {
        format!(
            "failed to create quarantine dir {}: {err}",
            quarantine_root.display()
        )
    })?;
    let dst = quarantine_destination(&quarantine_root, repo_root, branch);

    let checkout_str = checkout.to_string_lossy().to_string();
    let repo_str = repo_root.to_string_lossy().to_string();
    let dst_str = dst.to_string_lossy().to_string();
    // `git worktree move` is what we want: atomic (same-fs rename), refuses
    // cross-fs with `invalid argument: cross-device link`, and updates git's
    // internal admin dir so the worktree stays a first-class worktree at
    // its new location. Never `mv`/`rename` by hand — that would corrupt
    // .git/worktrees admin state.
    run_command_capture(
        "git",
        &["-C", &repo_str, "worktree", "move", &checkout_str, &dst_str],
        None,
    )
    .map_err(|err| {
        // Same-fs vs cross-fs is the classic failure mode. Callers can
        // key off the substring the same way the sweep keys off
        // `is_dirty_worktree_remove_error`.
        format!(
            "git worktree move failed (checkout={}, dst={}): {err}",
            checkout.display(),
            dst.display()
        )
    })?;
    // The checkout just moved on disk; memoized canonicalizations of its old
    // path are now wrong (#262).
    crate::workspace::git::invalidate_path_canonicalization();

    // Recovery breadcrumb. Deliberately Markdown so `less` renders cleanly.
    // A write failure here must NOT fail the quarantine: the worktree is
    // already safely moved, and returning Err would report a failure for an
    // operation that succeeded (P4 — the preserved worktree is the point,
    // the note is the convenience). `quarantine-list` still finds it.
    let note = quarantine_note(repo_root, &dst, branch, reason, checkout);
    let note_path = dst.join("QUARANTINE.md");
    if let Err(err) = std::fs::write(&note_path, note) {
        crate::logging::quarantine_note_write_failed(
            &note_path.display().to_string(),
            &err.to_string(),
        );
    }
    Ok(dst)
}

/// Move a quarantined worktree back into an operator-chosen path via
/// `git worktree move` (atomic same-fs). NEVER deletes the source; the
/// quarantine dir is the single record.
pub(crate) fn unquarantine_worktree(quarantined: &Path, dst: &Path) -> Result<(), String> {
    // Repo root is the quarantined worktree itself; `git -C` there
    // resolves to the shared common dir automatically.
    let src_str = quarantined.to_string_lossy().to_string();
    let dst_str = dst.to_string_lossy().to_string();
    run_command_capture(
        "git",
        &["-C", &src_str, "worktree", "move", &src_str, &dst_str],
        None,
    )
    .map(|_| crate::workspace::git::invalidate_path_canonicalization())
    .map_err(|err| format!("git worktree move (unquarantine) failed: {err}"))
}

fn quarantine_root_dir() -> PathBuf {
    crate::session::data_dir().join("quarantine")
}

fn quarantine_destination(root: &Path, repo_root: &Path, branch: Option<&str>) -> PathBuf {
    let repo = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let branch_slug = branch_to_path_slug(branch.unwrap_or("detached"));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Second-resolution timestamps collide when two worktrees of the same
    // repo+branch quarantine within one second; the move would fail on an
    // existing destination and the operator would get an error instead of a
    // preserved worktree. A per-process counter disambiguates.
    static NONCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut candidate = root.join(format!("{repo}-{branch_slug}-{ts}"));
    if candidate.exists() {
        candidate = root.join(format!("{repo}-{branch_slug}-{ts}-{nonce}"));
    }
    candidate
}

fn quarantine_note(
    repo_root: &Path,
    dst: &Path,
    branch: Option<&str>,
    reason: &str,
    original_checkout: &Path,
) -> String {
    // Kept plain-text, easy to eyeball. Recovery command uses the DST as
    // the source because that's where the worktree now lives.
    let branch_desc = branch.unwrap_or("(detached)");
    format!(
        "# Quarantined worktree\n\n\
         This worktree was moved by the flock scheduled reap.\n\
         The branch was preserved; no git ref was deleted.\n\n\
         - repo:            {}\n\
         - branch:          {}\n\
         - reason:          {}\n\
         - original path:   {}\n\
         - quarantined at:  {}\n\n\
         ## Recover\n\n\
         Move the worktree back to any location on the same filesystem:\n\n\
         ```\n\
         git -C {} worktree move {} <your-target-path>\n\
         ```\n\n\
         Or run:\n\n\
         ```\n\
         flk worktree unquarantine {}\n\
         ```\n",
        repo_root.display(),
        branch_desc,
        reason,
        original_checkout.display(),
        dst.display(),
        dst.display(),
        dst.display(),
        dst.display(),
    )
}

/// Enumerate every quarantined worktree under the session's data dir. Each
/// entry surfaces the raw quarantine path; the recovery note inside carries
/// the repo/branch context. `Ok(vec![])` when no quarantine exists.
pub(crate) fn list_quarantined_worktrees() -> Result<Vec<PathBuf>, String> {
    let root = quarantine_root_dir();
    let read = match std::fs::read_dir(&root) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", root.display())),
    };
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|err| format!("{err}"))?;
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures — TracedCommand polices product code (logging redesign PR-3).
mod tests {
    use super::*;

    #[test]
    fn classify_kill_tier_covers_every_state() {
        let facts = |is_main, working_agent, dirty, merged| KillFacts {
            is_main,
            working_agent,
            dirty,
            merged,
        };

        // A running agent is protected first, whatever else is true.
        assert_eq!(
            classify_kill_tier(facts(false, true, false, true)),
            KillTier::SkipAgent
        );
        assert_eq!(
            classify_kill_tier(facts(true, true, false, false)),
            KillTier::SkipAgent
        );

        // Main checkout: close-pane when clean, skip when dirty.
        assert_eq!(
            classify_kill_tier(facts(true, false, false, false)),
            KillTier::ClosePane
        );
        assert_eq!(
            classify_kill_tier(facts(true, false, true, false)),
            KillTier::SkipMainDirty
        );

        // Linked + merged: kill branch either way; dirty is just flagged.
        assert_eq!(
            classify_kill_tier(facts(false, false, false, true)),
            KillTier::KillBranch { dirty: false }
        );
        assert_eq!(
            classify_kill_tier(facts(false, false, true, true)),
            KillTier::KillBranch { dirty: true }
        );

        // Linked + not merged: checkout-only when clean, force-only when dirty.
        assert_eq!(
            classify_kill_tier(facts(false, false, false, false)),
            KillTier::CheckoutOnly
        );
        assert_eq!(
            classify_kill_tier(facts(false, false, true, false)),
            KillTier::SkipUnmergedDirty
        );
    }

    #[test]
    fn planned_action_force_only_escalates_unmerged_dirty() {
        // Force never changes the safe tiers.
        for force in [false, true] {
            assert_eq!(
                planned_action(KillTier::ClosePane, force),
                KillAction::ClosePane
            );
            assert_eq!(
                planned_action(KillTier::KillBranch { dirty: true }, force),
                KillAction::KillBranch { dirty: true }
            );
            assert_eq!(
                planned_action(KillTier::CheckoutOnly, force),
                KillAction::CheckoutOnly
            );
            // Protected skips stay skipped even under force.
            assert_eq!(
                planned_action(KillTier::SkipMainDirty, force),
                KillAction::Skip
            );
            assert_eq!(planned_action(KillTier::SkipAgent, force), KillAction::Skip);
        }
        // Only the unmerged-dirty tier moves, and only to checkout-only.
        assert_eq!(
            planned_action(KillTier::SkipUnmergedDirty, false),
            KillAction::Skip
        );
        assert_eq!(
            planned_action(KillTier::SkipUnmergedDirty, true),
            KillAction::CheckoutOnly
        );
    }

    // ------- #175 S2: scheduled reap classification + quarantine -------

    /// Scheduled paths preserve every non-skip tier's action byte-for-byte.
    /// Human `planned_action` is UNCHANGED — no scheduled variant leaks.
    #[test]
    fn scheduled_action_never_emits_quarantine_from_safe_tiers() {
        assert_eq!(scheduled_action(KillTier::ClosePane), KillAction::ClosePane);
        assert_eq!(
            scheduled_action(KillTier::KillBranch { dirty: false }),
            KillAction::KillBranch { dirty: false }
        );
        assert_eq!(
            scheduled_action(KillTier::KillBranch { dirty: true }),
            KillAction::KillBranch { dirty: true }
        );
        assert_eq!(
            scheduled_action(KillTier::CheckoutOnly),
            KillAction::CheckoutOnly
        );
        // Protected tiers keep their protection under scheduled reap too.
        assert_eq!(scheduled_action(KillTier::SkipMainDirty), KillAction::Skip);
        assert_eq!(scheduled_action(KillTier::SkipAgent), KillAction::Skip);
    }

    /// §8.5 adversarial rows: every state that would normally trip the
    /// human `Skip-because-unmerged/dirty` path becomes `Quarantine` under
    /// scheduled reap. Human `planned_action(...)` still yields `Skip` (or
    /// force-escalated `CheckoutOnly`) for the same tier.
    #[test]
    fn scheduled_action_quarantines_skip_unmerged_dirty_tier() {
        assert_eq!(
            scheduled_action(KillTier::SkipUnmergedDirty),
            KillAction::Quarantine,
            "scheduled reap must never delete an unmerged-dirty worktree; the tier converts to Quarantine"
        );
        // The human path is undisturbed — regressing that is the invariant
        // this assertion locks down.
        assert_eq!(
            planned_action(KillTier::SkipUnmergedDirty, false),
            KillAction::Skip,
            "human planned_action for this tier must still Skip"
        );
    }

    /// §8.5 adversarial matrix: classify_kill_tier on each of the shapes
    /// the design calls out (dirty tree, unpushed commits, detached HEAD,
    /// stash) — and scheduled_action must yield Quarantine (never a delete)
    /// for the ones that are unmerged/dirty.
    #[test]
    fn scheduled_reap_covers_adversarial_rows() {
        let mut facts = KillFacts {
            is_main: false,
            working_agent: false,
            dirty: false,
            merged: false,
        };

        // 1. Dirty tree (unmerged + dirty) → SkipUnmergedDirty → Quarantine.
        facts.dirty = true;
        let tier = classify_kill_tier(facts);
        assert_eq!(tier, KillTier::SkipUnmergedDirty);
        assert_eq!(scheduled_action(tier), KillAction::Quarantine);

        // 2. Unpushed commits (unmerged, clean) → CheckoutOnly. The
        // scheduled reap still removes the checkout, but the BRANCH stays
        // — no delete without merge evidence.
        facts.dirty = false;
        let tier = classify_kill_tier(facts);
        assert_eq!(tier, KillTier::CheckoutOnly);
        assert_eq!(scheduled_action(tier), KillAction::CheckoutOnly);

        // 3. Detached HEAD: the sweep asks classify_kill_tier via
        //    is_main=false, merged=false, dirty=? The tier is
        //    SkipUnmergedDirty when dirty, CheckoutOnly when clean. Both
        //    map to non-delete actions.
        facts.dirty = true;
        assert!(!matches!(
            scheduled_action(classify_kill_tier(facts)),
            KillAction::KillBranch { .. }
        ));

        // 4. Active agent wins over everything — even scheduled.
        facts.working_agent = true;
        assert_eq!(
            scheduled_action(classify_kill_tier(facts)),
            KillAction::Skip
        );
    }

    #[test]
    fn quarantine_worktree_moves_atomically_and_writes_note() {
        // Fixture: main repo with a real worktree, dirty tree so the
        // scheduled reap would trip §8.5. We invoke `quarantine_worktree`
        // directly (no runner needed) to prove the atomic-move + note
        // contract.
        let repo = create_committed_repo("quarantine-move");
        // Create a linked worktree on a feature branch.
        let wt = unique_temp_path("quarantine-wt");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature-y",
                &wt.display().to_string(),
            ],
        );
        // Dirty the tree.
        std::fs::write(wt.join("scratch.txt"), "wip\n").unwrap();

        // Redirect XDG so quarantine goes into a test-owned dir.
        let sandbox = unique_temp_path("quarantine-sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &sandbox);

        let dst = quarantine_worktree(&repo, &wt, Some("feature-y"), "dirty").unwrap();
        assert!(
            dst.is_dir(),
            "moved worktree dir must exist: {}",
            dst.display()
        );
        assert!(!wt.exists(), "original checkout must have moved");
        // The quarantined checkout is still a working worktree (git knows
        // about it) — its README from the initial commit is present.
        assert!(dst.join("README.md").is_file());
        // Recovery note is dropped alongside.
        let note = std::fs::read_to_string(dst.join("QUARANTINE.md")).unwrap();
        assert!(
            note.contains("feature-y"),
            "note must mention branch: {note}"
        );
        assert!(note.contains("dirty"), "note must mention reason: {note}");
        assert!(
            note.contains("unquarantine"),
            "recovery instructions: {note}"
        );
        // Branch was NOT deleted (never delete without merge evidence).
        let branches = run_command_capture(
            "git",
            &["-C", &repo.display().to_string(), "branch", "--list"],
            None,
        )
        .unwrap();
        assert!(
            branches.contains("feature-y"),
            "branch preserved: {branches}"
        );

        // list_quarantined_worktrees sees it.
        let list = list_quarantined_worktrees().unwrap();
        assert!(list.iter().any(|p| p == &dst));

        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn unquarantine_moves_back_and_preserves_branch() {
        let repo = create_committed_repo("unquarantine");
        let wt = unique_temp_path("unquarantine-wt");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature-z",
                &wt.display().to_string(),
            ],
        );
        std::fs::write(wt.join("scratch.txt"), "wip\n").unwrap();

        let sandbox = unique_temp_path("unquarantine-sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &sandbox);
        let quarantined = quarantine_worktree(&repo, &wt, Some("feature-z"), "dirty").unwrap();

        let restored = unique_temp_path("unquarantine-restored");
        unquarantine_worktree(&quarantined, &restored).unwrap();
        assert!(
            restored.join("scratch.txt").is_file(),
            "scratch content restored"
        );
        // The quarantined path is now empty (git worktree move renamed it).
        assert!(
            !quarantined.exists()
                || std::fs::read_dir(&quarantined)
                    .ok()
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true)
        );

        std::env::remove_var("XDG_CONFIG_HOME");
    }

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("flock-{name}-{}-{nanos}", std::process::id()))
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git command failed: git -C {} {}",
            repo.display(),
            args.join(" ")
        );
    }

    fn create_committed_repo(name: &str) -> PathBuf {
        let repo = unique_temp_path(name);
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet"]);
        run_git(&repo, &["config", "user.email", "flock@example.invalid"]);
        run_git(&repo, &["config", "user.name", "Flock Test"]);
        std::fs::write(repo.join("README.md"), "test\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(&repo, &["commit", "--quiet", "-m", "initial"]);
        repo
    }

    /// A committed repo wired to a fresh bare `origin` remote (no upstream
    /// tracking set yet) — the shape `prepare_peer_checkout` operates on.
    fn create_repo_with_bare_origin(name: &str) -> (PathBuf, PathBuf) {
        let repo = create_committed_repo(name);
        let origin = unique_temp_path(&format!("{name}-origin"));
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "--quiet", "--bare"]);
        run_git(
            &repo,
            &["remote", "add", "origin", &origin.display().to_string()],
        );
        (repo, origin)
    }

    fn origin_has_branch(origin: &Path, branch: &str) -> bool {
        run_command_capture(
            "git",
            &[
                "-C",
                &origin.display().to_string(),
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            None,
        )
        .is_ok()
    }

    #[test]
    fn prepare_peer_checkout_probe_reports_unpushed_without_mutating() {
        let (repo, origin) = create_repo_with_bare_origin("peer-probe");
        run_git(&repo, &["checkout", "--quiet", "-b", "feature-x"]);

        let report = prepare_peer_checkout(&repo, "feature-x", false).unwrap();
        assert_eq!(
            report,
            PeerCheckoutReport {
                was_dirty: false,
                was_unpushed: true,
                pushed: false,
            }
        );
        // A read-only probe must not push.
        assert!(!origin_has_branch(&origin, "feature-x"));
    }

    #[test]
    fn prepare_peer_checkout_pushes_branch_to_origin() {
        let (repo, origin) = create_repo_with_bare_origin("peer-push");
        run_git(&repo, &["checkout", "--quiet", "-b", "feature-x"]);

        let report = prepare_peer_checkout(&repo, "feature-x", true).unwrap();
        assert!(report.pushed);
        assert!(report.was_unpushed, "no upstream before the push");
        assert!(origin_has_branch(&origin, "feature-x"));

        // A second prepare now sees it as already pushed (in sync).
        let again = prepare_peer_checkout(&repo, "feature-x", false).unwrap();
        assert!(!again.was_unpushed);
    }

    #[test]
    fn prepare_peer_checkout_ignores_a_tag_sharing_the_branch_name() {
        // The ahead-count's range endpoint is a commit-ish, so a same-named tag
        // captures it and the branch is reported unpushed while its upstream is
        // in sync (#243). `@{upstream}` above is immune — it resolves a branch
        // name, never a tag — which is why only one of the two is qualified.
        let (repo, _origin) = create_repo_with_bare_origin("peer-tag-collision");
        run_git(&repo, &["checkout", "--quiet", "-b", "v1.0"]);
        let pushed = prepare_peer_checkout(&repo, "v1.0", true).unwrap();
        assert!(pushed.pushed);

        // A tag named after the branch, pointing at an unrelated commit.
        run_git(&repo, &["checkout", "--quiet", "-b", "elsewhere"]);
        std::fs::write(repo.join("other.txt"), "other\n").unwrap();
        run_git(&repo, &["add", "other.txt"]);
        run_git(&repo, &["commit", "--quiet", "-m", "unrelated"]);
        let elsewhere = run_command_capture(
            "git",
            &["-C", &repo.display().to_string(), "rev-parse", "HEAD"],
            None,
        )
        .expect("tip");
        run_git(&repo, &["update-ref", "refs/tags/v1.0", &elsewhere]);

        let again = prepare_peer_checkout(&repo, "v1.0", false).unwrap();
        assert!(
            !again.was_unpushed,
            "the branch is in sync with its upstream; the tag is not the branch"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn prepare_peer_checkout_flags_a_dirty_working_tree() {
        let (repo, _origin) = create_repo_with_bare_origin("peer-dirty");
        run_git(&repo, &["checkout", "--quiet", "-b", "feature-x"]);
        std::fs::write(repo.join("README.md"), "uncommitted\n").unwrap();

        let report = prepare_peer_checkout(&repo, "feature-x", false).unwrap();
        assert!(report.was_dirty);
    }

    #[test]
    fn prepare_peer_checkout_errors_on_missing_branch() {
        let (repo, _origin) = create_repo_with_bare_origin("peer-missing");
        let err = prepare_peer_checkout(&repo, "nope", false).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn fetch_and_add_peer_worktree_brings_branch_across_from_origin() {
        // A peer pushes feature-x to origin (the spoke's checkout-prepare leg).
        let (peer, origin) = create_repo_with_bare_origin("xchk-peer");
        run_git(&peer, &["checkout", "--quiet", "-b", "feature-x"]);
        std::fs::write(peer.join("feature.txt"), "from peer\n").unwrap();
        run_git(&peer, &["add", "feature.txt"]);
        run_git(&peer, &["commit", "--quiet", "-m", "feature work"]);
        run_git(&peer, &["push", "--quiet", "-u", "origin", "feature-x"]);

        // The hub has its OWN clone of the same origin, with no feature-x yet.
        let hub = unique_temp_path("xchk-hub");
        let clone_ok = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args([
                "clone",
                "--quiet",
                &origin.display().to_string(),
                &hub.display().to_string(),
            ])
            .status()
            .unwrap()
            .success();
        assert!(clone_ok, "git clone of origin failed");
        run_git(&hub, &["config", "user.email", "flock@example.invalid"]);
        run_git(&hub, &["config", "user.name", "Flock Test"]);

        let worktree_dir = unique_temp_path("xchk-worktrees");
        let path = fetch_and_add_peer_worktree(&hub, &worktree_dir, "hub", "feature-x").unwrap();

        // The hub now has a worktree on the peer's branch, with its commit.
        assert!(path.is_dir(), "worktree checkout exists");
        assert_eq!(checkout_branch_name(&path).as_deref(), Some("feature-x"));
        assert!(
            path.join("feature.txt").is_file(),
            "peer's commit is present"
        );
    }

    #[test]
    fn generated_branch_slug_is_worktree_namespaced_and_stable() {
        assert_eq!(generated_branch_slug(0), "worktree/brave-river-0000");
        assert_eq!(generated_branch_slug(9), "worktree/calm-cloud-0009");
    }

    #[test]
    fn parses_git_worktree_list_porcelain() {
        let output = "\
worktree /repo/main
HEAD abc
branch refs/heads/main

worktree /repo/issue
HEAD def
branch refs/heads/worktree/issue

worktree /repo/detached
HEAD fed
detached
prunable stale

";

        assert_eq!(
            parse_worktree_list_porcelain(output),
            vec![
                ExistingWorktree {
                    path: PathBuf::from("/repo/main"),
                    branch: Some("main".into()),
                    is_bare: false,
                    is_detached: false,
                    is_prunable: false,
                },
                ExistingWorktree {
                    path: PathBuf::from("/repo/issue"),
                    branch: Some("worktree/issue".into()),
                    is_bare: false,
                    is_detached: false,
                    is_prunable: false,
                },
                ExistingWorktree {
                    path: PathBuf::from("/repo/detached"),
                    branch: None,
                    is_bare: false,
                    is_detached: true,
                    is_prunable: true,
                },
            ]
        );
    }

    #[test]
    fn branch_to_path_slug_makes_branch_safe_folder_name() {
        assert_eq!(
            branch_to_path_slug("worktree/brave-river"),
            "worktree-brave-river"
        );
        assert_eq!(
            branch_to_path_slug("issue/137 Worktree Spaces"),
            "issue-137-worktree-spaces"
        );
        assert_eq!(branch_to_path_slug("///"), "worktree");
    }

    #[test]
    fn expand_tilde_path_uses_home_when_available() {
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", "/home/me");
        assert_eq!(
            expand_tilde_path("~/.flock/worktrees"),
            PathBuf::from("/home/me/.flock/worktrees")
        );
        assert_eq!(
            expand_tilde_path("/tmp/worktrees"),
            PathBuf::from("/tmp/worktrees")
        );
        if let Some(home) = old_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
    }

    /// #212: the repro from the issue — a standalone repo and a same-named
    /// submodule of another repo. Both answer "nucl-parquet" for their
    /// basename, so under the old scheme their worktrees shared one base
    /// directory with only the branch slug telling them apart.
    #[test]
    fn worktree_base_is_keyed_on_repo_identity_not_basename() {
        let standalone =
            worktree_base_dir_name("nucl-parquet", "/home/me/Projects/nucl-parquet/.git");
        let submodule = worktree_base_dir_name(
            "nucl-parquet",
            "/home/me/Projects/hyrr/.git/modules/nucl-parquet",
        );
        assert_ne!(
            standalone, submodule,
            "same-named repos must not share a worktree base"
        );

        // Parent-qualifying would not have been enough either: two repos can
        // share a parent directory name at different absolute paths.
        let a = worktree_base_dir_name("repo", "/a/x/repo/.git");
        let b = worktree_base_dir_name("repo", "/b/x/repo/.git");
        assert_ne!(a, b, "identity, not the parent name, has to discriminate");

        // Readable prefix retained, and the name is a pure function of the
        // repo — same inputs, same directory, every time.
        assert!(standalone.starts_with("nucl-parquet-"), "{standalone}");
        assert_eq!(
            standalone,
            worktree_base_dir_name("nucl-parquet", "/home/me/Projects/nucl-parquet/.git")
        );
    }

    /// A worktree created from a linked worktree of a repo still belongs to
    /// that repo's namespace — branch-from-here must not scatter worktrees.
    #[test]
    fn worktree_base_is_shared_across_a_repos_checkouts() {
        // Both the main checkout and its linked worktree carry the same
        // `key` (the git common dir), which is what the base is keyed on.
        let from_main = worktree_base_dir_name("flock", "/repo/flock/.git");
        let from_linked = worktree_base_dir_name("flock", "/repo/flock/.git");
        assert_eq!(from_main, from_linked);
    }

    #[test]
    fn default_checkout_path_appends_repo_and_branch_slug() {
        assert_eq!(
            default_checkout_path(
                Path::new("/home/me/.flock/worktrees"),
                "flock",
                "/repo/flock/.git",
                "worktree/brave-river",
            ),
            PathBuf::from("/home/me/.flock/worktrees")
                .join(worktree_base_dir_name("flock", "/repo/flock/.git"))
                .join("worktree-brave-river")
        );
    }

    #[test]
    fn worktree_remove_command_preserves_branch_by_not_deleting_it() {
        let command = build_worktree_remove_command(
            Path::new("/repo/flock"),
            Path::new("/w/flock/issue-137"),
            false,
        );
        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/flock",
                "worktree",
                "remove",
                "/w/flock/issue-137"
            ]
        );
    }

    #[test]
    fn forced_worktree_remove_command_uses_git_force_flag() {
        let command = build_worktree_remove_command(
            Path::new("/repo/flock"),
            Path::new("/w/flock/issue-137"),
            true,
        );
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/flock",
                "worktree",
                "remove",
                "--force",
                "/w/flock/issue-137"
            ]
        );
    }

    #[test]
    fn dirty_remove_error_detection_matches_git_force_hint() {
        assert!(is_dirty_worktree_remove_error(
            "fatal: '/w/flock' contains modified or untracked files, use --force to delete it"
        ));
        assert!(!is_dirty_worktree_remove_error(
            "fatal: '/w/flock' is a missing but already registered worktree"
        ));
        assert!(!is_dirty_worktree_remove_error(
            "fatal: '/w/flock' contains a locked worktree, use --force only if you know why"
        ));
    }

    #[test]
    fn worktree_add_command_creates_new_branch_from_base() {
        let command = build_worktree_add_new_branch_command(
            Path::new("/repo/flock"),
            Path::new("/w/flock/worktree-brave-river"),
            "worktree/brave-river",
            "HEAD",
        );
        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "-C",
                "/repo/flock",
                "worktree",
                "add",
                "-b",
                "worktree/brave-river",
                "/w/flock/worktree-brave-river",
                "HEAD"
            ]
        );
    }

    #[test]
    fn worktree_add_in_a_repo_without_commits_names_the_unborn_head() {
        // #198: `git init` with nothing committed leaves HEAD unborn, so the
        // default base can't resolve. git's own words are "fatal: invalid
        // reference: HEAD", which reads as a flock bug rather than a missing
        // initial commit — four such failures sat in a live server log.
        let repo = unique_temp_path("worktree-unborn-head-repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet"]);
        let checkout = unique_temp_path("worktree-unborn-head-checkout");

        let add = build_worktree_add_new_branch_command(&repo, &checkout, "worktree/x", "HEAD");
        let err = run_worktree_command(&add).expect_err("unborn HEAD cannot be branched");
        assert!(err.contains("invalid reference: HEAD"), "{err}");

        let explained = explain_worktree_add_failure("HEAD", &err);
        assert!(explained.contains("no commits yet"), "{explained}");
        assert!(explained.contains("make one"), "{explained}");
        // git's own text is kept: it is what a user would search for.
        assert!(explained.contains("invalid reference: HEAD"), "{explained}");
        // Every line has to fit the create dialog's error area or the remedy
        // is the half that gets wrapped out of sight (#243).
        for line in explained.lines() {
            assert!(
                line.chars().count() <= 64,
                "line outruns the dialog width: {line}"
            );
        }

        assert!(!checkout.exists());
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn worktree_add_in_a_non_repo_names_the_remedy() {
        // A project directory that was never `git init`-ed (#243). git's own
        // message is accurate but the parenthetical doubles its length for no
        // extra help, and it never says what to do.
        let plain = unique_temp_path("worktree-non-repo");
        std::fs::create_dir_all(&plain).unwrap();
        let add =
            build_worktree_add_new_branch_command(&plain, &plain.join("wt"), "worktree/x", "HEAD");
        let err = run_worktree_command(&add).expect_err("a non-repo cannot be branched");
        assert!(err.contains("not a git repository"), "{err}");

        let explained = explain_worktree_add_failure("HEAD", &err);
        assert!(explained.contains("not a git repository"), "{explained}");
        assert!(explained.contains("git init"), "{explained}");
        assert!(
            !explained.contains("parent directories"),
            "the parenthetical is noise: {explained}"
        );
        for line in explained.lines() {
            assert!(
                line.chars().count() <= 64,
                "line outruns the dialog width: {line}"
            );
        }

        std::fs::remove_dir_all(&plain).ok();
    }

    #[test]
    fn unrelated_worktree_add_failures_pass_through_untouched() {
        // An explicit base that isn't there is a different problem, and git
        // already names it — don't blame a missing initial commit for it.
        let missing_base = "fatal: invalid reference: no-such-branch";
        assert_eq!(
            explain_worktree_add_failure("no-such-branch", missing_base),
            missing_base
        );

        let occupied = "fatal: '/w/x' already exists";
        assert_eq!(explain_worktree_add_failure("HEAD", occupied), occupied);
    }

    #[test]
    fn run_worktree_add_and_remove_create_and_delete_checkout() {
        let repo = create_committed_repo("worktree-run-repo");
        let checkout = unique_temp_path("worktree-run-checkout");
        let branch = "worktree/test-create-remove";

        let add = build_worktree_add_new_branch_command(&repo, &checkout, branch, "HEAD");
        run_worktree_command(&add).unwrap();

        assert!(checkout.join("README.md").exists());
        let branch_name = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .arg("-C")
            .arg(&checkout)
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert!(branch_name.status.success());
        assert_eq!(
            String::from_utf8(branch_name.stdout).unwrap().trim(),
            branch
        );

        let remove = build_worktree_remove_command(&repo, &checkout, false);
        run_worktree_command(&remove).unwrap();
        assert!(!checkout.exists());

        let _ = std::fs::remove_dir_all(repo);
    }
    #[test]
    fn checkout_branch_name_and_default_branch_detection() {
        let repo = create_committed_repo("merge-gate-names");
        let checkout = unique_temp_path("merge-gate-names-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/gate",
                checkout.to_str().unwrap(),
            ],
        );

        assert_eq!(
            checkout_branch_name(&checkout).as_deref(),
            Some("feature/gate")
        );
        // create_committed_repo commits on the default init branch; detection
        // falls back to main/master existence when origin/HEAD is unset.
        let default = detect_default_branch(&repo);
        assert!(
            default.as_deref() == Some("master") || default.as_deref() == Some("main"),
            "unexpected default branch: {default:?}"
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn resolve_gate_with_timeout_returns_fast_result_untimed() {
        let (gate, timed_out) = resolve_gate_with_timeout(
            || WorktreeMergeGate::Merged {
                evidence: "PR #1 merged".into(),
            },
            std::time::Duration::from_secs(5),
        );
        assert_eq!(
            gate,
            WorktreeMergeGate::Merged {
                evidence: "PR #1 merged".into()
            }
        );
        assert!(!timed_out, "a fast gate must not be marked timed out");
    }

    #[test]
    fn resolve_gate_with_timeout_degrades_to_not_merged_on_timeout() {
        // The work outlives the timeout (simulating a hung `gh pr view`): the
        // gate must degrade to the safe NotMerged and flag the timeout.
        let (gate, timed_out) = resolve_gate_with_timeout(
            || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                WorktreeMergeGate::Merged {
                    evidence: "too late".into(),
                }
            },
            std::time::Duration::from_millis(30),
        );
        assert_eq!(gate, WorktreeMergeGate::NotMerged);
        assert!(timed_out, "a gate slower than the bound must be timed out");
    }

    #[test]
    fn branch_merge_gate_deletes_a_branch_with_no_commits_of_its_own() {
        // The reported case (#243): branch a session off a local feature
        // branch that was never pushed, change your mind immediately, kill it.
        // The tip is not on the default branch and not on any remote, so all
        // three merge-evidence sources come up empty — but the branch holds no
        // commits of its own, so deleting it discards nothing.
        let repo = create_committed_repo("merge-gate-empty");
        run_git(&repo, &["checkout", "--quiet", "-b", "feature/base"]);
        std::fs::write(repo.join("wip.txt"), "wip\n").unwrap();
        run_git(&repo, &["add", "wip.txt"]);
        run_git(&repo, &["commit", "--quiet", "-m", "unpushed base work"]);

        let checkout = unique_temp_path("merge-gate-empty-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree/fresh",
                checkout.to_str().unwrap(),
                "feature/base",
            ],
        );

        // The evidence names the base the session was cut from, rather than
        // the bare "no commits of its own" — the user learns where the work is.
        assert_eq!(
            branch_merge_gate(&repo, &checkout, "worktree/fresh"),
            WorktreeMergeGate::Merged {
                evidence: "contained in feature/base".to_string()
            }
        );

        // One real commit and the branch is no longer disposable: its work
        // exists nowhere else, so the gate must fall back to keeping it.
        std::fs::write(checkout.join("new.txt"), "x\n").unwrap();
        run_git(&checkout, &["add", "new.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "real work"]);
        assert_eq!(
            branch_merge_gate(&repo, &checkout, "worktree/fresh"),
            WorktreeMergeGate::NotMerged
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_has_no_unique_commits_ignores_the_branchs_own_refs() {
        // Both exclusions matter: without --exclude <branch> --branches the
        // count is trivially zero for every branch, and a branch's own remote
        // tracking ref is not independent evidence that the work is safe.
        let (repo, _origin) = create_repo_with_bare_origin("merge-gate-own-refs");
        let root = repo.display().to_string();
        run_git(&repo, &["checkout", "--quiet", "-b", "solo"]);
        std::fs::write(repo.join("solo.txt"), "solo\n").unwrap();
        run_git(&repo, &["add", "solo.txt"]);
        run_git(&repo, &["commit", "--quiet", "-m", "solo work"]);

        assert!(
            !branch_has_no_unique_commits(&root, "solo"),
            "a branch whose commit exists nowhere else has unique work"
        );

        // Pushing it creates refs/remotes/origin/solo — still its own ref, so
        // the answer must not flip.
        run_git(&repo, &["push", "--quiet", "origin", "solo"]);
        assert!(
            !branch_has_no_unique_commits(&root, "solo"),
            "a branch's own tracking ref is not independent evidence"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_merge_gate_is_not_fooled_by_a_tag_sharing_the_branch_name() {
        // git resolves a bare name against refs/tags BEFORE refs/heads, so a
        // release tag and a hotfix branch sharing a name (`v1.0` here) made
        // every commit-ish argument in the gate read the tag's history. The
        // tag sits on the default branch, so the gate found *positive*
        // deletion evidence for a branch whose commit exists nowhere else —
        // both via remote containment and via the unique-commit count (#243).
        let (repo, _origin) = create_repo_with_bare_origin("merge-gate-tag-collision");
        let root = repo.display().to_string();
        let default = detect_default_branch(&repo).expect("default branch");
        run_git(&repo, &["push", "--quiet", "origin", &default]);
        run_git(&repo, &["tag", "-a", "v1.0", "-m", "release"]);
        run_git(&repo, &["push", "--quiet", "origin", "v1.0"]);

        let checkout = unique_temp_path("merge-gate-tag-collision-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "v1.0",
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::write(checkout.join("hotfix.txt"), "hotfix\n").unwrap();
        run_git(&checkout, &["add", "hotfix.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "real hotfix work"]);

        assert!(
            !branch_has_no_unique_commits(&root, "v1.0"),
            "the branch's commit lives nowhere else — the tag is not it"
        );
        assert_eq!(
            branch_merge_gate(&repo, &checkout, "v1.0"),
            WorktreeMergeGate::NotMerged,
            "a same-named tag must not become evidence for deleting the branch"
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_merge_gate_is_not_fooled_by_a_tag_shadowing_the_default_branch() {
        // The *base* of `git branch --merged <base>` is a commit-ish too, and
        // shadowable the same way: tag the default branch's name onto some
        // other commit and the merged set is computed about the wrong history.
        // A tag pointing forward inflates that set, which is the dangerous
        // direction — it authorises deleting a branch that is not merged.
        // Default branch names like `release`, `stable` or `v1` make a
        // same-named tag entirely plausible.
        let repo = create_committed_repo("merge-gate-default-shadow");
        let default = detect_default_branch(&repo).expect("default branch");

        let checkout = unique_temp_path("merge-gate-default-shadow-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "sidework",
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::write(checkout.join("unique.txt"), "unique\n").unwrap();
        run_git(&checkout, &["add", "unique.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "work only here"]);
        let side_tip = run_command_capture(
            "git",
            &["-C", &checkout.display().to_string(), "rev-parse", "HEAD"],
            None,
        )
        .expect("side tip");

        // A tag named after the default branch, pointing past it.
        run_git(
            &repo,
            &["update-ref", &format!("refs/tags/{default}"), &side_tip],
        );

        assert_eq!(
            branch_merge_gate(&repo, &checkout, "sidework"),
            WorktreeMergeGate::NotMerged,
            "a tag shadowing the default branch must not authorise a delete"
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn containment_evidence_never_names_a_remotes_symbolic_head() {
        // `%(refname:short)` renders refs/remotes/origin/HEAD as bare `origin`,
        // so the old `contains("HEAD")` filter missed it and the first match
        // for an empty branch cut from the default was `contained in origin` —
        // which names no branch at all (#243).
        let (repo, origin) = create_repo_with_bare_origin("containment-symbolic-head");
        let default = detect_default_branch(&repo).expect("default branch");
        run_git(&repo, &["push", "--quiet", "-u", "origin", &default]);
        // A clone is what actually creates refs/remotes/origin/HEAD.
        let clone = unique_temp_path("containment-symbolic-head-clone");
        run_git(
            &repo,
            &[
                "clone",
                "--quiet",
                &origin.display().to_string(),
                clone.to_str().unwrap(),
            ],
        );
        let clone_root = clone.display().to_string();
        run_git(
            &clone,
            &["branch", "worktree/fresh", &format!("origin/{default}")],
        );

        let containing = refs_containing(&clone_root, "worktree/fresh", RefScope::Remote);
        assert!(
            !containing.iter().any(|r| r == "origin"),
            "a remote's symbolic HEAD is not a branch name: {containing:?}"
        );
        assert!(
            containing.iter().any(|r| r == &format!("origin/{default}")),
            "the real remote branch must still be found: {containing:?}"
        );

        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&origin);
    }

    #[test]
    fn refs_containing_excludes_the_branchs_own_refs() {
        // A ref cannot vouch for itself: neither refs/heads/<branch> nor its
        // tracking ref on any remote is evidence about <branch>.
        let (repo, _origin) = create_repo_with_bare_origin("refs-containing-own");
        let root = repo.display().to_string();
        run_git(&repo, &["checkout", "--quiet", "-b", "solo"]);
        std::fs::write(repo.join("solo.txt"), "solo\n").unwrap();
        run_git(&repo, &["add", "solo.txt"]);
        run_git(&repo, &["commit", "--quiet", "-m", "solo work"]);
        run_git(&repo, &["push", "--quiet", "-u", "origin", "solo"]);

        // Prove git really does list the refs being filtered, so the
        // assertions below cannot pass by the helper simply returning nothing.
        let raw = run_command_capture(
            "git",
            &[
                "-C",
                &root,
                "branch",
                "-a",
                "--contains",
                "refs/heads/solo",
                "--format",
                "%(refname)",
            ],
            None,
        )
        .expect("git lists the containing refs");
        for own in ["refs/heads/solo", "refs/remotes/origin/solo"] {
            assert!(
                raw.lines().any(|line| line.trim() == own),
                "fixture must contain {own} for the filter to be doing work: {raw:?}"
            );
        }

        let all = refs_containing(&root, "solo", RefScope::All);
        assert!(
            !all.iter().any(|r| r == "solo" || r == "origin/solo"),
            "own refs must not appear: {all:?}"
        );
        assert!(
            refs_containing(&root, "solo", RefScope::Remote).is_empty(),
            "only its own tracking ref contains it, so there is no evidence"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_has_no_unique_commits_keeps_the_branch_when_git_fails() {
        // The count is only evidence when it is readable. Every failure path
        // — nonzero exit, missing ref, unparsable stdout — has to answer
        // `false`, because `true` is the answer that deletes a branch.
        let repo = create_committed_repo("merge-gate-unreadable");
        let root = repo.display().to_string();

        assert!(
            !branch_has_no_unique_commits(&root, "no/such/branch"),
            "a ref that does not resolve is not evidence of anything"
        );
        assert!(
            !branch_has_no_unique_commits("/no/such/repo/path", "main"),
            "an unreadable repo is not evidence of anything"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn probe_kill_targets_names_the_dirty_files_and_counts_unpushed_commits() {
        // #325: the dialog authorises a destructive act, so the account has to
        // be a list of paths, not a boolean.
        let repo = create_committed_repo("kill-probe");
        let checkout = unique_temp_path("kill-probe-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/probe",
                checkout.to_str().unwrap(),
            ],
        );

        // Clean, and every commit is on the (remote-less) default branch's
        // history — but with no remotes at all, nothing is "pushed".
        let clean = probe_kill_targets(&checkout, Some("feature/probe"));
        assert_eq!(clean.dirty.as_deref(), Some(&[][..]));

        std::fs::write(checkout.join("tracked.txt"), "one\n").unwrap();
        run_git(&checkout, &["add", "tracked.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "a commit"]);
        std::fs::write(checkout.join("tracked.txt"), "changed\n").unwrap();
        std::fs::create_dir_all(checkout.join("scratch")).unwrap();
        std::fs::write(checkout.join("scratch/untracked.txt"), "x\n").unwrap();

        let probe = probe_kill_targets(&checkout, Some("feature/probe"));
        let dirty = probe
            .dirty
            .clone()
            .expect("a readable checkout reports its state");
        assert!(
            dirty.iter().any(|line| line.contains("tracked.txt")),
            "{dirty:?}"
        );
        // `-uall` lists the file, not the directory: "one untracked directory"
        // is exactly the summary that hides how much is about to go.
        assert!(
            dirty
                .iter()
                .any(|line| line.contains("scratch/untracked.txt")),
            "{dirty:?}"
        );
        assert_eq!(
            probe.unpushed,
            Some(2),
            "no remote holds either commit of this branch"
        );
        assert!(probe.has_stakes());

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn probe_kill_targets_reports_unknown_rather_than_clean() {
        // An unreadable checkout must not render as "nothing to lose" — the
        // caller distinguishes the two, and this is where that starts.
        let missing = unique_temp_path("kill-probe-missing");
        let probe = probe_kill_targets(&missing, Some("feature/gone"));
        assert_eq!(probe.dirty, None);
        assert_eq!(probe.unpushed, None);
        assert!(
            probe.has_stakes(),
            "unknown counts as stakes — it is the reading that makes the user check"
        );

        // No branch at all (a detached checkout) leaves the count unknown too,
        // rather than claiming zero.
        let repo = create_committed_repo("kill-probe-detached");
        assert_eq!(probe_kill_targets(&repo, None).unpushed, None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_merge_gate_sees_a_squash_merge() {
        // The live miss (#287): the PR squash-merged, so the branch's work is
        // in main while none of its commits is an ancestor of main. Every
        // ancestry-based source says "no evidence" and the merge-gated kill
        // refuses to clean up a worktree whose work has fully landed.
        let repo = create_committed_repo("merge-gate-squash");
        let checkout = unique_temp_path("merge-gate-squash-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/squashed",
                checkout.to_str().unwrap(),
            ],
        );
        // Two commits, so the squash is a genuine collapse rather than a
        // rename of a single one.
        std::fs::write(checkout.join("new.txt"), "one\n").unwrap();
        run_git(&checkout, &["add", "new.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "first"]);
        std::fs::write(checkout.join("new.txt"), "one\ntwo\n").unwrap();
        run_git(&checkout, &["add", "new.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "second"]);

        assert_eq!(
            branch_merge_gate(&repo, &checkout, "feature/squashed"),
            WorktreeMergeGate::NotMerged,
            "unmerged work must keep the branch"
        );

        // Squash it onto the default branch, exactly as GitHub's squash merge
        // does: same content, one new commit, no ancestry.
        let default = detect_default_branch(&repo).expect("default branch");
        run_git(&repo, &["merge", "--squash", "feature/squashed"]);
        run_git(&repo, &["commit", "--quiet", "-m", "feature work (#1)"]);
        assert!(
            run_command_capture(
                "git",
                &[
                    "-C",
                    repo.to_str().unwrap(),
                    "merge-base",
                    "--is-ancestor",
                    "refs/heads/feature/squashed",
                    &format!("refs/heads/{default}"),
                ],
                None,
            )
            .is_err(),
            "a squash must leave the branch tip unreachable from the default branch"
        );

        assert_eq!(
            branch_merge_gate(&repo, &checkout, "feature/squashed"),
            WorktreeMergeGate::Merged {
                evidence: format!("already in {default} (squashed or rebased)")
            }
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_merge_gate_keeps_a_branch_whose_squash_left_work_behind() {
        // The dangerous neighbour of the case above: the branch was squashed,
        // then gained a commit. Its content is NOT fully in the default branch
        // any more, and the containment check must not wave it through.
        let repo = create_committed_repo("merge-gate-squash-extra");
        let checkout = unique_temp_path("merge-gate-squash-extra-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/squashed-plus",
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::write(checkout.join("new.txt"), "one\n").unwrap();
        run_git(&checkout, &["add", "new.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "first"]);
        run_git(&repo, &["merge", "--squash", "feature/squashed-plus"]);
        run_git(&repo, &["commit", "--quiet", "-m", "squashed (#2)"]);

        // Work that landed after the squash — this is what would be lost.
        std::fs::write(checkout.join("later.txt"), "unmerged\n").unwrap();
        run_git(&checkout, &["add", "later.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "after the squash"]);

        assert_eq!(
            branch_merge_gate(&repo, &checkout, "feature/squashed-plus"),
            WorktreeMergeGate::NotMerged,
            "post-squash work must keep the branch"
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_merge_gate_requires_positive_evidence() {
        let repo = create_committed_repo("merge-gate-evidence");
        let checkout = unique_temp_path("merge-gate-evidence-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/unmerged",
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::write(checkout.join("new.txt"), "x\n").unwrap();
        run_git(&checkout, &["add", "new.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "feature work"]);

        // Unmerged branch: no evidence (gh pr view fails in a remote-less repo).
        assert_eq!(
            branch_merge_gate(&repo, &checkout, "feature/unmerged"),
            WorktreeMergeGate::NotMerged
        );

        // Merge it into the default branch: the git fallback now has evidence.
        let default = detect_default_branch(&repo).expect("default branch");
        run_git(&repo, &["merge", "--quiet", "feature/unmerged"]);
        let gate = branch_merge_gate(&repo, &checkout, "feature/unmerged");
        assert_eq!(
            gate,
            WorktreeMergeGate::Merged {
                evidence: format!("merged into {default}")
            }
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn delete_local_branch_removes_merged_branch() {
        let repo = create_committed_repo("merge-gate-delete");
        run_git(&repo, &["branch", "feature/done"]);
        delete_local_branch(&repo, "feature/done").expect("branch delete should succeed");
        let out = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args([
                "-C",
                repo.to_str().unwrap(),
                "branch",
                "--list",
                "feature/done",
            ])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn delete_local_branch_refuses_the_primary_branch() {
        // #121 floor: main/master are never deletable by this path.
        let repo = create_committed_repo("protect-main");
        // Move OFF the primary branch and ensure `main` exists but is unchecked-
        // out, so `git branch -D main` would otherwise SUCCEED — the vulnerable
        // condition. (RED without the floor: this returned Ok and pruned main.)
        run_git(&repo, &["checkout", "-b", "work"]);
        run_git(&repo, &["branch", "-f", "main", "HEAD"]);
        let err =
            delete_local_branch(&repo, "main").expect_err("primary branch delete must be refused");
        assert!(err.contains("main"), "message names the branch: {err}");
        assert!(
            run_command_capture(
                "git",
                &["-C", repo.to_str().unwrap(), "rev-parse", "main"],
                None
            )
            .is_ok(),
            "main must still exist after a refused delete"
        );
        // A non-primary branch still deletes.
        run_git(&repo, &["branch", "feature/x"]);
        delete_local_branch(&repo, "feature/x").expect("non-primary branch still deletes");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn is_protected_branch_covers_floor_default_and_config() {
        // Tier 1: hardcoded floor, even with no default detected and no config.
        assert!(is_protected_branch("main", None, &[]));
        assert!(is_protected_branch("master", None, &[]));
        // Tier 2: the repo's detected default branch.
        assert!(is_protected_branch("trunk", Some("trunk"), &[]));
        // Tier 3: config policy extends the set.
        let extra = vec!["develop".to_string(), "release/1.x".to_string()];
        assert!(is_protected_branch("develop", Some("main"), &extra));
        assert!(is_protected_branch("release/1.x", None, &extra));
        // An ordinary feature branch is never protected.
        assert!(!is_protected_branch("feature/thing", Some("main"), &extra));
    }

    #[test]
    fn github_repo_parses_ssh_and_https_remote_urls() {
        assert_eq!(
            github_repo_from_remote_url("git@github.com:gerchowl/flock.git").as_deref(),
            Some("gerchowl/flock")
        );
        assert_eq!(
            github_repo_from_remote_url("https://github.com/gerchowl/flock").as_deref(),
            Some("gerchowl/flock")
        );
        assert_eq!(github_repo_from_remote_url("https://example.com/x/y"), None);
        assert_eq!(github_repo_from_remote_url("git@github.com:broken"), None);
    }

    #[test]
    fn branch_merge_gate_accepts_remote_containment_in_feature_branch() {
        // origin bare repo; feature branch merged into a NON-default branch
        // that is pushed — the containment fallback must accept it.
        let origin = unique_temp_path("merge-gate-containment-origin");
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "--quiet", "--bare"]);

        let repo = create_committed_repo("merge-gate-containment-repo");
        run_git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        let default = detect_default_branch(&repo).expect("default branch");
        run_git(&repo, &["push", "--quiet", "origin", &default]);

        let checkout = unique_temp_path("merge-gate-containment-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature/float",
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::write(checkout.join("w.txt"), "w\n").unwrap();
        run_git(&checkout, &["add", "w.txt"]);
        run_git(&checkout, &["commit", "--quiet", "-m", "float work"]);
        run_git(&checkout, &["push", "--quiet", "origin", "feature/float"]);

        // Not merged anywhere else yet: own tracking ref must NOT count.
        assert_eq!(
            branch_merge_gate(&repo, &checkout, "feature/float"),
            WorktreeMergeGate::NotMerged
        );

        // Merge into a pushed integration branch (not the default).
        run_git(&repo, &["branch", "integration", &default]);
        run_git(&repo, &["checkout", "--quiet", "integration"]);
        run_git(&repo, &["merge", "--quiet", "feature/float"]);
        run_git(&repo, &["push", "--quiet", "origin", "integration"]);
        run_git(&repo, &["checkout", "--quiet", &default]);
        run_git(&repo, &["fetch", "--quiet", "origin"]);

        assert_eq!(
            branch_merge_gate(&repo, &checkout, "feature/float"),
            WorktreeMergeGate::Merged {
                evidence: "contained in origin/integration".to_string()
            }
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&origin);
    }
    #[test]
    fn main_root_from_common_dir_strips_dot_git() {
        assert_eq!(
            main_root_from_common_dir(std::path::Path::new("/repo/flock/.git")),
            std::path::PathBuf::from("/repo/flock")
        );
        assert_eq!(
            main_root_from_common_dir(std::path::Path::new("/repo/bare.git")),
            std::path::PathBuf::from("/repo/bare.git")
        );
    }
    #[test]
    fn pr_state_json_parses_all_states() {
        assert_eq!(
            parse_pr_state_fields("OPEN", 7, Some(false)),
            Some(PrStateInfo {
                state: PrState::Open,
                number: 7
            })
        );
        assert_eq!(
            parse_pr_state_fields("OPEN", 7, Some(true)),
            Some(PrStateInfo {
                state: PrState::Draft,
                number: 7
            })
        );
        assert_eq!(
            parse_pr_state_fields("MERGED", 5, Some(false)),
            Some(PrStateInfo {
                state: PrState::Merged,
                number: 5
            })
        );
        assert_eq!(
            parse_pr_state_fields("CLOSED", 2, Some(false)),
            Some(PrStateInfo {
                state: PrState::Closed,
                number: 2
            })
        );
        assert_eq!(parse_pr_state_fields("", 1, None), None);
        assert_eq!(parse_pr_state_fields("WEIRD", 1, None), None);
    }
}
