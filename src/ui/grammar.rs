//! Single source of the spaces/agents label grammar (#62).
//!
//! Every concrete checkout — local or remote — renders as `<server>:<target>`
//! (`mba22:main`, `sage:keyboard-shorcuts`). The server qualifier is always
//! present, the local host included, so "where is this checkout" is always
//! answered. The space row above carries the project identity itself
//! (`owner/repo` per #27), not a checkout. Keeping the rendering here in one
//! place makes the eventual origin-gossip merge (peers.rs may add an Origin
//! remote ref) trivial: only the call sites that build the `(server, target)`
//! pair change, never the formatting.

use crate::app::AppState;
use crate::workspace::Workspace;

/// The local server name, as it appears in member rows (`mba22:main`). Shared
/// with the servers band and status line so one machine reads the same name
/// everywhere.
pub(crate) fn local_server_name() -> String {
    crate::app::short_host_name()
}

/// `<server>:<target>` — the uniform member grammar. The single formatter all
/// concrete-checkout rows (local and remote) funnel through.
pub(crate) fn member_label(server: &str, target: &str) -> String {
    format!("{server}:{target}")
}

/// A member row's identity per the viewer's `server_label` mode (#164). In
/// `icon` mode a server that declared a (usable) icon reads `<glyph> · <target>`
/// — the symbol standing in for the hostname — falling back to the uniform
/// `<host>:<target>` when it has no icon. `name`/`both` keep `<host>:<target>`,
/// so the default look is unchanged; only the opt-in `icon` mode shifts.
pub(crate) fn member_label_moded(
    mode: crate::config::ServerLabelConfig,
    icon_name: Option<&str>,
    host: &str,
    target: &str,
) -> String {
    if mode == crate::config::ServerLabelConfig::Icon {
        if let Some(glyph) = icon_name.and_then(crate::server_icons::resolve) {
            return format!("{glyph} \u{00b7} {target}");
        }
    }
    member_label(host, target)
}

/// The target half of a local member's label: the branch for a git checkout
/// (the branch IS the label — the two-line name+branch row collapses), else
/// the workspace display name for misc (non-git) workspaces.
pub(crate) fn local_member_target(
    app: &AppState,
    ws: &Workspace,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
) -> String {
    if let Some(name) = &ws.custom_name {
        return name.clone();
    }
    if let Some(branch) = ws.branch() {
        return branch
            .strip_prefix("worktree/")
            .unwrap_or(&branch)
            .to_string();
    }
    ws.display_name_from(&app.terminals, terminal_runtimes)
}

/// The full `<local-server>:<target>` label for a local workspace member row.
pub(crate) fn local_member_label(
    app: &AppState,
    ws: &Workspace,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
) -> String {
    member_label_moded(
        app.server_label,
        crate::app::configured_node_icon().as_deref(),
        &local_server_name(),
        &local_member_target(app, ws, terminal_runtimes),
    )
}

/// The project identity label for a space row: `owner/repo` derived from the
/// machine-independent project key (`github.com/owner/repo` → `owner/repo`).
/// Origin-less repos (`dir:<name>`) and bare local paths fall back to their
/// trailing segment.
pub(crate) fn project_identity_label(project_key: &str) -> String {
    if let Some(rest) = project_key.strip_prefix("dir:") {
        return rest.to_string();
    }
    // host/owner/repo -> owner/repo; host/repo -> repo; bare -> as-is.
    let mut segments: Vec<&str> = project_key.split('/').filter(|s| !s.is_empty()).collect();
    match segments.len() {
        0 => project_key.to_string(),
        1 => segments.remove(0).to_string(),
        // Drop the host segment, keep the remaining owner/repo path.
        _ => segments[1..].join("/"),
    }
}

/// The label for a section-LEADER row (the selectable main checkout that heads
/// a multi-member project section): the PROJECT IDENTITY, never `<server>:<branch>`
/// (#78). Two different repos that both head as `mba22:main` are indistinguishable
/// under the member grammar — the leader must read the project, with its members
/// carrying the `<server>:<target>` qualifier beneath. Resolves to `owner/repo`
/// from the project key (#27), falling back to the workspace's display label when
/// the git identity hasn't resolved (no project key yet).
pub(crate) fn leader_label(
    app: &AppState,
    ws: &Workspace,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
) -> String {
    match ws.project_key() {
        Some(key) => project_identity_label(key),
        None => ws.display_name_from(&app.terminals, terminal_runtimes),
    }
}

/// The label for a SOLO local row — a project's lone local checkout, with no
/// sibling members to fold beneath it (#92). Mirrors solo remotes (#81): the
/// row IS both the project AND its only checkout, so it reads
/// `<owner/repo> · <server>:<branch>` — identity first, member locator second,
/// one line, no synthetic group. When the git identity hasn't resolved (no
/// project key yet) we fall back to the leader's display label alone, never
/// bare `<server>:<branch>` — a solo row without identity must not look like
/// a member of an absent group.
pub(crate) fn solo_local_label(
    app: &AppState,
    ws: &Workspace,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
) -> String {
    match ws.project_key() {
        Some(key) => format!(
            "{} \u{00b7} {}",
            project_identity_label(key),
            local_member_label(app, ws, terminal_runtimes),
        ),
        None => leader_label(app, ws, terminal_runtimes),
    }
}

/// The PR glyph + number for a member row, sharing the pane-header symbol set:
/// open `⊙`, draft `◐`, merged `✓`, closed `✗`. Returns `(text, color)` where
/// `text` is `#<n> <glyph>`.
pub(crate) fn pr_glyph(
    pr: crate::worktree::PrStateInfo,
    p: &crate::app::state::Palette,
) -> (String, ratatui::style::Color) {
    let (glyph, color) = match pr.state {
        crate::worktree::PrState::Open => ("\u{2299}", p.accent),
        crate::worktree::PrState::Draft => ("\u{25d0}", p.overlay0),
        crate::worktree::PrState::Merged => ("\u{2713}", p.mauve),
        crate::worktree::PrState::Closed => ("\u{2717}", p.red),
    };
    (format!("#{} {glyph}", pr.number), color)
}

/// The target half of a remote member's label, from a peer workspace summary:
/// the branch when present, else the remote workspace name (misc).
pub(crate) fn remote_member_target(summary: &crate::api::schema::PeerWorkspaceSummary) -> String {
    summary
        .branch
        .as_deref()
        .unwrap_or(summary.workspace.as_str())
        .to_string()
}

/// The self-contained label for a remote workspace shown as its OWN flat row
/// (the fleet-wide navigator): `<owner/repo> · <host>:<target>` — project
/// identity plus the full member locator, ALWAYS both. This is the remote twin
/// of [`solo_local_label`]; unlike [`remote_entry_label`](crate::ui::sidebar)
/// it never drops the member half, because the navigator is a flat list with no
/// group-leader row to nest members beneath. When the peer reports no project
/// identity, falls back to the bare `<host>:<target>` member label.
pub(crate) fn solo_remote_label(
    mode: crate::config::ServerLabelConfig,
    peer: &crate::peers::PeerSummaryState,
    summary: &crate::api::schema::PeerWorkspaceSummary,
) -> String {
    let host = peer.host.as_deref().unwrap_or(peer.peer.as_str());
    let member = member_label_moded(
        mode,
        peer.icon.as_deref(),
        host,
        &remote_member_target(summary),
    );
    match summary
        .project_key
        .as_deref()
        .map(project_identity_label)
        .or_else(|| summary.project_label.clone())
    {
        Some(project) => format!("{project} \u{00b7} {member}"),
        None => member,
    }
}

/// The SERVER segment of a space-joined location string, per the viewer's
/// `server_label` mode (#164): `name` = the bare hostname (the pre-icon look),
/// `both` = `<glyph> <host>`, `icon` = the glyph alone. A node that declared no
/// usable icon falls back to its hostname in every mode, so the segment is
/// never empty and never renders a raw/unresolvable name.
///
/// This is the space-joined twin of [`member_label_moded`]: member rows read
/// `<glyph> \u{00b7} <branch>` because their grammar is `<server>:<target>`,
/// while a location is already `<server> <proj> <target>` and needs no
/// separator. Feed the result to [`agent_location_label`] as its `server`.
pub(crate) fn server_field_label(
    mode: crate::config::ServerLabelConfig,
    icon_name: Option<&str>,
    host: &str,
) -> String {
    use crate::config::ServerLabelConfig;
    let glyph = icon_name.and_then(crate::server_icons::resolve);
    match (mode, glyph) {
        (ServerLabelConfig::Name, _) | (_, None) => host.to_string(),
        (ServerLabelConfig::Both, Some(glyph)) => format!("{glyph} {host}"),
        (ServerLabelConfig::Icon, Some(glyph)) => glyph.to_string(),
    }
}

/// The agents-panel single-row location string (#62), matching the spaces
/// grammar: `<server> <proj> <target>` (e.g. `mba22 flock keyboard-shorcuts`).
/// Under width pressure the location truncates right-to-left: the branch/target
/// shrinks first (middle-truncated), then the project, while the server
/// qualifier stays whole so "where" is always answered. Returns the rendered
/// location that fits `max_width` columns; the leading `<icon> <agent> ` is the
/// caller's responsibility and is excluded from `max_width`.
pub(crate) fn agent_location_label(
    server: &str,
    project: Option<&str>,
    target: &str,
    max_width: usize,
) -> String {
    if max_width == 0 {
        return String::new();
    }
    // Assemble server → proj → target, dropping the project segment entirely
    // before sacrificing the server qualifier.
    let mut segments: Vec<&str> = Vec::with_capacity(3);
    segments.push(server);
    if let Some(project) = project.filter(|p| !p.is_empty()) {
        segments.push(project);
    }
    segments.push(target);

    let joined = segments.join(" ");
    if joined.chars().count() <= max_width {
        return joined;
    }

    // Over budget: shrink target first (it carries the least-stable identity),
    // then drop the project, keeping the server whole.
    let sep = 1; // single space between segments
    let server_len = server.chars().count();
    let has_project = segments.len() == 3;

    if has_project {
        let project = segments[1];
        let project_len = project.chars().count();
        // Width left for the target after server + proj + two separators.
        let fixed = server_len + sep + project_len + sep;
        if fixed < max_width {
            let target_budget = max_width - fixed;
            return format!(
                "{server} {project} {}",
                crate::terminal::middle_truncate_chars(target, target_budget)
            );
        }
        // Even a 1-col target won't fit alongside the project: drop the project.
    }

    let fixed = server_len + sep;
    if fixed < max_width {
        let target_budget = max_width - fixed;
        return format!(
            "{server} {}",
            crate::terminal::middle_truncate_chars(target, target_budget)
        );
    }
    // Server alone overflows: middle-truncate the whole thing.
    crate::terminal::middle_truncate_chars(&joined, max_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_label_joins_server_and_target() {
        assert_eq!(member_label("mba22", "main"), "mba22:main");
        assert_eq!(
            member_label("sage", "keyboard-shorcuts"),
            "sage:keyboard-shorcuts"
        );
    }

    #[test]
    fn project_identity_strips_host_to_owner_repo() {
        assert_eq!(
            project_identity_label("github.com/gerchowl/flock"),
            "gerchowl/flock"
        );
        // Host + single path segment -> that segment.
        assert_eq!(project_identity_label("example.com/flock"), "flock");
    }

    #[test]
    fn project_identity_uses_dir_fallback_name() {
        assert_eq!(project_identity_label("dir:scratch"), "scratch");
    }

    #[test]
    fn project_identity_keeps_bare_key() {
        assert_eq!(project_identity_label("flock"), "flock");
    }

    #[test]
    fn agent_location_joins_server_proj_target_when_it_fits() {
        assert_eq!(
            agent_location_label("mba22", Some("flock"), "keyboard-shorcuts", 80),
            "mba22 flock keyboard-shorcuts"
        );
    }

    #[test]
    fn agent_location_omits_absent_project() {
        assert_eq!(agent_location_label("sage", None, "main", 80), "sage main");
    }

    #[test]
    fn agent_location_truncates_target_before_project() {
        // Server + project stay whole; the target shrinks (middle-truncated).
        let out = agent_location_label("mba22", Some("flock"), "keyboard-shorcuts", 20);
        assert!(out.starts_with("mba22 flock "), "got {out:?}");
        assert!(out.chars().count() <= 20, "got {out:?}");
        assert!(out.contains('…'), "got {out:?}");
    }

    #[test]
    fn solo_local_label_combines_identity_with_member_grammar() {
        // Mirrors the shape `remote_entry_label` already produces for solo
        // remotes (#81): `<owner/repo> · <server>:<branch>`.
        let mut app = crate::app::AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("flock");
        ws.custom_name = None;
        ws.cached_git_branch = Some("keyboard-shorcuts".into());
        ws.cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "/repo/flock/.git".into(),
            checkout_key: "/repo/flock".into(),
            label: "flock".into(),
            repo_root: std::path::PathBuf::from("/repo/flock"),
            is_linked_worktree: false,
            project_key: "github.com/gerchowl/flock".into(),
        });
        app.workspaces = vec![ws];
        let runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let label = solo_local_label(&app, &app.workspaces[0], &runtimes);
        let server = local_server_name();
        assert_eq!(
            label,
            format!("gerchowl/flock \u{00b7} {server}:keyboard-shorcuts")
        );
    }

    #[test]
    fn solo_local_label_unresolved_falls_back_to_display_label() {
        let mut app = crate::app::AppState::test_new();
        // Plain test workspace: no `cached_git_space`, so `project_key()` is
        // None — the identity hasn't resolved. Must NOT fall through to
        // `<server>:<branch>` (that would read like a member of an absent
        // group); display label alone.
        app.workspaces = vec![crate::workspace::Workspace::test_new("scratch")];
        let runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let label = solo_local_label(&app, &app.workspaces[0], &runtimes);
        assert_eq!(label, "scratch");
        assert!(!label.contains(':'));
    }

    fn remote_summary(
        project_key: Option<&str>,
        branch: Option<&str>,
    ) -> crate::api::schema::PeerWorkspaceSummary {
        crate::api::schema::PeerWorkspaceSummary {
            id: "ws_1".into(),
            workspace: "flock".into(),
            project_key: project_key.map(str::to_string),
            project_label: project_key.map(|_| "flock".to_string()),
            branch: branch.map(str::to_string),
            is_linked_worktree: branch.is_some(),
            agent: Some("cc".into()),
            status: crate::api::schema::AgentStatus::Idle,
            status_age_secs: None,
            activity: None,
            agents: Vec::new(),
        }
    }

    fn peer_named(host: &str) -> crate::peers::PeerSummaryState {
        let mut peer = crate::peers::PeerSummaryState::new(&crate::config::PeerConfig {
            name: host.into(),
            ..Default::default()
        });
        peer.host = Some(host.into());
        peer
    }

    #[test]
    fn solo_remote_label_combines_project_and_member() {
        // The remote twin of solo_local_label: `owner/repo · host:branch`.
        let peer = peer_named("sage");
        let summary = remote_summary(Some("github.com/gerchowl/flock"), Some("main"));
        assert_eq!(
            solo_remote_label(crate::config::ServerLabelConfig::Both, &peer, &summary),
            "gerchowl/flock \u{00b7} sage:main"
        );
    }

    #[test]
    fn solo_remote_label_without_project_is_bare_member() {
        // No project identity reported: fall back to `host:target`, never a
        // dangling `· `.
        let peer = peer_named("sage");
        let summary = remote_summary(None, Some("wip"));
        assert_eq!(
            solo_remote_label(crate::config::ServerLabelConfig::Both, &peer, &summary),
            "sage:wip"
        );
    }

    #[test]
    fn member_label_moded_icon_replaces_host_with_glyph() {
        use crate::config::ServerLabelConfig;
        let glyph = crate::server_icons::glyph("anvil").unwrap();
        // `icon` mode: the server's glyph stands in for the host, `· branch`.
        assert_eq!(
            member_label_moded(ServerLabelConfig::Icon, Some("anvil"), "anvil", "fix/pty"),
            format!("{glyph} \u{00b7} fix/pty")
        );
        // No usable icon → falls back to the uniform `host:branch`.
        assert_eq!(
            member_label_moded(ServerLabelConfig::Icon, None, "ksb", "wip"),
            "ksb:wip"
        );
        // `both` / `name` are unchanged: always `host:branch`.
        assert_eq!(
            member_label_moded(ServerLabelConfig::Both, Some("anvil"), "anvil", "fix/pty"),
            "anvil:fix/pty"
        );
        assert_eq!(
            member_label_moded(ServerLabelConfig::Name, Some("anvil"), "anvil", "fix/pty"),
            "anvil:fix/pty"
        );
    }

    #[test]
    fn server_field_label_follows_the_server_label_mode() {
        use crate::config::ServerLabelConfig;
        let glyph = crate::server_icons::glyph("toad").unwrap();
        // `name` keeps the bare hostname even when an icon is declared.
        assert_eq!(
            server_field_label(ServerLabelConfig::Name, Some("toad"), "sage"),
            "sage"
        );
        // `both` = the glyph, then the hostname, space-joined like the rest of
        // the location string.
        assert_eq!(
            server_field_label(ServerLabelConfig::Both, Some("toad"), "sage"),
            format!("{glyph} sage")
        );
        // `icon` = the symbol standing in for the host.
        assert_eq!(
            server_field_label(ServerLabelConfig::Icon, Some("toad"), "sage"),
            glyph
        );
    }

    #[test]
    fn server_field_label_falls_back_to_the_host_without_a_usable_icon() {
        use crate::config::ServerLabelConfig;
        for mode in [
            ServerLabelConfig::Name,
            ServerLabelConfig::Both,
            ServerLabelConfig::Icon,
        ] {
            // No icon at all, and a name the registry does not know (a
            // version-skewed or hostile peer) both fall back to the host —
            // the raw name never reaches the screen.
            assert_eq!(server_field_label(mode, None, "ksb"), "ksb");
            assert_eq!(
                server_field_label(mode, Some("no-such-icon-name"), "ksb"),
                "ksb"
            );
        }
    }

    #[test]
    fn solo_remote_label_icon_mode_uses_glyph_member() {
        let glyph = crate::server_icons::glyph("toad").unwrap();
        let mut peer = peer_named("sage");
        peer.icon = Some("toad".into());
        let summary = remote_summary(Some("github.com/gerchowl/flock"), Some("main"));
        assert_eq!(
            solo_remote_label(crate::config::ServerLabelConfig::Icon, &peer, &summary),
            format!("gerchowl/flock \u{00b7} {glyph} \u{00b7} main")
        );
    }

    #[test]
    fn agent_location_drops_project_under_hard_pressure() {
        // Too tight for any project segment: drop it, keep server + target.
        let out = agent_location_label("mba22", Some("flock"), "main", 9);
        assert!(out.starts_with("mba22 "), "got {out:?}");
        assert!(!out.contains("flock"), "got {out:?}");
        assert!(out.chars().count() <= 9, "got {out:?}");
    }
}
