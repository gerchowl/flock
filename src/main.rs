#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "pre-runtime + pre-TUI user messages on the process's own terminal"
)]
use std::io;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;

pub(crate) const FLOCK_ENV_VAR: &str = "FLOCK_ENV";
pub(crate) const FLOCK_ENV_VALUE: &str = "1";
const NESTED_FLOCK_MESSAGES: [&str; 6] = [
    "inception detected. we need to go deeper... said no one ever.",
    "recursion is a pathway to many abilities some consider to be... unnatural.",
    "you were so preoccupied with whether you could, you didn't stop to think if you should. — dr. malcolm",
    "recursive flocking is disabled. somewhere, a call stack breathes a sigh of relief.",
    "recursive descent denied. there is, in fact, such a thing as too much flock.",
    "recursion detected. base case not found. aborting.",
];

mod agent_resume;
mod api;
mod app;
mod build_info;
mod checksum;
mod cli;
mod client;
mod config;
mod detect;
mod events;
mod ghostty;
mod handoff_runtime;
mod input;
mod integration;
mod ipc;
mod kitty_graphics;
mod layout;
mod logging;
mod pane;
mod peers;
mod persist;
mod platform;
mod process;
mod product_announcements;
mod protocol;
mod pty;
mod raw_input;
mod release_notes;
mod remote;
mod render_prof;
mod selection;
mod server;
mod session;
mod sound;
mod system_stats;
mod terminal;
mod terminal_notify;
mod terminal_theme;
mod ui;
mod update;
#[cfg(feature = "web")]
mod web;
mod workspace;
mod worktree;

fn init_logging() {
    crate::logging::init_file_logging("flock.log");
}

/// `flk --default-config` output: the machine-true serde defaults, derived —
/// never hand-maintained (it drifted whenever a field landed in `Config`
/// without an edit here; round-trip-tested in `default_config_output_parses_
/// back_to_default_config`). The annotated prose lives in
/// docs/config-reference.md.
fn default_config_toml() -> String {
    let body =
        toml::to_string_pretty(&config::Config::default()).expect("Config::default serializes");
    format!(
        "# flock configuration — annotated reference: docs/config-reference.md\n\
         # File: ~/.config/flock/config.toml (or $FLOCK_CONFIG_PATH). Read-only\n\
         # bases (nix/HM symlinks) take edits in config.local.toml (deep-merged,\n\
         # overlay wins). Values below are the built-in defaults.\n\n{body}"
    )
}

fn should_block_nested(config: &config::Config) -> bool {
    should_block_nested_for_env(config, std::env::var(FLOCK_ENV_VAR).ok().as_deref())
}

fn should_block_nested_for_env(config: &config::Config, flock_env: Option<&str>) -> bool {
    !config.experimental.allow_nested && flock_env == Some(FLOCK_ENV_VALUE)
}

fn random_nested_message() -> &'static str {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as usize)
        .unwrap_or(0);
    let index = (nanos ^ (std::process::id() as usize)) % NESTED_FLOCK_MESSAGES.len();
    NESTED_FLOCK_MESSAGES[index]
}

fn exit_if_nested_disabled(config: &config::Config) {
    if should_block_nested(config) {
        eprintln!("\x1b[1merror:\x1b[0m nested flock is disabled by default.");
        eprintln!("see configuration if you want to enable it.");
        eprintln!();
        eprintln!("\x1b[2m\"{}\"\x1b[0m", random_nested_message());
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachLeg {
    /// Local server/client attach (auto-detect launch).
    Local,
    /// Remote attach over the SSH bridge.
    Remote(remote::RemoteLaunch),
}

/// Run attach legs until the client exits without requesting a server
/// switch. A leg's client records its switch target (a federated peer's SSH
/// destination, from a sidebar remote row) in the switch file; each recorded
/// target chains into a fresh `--remote` leg.
///
/// Every leg — local in-process and remote via the spawned `flk client`
/// subprocess — funnels into `client::run_client_with_mode`, which retries
/// attaches refused with the live-handoff notice (#38) and re-captures the
/// host terminal theme per leg for the attach handshake (#47). A SwitchServer
/// relaunch racing a handoff therefore waits inside the leg instead of
/// bailing here.
fn run_attach_legs(first: AttachLeg) -> io::Result<()> {
    let switch_file = std::env::temp_dir().join(format!("flock-switch-{}", std::process::id()));
    // Inherited by the (possibly nested) client process of every leg.
    std::env::set_var(client::SWITCH_FILE_ENV_VAR, &switch_file);

    let mut leg = first;
    // The leg to bounce back to if the NEXT one fails to establish (#63): the
    // server the user switched away from. While set, a leg that dies before
    // its client attaches re-attaches `previous` with a failure notice instead
    // of stranding the user at a raw shell.
    let mut previous: Option<(AttachLeg, String)> = None;
    // Whether some leg held the alternate screen for a seamless swap (#63).
    // Once a switch fires, a dying chain must reclaim the host terminal.
    let mut handoff_held = false;
    loop {
        // A leg chained in after a seamless switch inherits the previous leg's
        // held terminal (frozen frame, raw mode) until it repaints (#69). Tell
        // it so an abnormal exit in its retry window reclaims the terminal
        // instead of stranding the user behind the frame. Cleared otherwise so
        // a first/clean leg never thinks it inherited a hold.
        if handoff_held {
            std::env::set_var(client::HELD_TERMINAL_ENV_VAR, "1");
        } else {
            std::env::remove_var(client::HELD_TERMINAL_ENV_VAR);
        }
        let result = match &leg {
            AttachLeg::Local => server::autodetect::auto_detect_launch(),
            AttachLeg::Remote(launch) => remote::run_remote(launch.clone()),
        };
        // The notice (if any) was a one-shot for this leg's attach; consume it
        // so a later switch off this leg does not re-show it.
        std::env::remove_var(client::SWITCH_NOTICE_ENV_VAR);

        let switch = client::take_switch_target(&switch_file);
        // An origin-workspace row going home (#66) carries the workspace to
        // focus once the home leg attaches. The next leg's server is the
        // local one (it may still be starting up), so fire a detached helper
        // that retries `workspace focus` against the local socket — the same
        // post-attach focus a config-peer leap does via ssh, but home-bound.
        if let Some(focus) = switch.as_ref().and_then(|s| s.focus_workspace.clone()) {
            spawn_home_focus(focus);
        }
        match decide_next_leg(&leg, switch, result, previous.take(), handoff_held) {
            LegStep::Switch {
                next,
                previous: prev,
            } => {
                handoff_held = true;
                previous = Some(prev);
                leg = next;
            }
            LegStep::FallBack {
                to,
                notice,
                previous: prev,
            } => {
                std::env::set_var(client::SWITCH_NOTICE_ENV_VAR, notice);
                previous = prev;
                leg = to;
            }
            LegStep::Finish {
                result,
                restore_terminal,
            } => {
                // A switch may have held the alternate screen for the seamless
                // swap (#63). If the chain dies with nothing left to reclaim
                // the screen, restore the host terminal so the user is not
                // stranded in a frozen alt-screen with raw mode on.
                if restore_terminal {
                    client::force_restore_host_terminal();
                }
                return result;
            }
        }
    }
}

/// What the leg loop does after one leg ends. Pure decision — no I/O — so the
/// switch / fall-back / finish branching (#63) is unit-testable.
enum LegStep {
    /// The leg requested a switch: run `next`, remembering `previous` to fall
    /// back to if `next` fails to establish.
    Switch {
        next: AttachLeg,
        previous: (AttachLeg, String),
    },
    /// The switch leg failed to establish: re-attach `to` (the previous
    /// server) with `notice` so the user lands back, told why.
    FallBack {
        to: AttachLeg,
        notice: String,
        previous: Option<(AttachLeg, String)>,
    },
    /// The chain is done (clean exit, or a failure with nowhere to fall back).
    Finish {
        result: io::Result<()>,
        restore_terminal: bool,
    },
}

fn decide_next_leg(
    current: &AttachLeg,
    switch: Option<client::RecordedSwitch>,
    result: io::Result<()>,
    previous: Option<(AttachLeg, String)>,
    handoff_held: bool,
) -> LegStep {
    match switch {
        // The reserved home target re-attaches locally: the way home from a
        // spoke is client knowledge, not server-side ssh config.
        Some(switch) if switch.target == protocol::HOME_SWITCH_TARGET => LegStep::Switch {
            previous: (current.clone(), switch_failure_label(&AttachLeg::Local)),
            next: AttachLeg::Local,
        },
        Some(switch) => {
            let keybindings = match current {
                AttachLeg::Remote(launch) => launch.keybindings,
                // Match the CLI's --remote-keybindings default.
                AttachLeg::Local => remote::RemoteKeybindings::Local,
            };
            let next = AttachLeg::Remote(remote::RemoteLaunch {
                target: switch.target,
                keybindings,
                live_handoff: false,
                fleet: switch.fleet,
                // The leg loop may still hold the previous leg's alt-screen
                // for the seamless swap (#69/#72). A federation switch must
                // never prompt for install/upgrade here — the held alt-screen
                // would swallow the prompt and the stdin read would block /
                // corrupt the terminal (#115). Fail-with-notice instead so
                // the user lands back at the previous leg with a top-right
                // failure notice (#67).
                context: remote::LaunchContext::FederationSwitch,
            });
            LegStep::Switch {
                previous: (current.clone(), switch_failure_label(&next)),
                next,
            }
        }
        // No switch requested: the leg exited cleanly or FAILED to establish.
        // A failure with a previous leg on hand is a failed switch — bounce
        // back with the reason as a top-right notice (#63), never strand at a
        // shell. A clean exit, or a failure with nowhere to fall back, ends.
        None => match (result, previous) {
            (Err(err), Some((fallback, target_label))) => LegStep::FallBack {
                notice: format!(
                    "switch to {target_label} failed: {}",
                    switch_failure_reason(&err)
                ),
                to: fallback,
                previous: None,
            },
            (result, _) => LegStep::Finish {
                // A held alt-screen (a switch fired earlier in the chain) that
                // ends on an error has no leg left to reclaim the screen.
                restore_terminal: result.is_err() && handoff_held,
                result,
            },
        },
    }
}

/// Display name of a switch target for the failure notice (#63): the remote
/// ssh target, or "home" for the reserved local re-attach.
fn switch_failure_label(leg: &AttachLeg) -> String {
    match leg {
        AttachLeg::Local => "home".to_string(),
        AttachLeg::Remote(launch) => launch.target.clone(),
    }
}

/// Focus a workspace on the local server once it comes up after a home
/// re-attach from an origin-workspace row (#66). The home leg is about to
/// (re)start the local server, so a detached thread retries the focus over
/// the local API socket until it lands or the budget runs out — the spoke
/// has no route to drive the hub, so the focus must fire here. Best-effort:
/// a failure just lands home on whatever workspace was last focused.
fn spawn_home_focus(workspace_id: String) {
    use crate::api::schema::{Method, Request, WorkspaceTarget};
    std::thread::spawn(move || {
        // ~10s budget: covers a cold local-server start; cheap if already up.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let request = Request {
            id: "launcher:home-focus".to_string(),
            method: Method::WorkspaceFocus(WorkspaceTarget {
                workspace_id: workspace_id.clone(),
            }),
        };
        loop {
            if crate::api::client::ApiClient::local()
                .request(request.clone())
                .is_ok()
            {
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
}

/// A concise, single-line reason from a failed leg launch for the notice.
fn switch_failure_reason(err: &io::Error) -> String {
    let text = err.to_string();
    let line = text.lines().next().unwrap_or(&text).trim();
    if line.is_empty() {
        "connection failed".to_string()
    } else {
        line.to_string()
    }
}

fn main() -> io::Result<()> {
    let raw_args: Vec<String> = std::env::args().collect();
    let args = match session::configure_from_args(&raw_args) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!("run 'flk --help' for usage");
            std::process::exit(2);
        }
    };
    let (args, remote_launch) = match remote::extract_remote_args(&args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!("run 'flk --help' for usage");
            std::process::exit(2);
        }
    };

    if remote_launch.is_some()
        && args.get(1).is_some()
        && !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "--help" | "-h" | "--version" | "-V" | "--default-config"
            )
        })
    {
        eprintln!("error: --remote can only be used with the default launch command");
        eprintln!("run 'flk --help' for usage");
        std::process::exit(2);
    }

    if let cli::CommandOutcome::Handled(code) = cli::maybe_run(&args)? {
        std::process::exit(code);
    }

    // Subcommands and flags (no TUI, no logging needed)
    if args.get(1).map(|s| s.as_str()) == Some("remote-client-bridge") {
        return remote::run_remote_client_bridge();
    }

    if args.get(1).map(|s| s.as_str()) == Some("server") {
        return server::headless::run_server();
    }

    // Hidden client mode: connect to an existing server's client socket.
    if args.get(1).map(|s| s.as_str()) == Some("client") {
        let loaded_config = config::Config::load();
        exit_if_nested_disabled(&loaded_config.config);
        return client::run_client();
    }

    if args.get(1).map(|s| s.as_str()) == Some("update") {
        let options = match update::parse_self_update_args(&args[2..]) {
            Ok(options) => options,
            Err(err) if err.starts_with("usage:") => {
                eprintln!("{err}");
                std::process::exit(0);
            }
            Err(err) => {
                eprintln!("{err}");
                eprintln!("usage: flk update [--handoff]");
                std::process::exit(2);
            }
        };
        match update::self_update(options) {
            Ok(_) => return Ok(()),
            Err(e) => {
                if e.starts_with("self-update is disabled") {
                    eprintln!("{e}");
                } else {
                    eprintln!("update failed: {e}");
                }
                std::process::exit(1);
            }
        }
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("flock — terminal workspace manager for AI coding agents");
        println!();
        println!("Usage: flk [options]");
        println!("       flk --session <name> [options]");
        println!("       flk --remote <ssh-target> [--session <name>]");
        println!("       flk session attach <name>");
        println!("       flk update [--handoff]");
        println!("       flk channel set <stable|preview>");
        println!("       flk server stop");
        println!("       flk server reload-config");
        println!("       flk config <subcommand> ...");
        println!("       flk channel <subcommand> ...");
        println!("       flk workspace <subcommand> ...");
        println!("       flk worktree <subcommand> ...");
        println!("       flk tab <subcommand> ...");
        println!("       flk notification <subcommand> ...");
        println!("       flk agent <subcommand> ...");
        println!("       flk pane <subcommand> ...");
        println!("       flk wait <subcommand> ...");
        println!("       flk session <subcommand> ...");
        println!("       flk integration <subcommand> ...");
        println!("       flk web [--bind <addr>] (requires the `web` feature)");
        println!();
        println!("Common commands:");
        for (command, description) in [
            ("flk", "Launch or attach to the persistent session"),
            (
                "flk status [server|client]",
                "Show local client and running server status",
            ),
            ("flk update", "Download and install the latest version"),
            (
                "flk server stop",
                "Stop the running server via the API socket",
            ),
            (
                "flk channel set <stable|preview>",
                "Choose the stable or preview update channel",
            ),
            (
                "flk server reload-config",
                "Reload config.toml in the running server",
            ),
            (
                "flk config reset-keys",
                "Back up config.toml and remove custom keybindings",
            ),
            (
                "flk channel <subcommand>",
                "Manage the stable or preview update channel",
            ),
            (
                "flk workspace <subcommand>",
                "Workspace helpers over the socket API",
            ),
            (
                "flk worktree <subcommand>",
                "Git worktree helpers over the socket API",
            ),
            ("flk tab <subcommand>", "Tab helpers over the socket API"),
            (
                "flk notification <subcommand>",
                "Notification helpers over the socket API",
            ),
            (
                "flk agent <subcommand>",
                "Agent/terminal helpers over the socket API",
            ),
            (
                "flk pane <subcommand>",
                "Pane control helpers over the socket API",
            ),
            (
                "flk wait <subcommand>",
                "Blocking wait helpers over the socket API",
            ),
            (
                "flk session <subcommand>",
                "Manage named persistent sessions",
            ),
            (
                "flk integration <subcommand>",
                "Manage built-in agent integrations",
            ),
        ] {
            println!("  {command:<32} {description}");
        }
        println!();
        println!("Advanced commands:");
        println!("  {:<32} Run as headless server", "flk server");
        println!();
        println!("Options:");
        println!("  --no-session        Run monolithically (no server/client, escape hatch)");
        println!("  --session <name>    Use or create a named persistent session");
        println!("  --remote <target>   Attach through SSH to a remote Flock server");
        println!("  --remote-keybindings <local|server>");
        println!("                      Keybindings for --remote app attach (default: local)");
        println!("  --handoff           Opt into live handoff for update or remote attach");
        println!("  --default-config    Print default configuration and exit");
        println!("  --version, -V       Print version and exit");
        println!("  --help, -h          Show this help");
        println!();
        println!("Config: {}", config::config_path().display());
        println!("Logs:   {}", logging::help_log_paths_summary());
        println!("Env:    FLOCK_CONFIG_PATH overrides config file path");
        println!("Home:   https://flock.dev");
        return Ok(());
    }

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("flk {}", crate::build_info::version());
        return Ok(());
    }

    if args.iter().any(|a| a == "--default-config") {
        print!("{}", default_config_toml());
        return Ok(());
    }

    // Reject unknown flags
    let known_flags = [
        "--no-session",
        "--session",
        "--remote",
        "--remote-keybindings",
        "--version",
        "-V",
        "--default-config",
        "--help",
        "-h",
    ];
    for arg in &args[1..] {
        let arg_name = arg.split_once('=').map(|(name, _)| name).unwrap_or(arg);
        if arg.starts_with('-') && !known_flags.contains(&arg_name) {
            eprintln!("unknown option: {arg}");
            eprintln!("run 'flk --help' for usage");
            std::process::exit(1);
        }
        if !arg.starts_with('-')
            && ![
                "server",
                "client",
                "remote-client-bridge",
                "update",
                "status",
                "config",
                "channel",
                "workspace",
                "worktree",
                "pane",
                "wait",
                "session",
                "integration",
            ]
            .contains(&arg.as_str())
        {
            eprintln!("unknown command: {arg}");
            eprintln!("run 'flk --help' for usage");
            std::process::exit(1);
        }
    }

    if let Some(remote_launch) = remote_launch {
        return run_attach_legs(AttachLeg::Remote(remote_launch));
    }

    let loaded_config = config::Config::load();
    exit_if_nested_disabled(&loaded_config.config);

    let no_session = args.iter().any(|a| a == "--no-session");

    // Auto-detect launch: when --no-session is NOT set, use server/client mode.
    // Check if a server is running, spawn one if needed, then attach as client.
    if !no_session {
        if let Err(err) = run_attach_legs(AttachLeg::Local) {
            eprintln!("flk: {err}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // --- Monolithic mode (--no-session escape hatch) ---
    // This is the pre-mission single-process behavior.

    init_logging();

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();
    let _api_server = match api::start_server_with_capabilities(api_tx, event_hub.clone(), None) {
        Ok(server) => server,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            eprintln!("error: flk is already running");
            eprintln!("socket: {}", api::socket_path().display());
            std::process::exit(1);
        }
        Err(err) => return Err(err),
    };

    let modify_other_keys_mode = crate::input::host_modify_other_keys_mode(
        std::env::var("TMUX").is_ok(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var_os("WEZTERM_PANE").is_some(),
    );

    let original_hook = std::panic::take_hook();
    let panic_resets_modify_other_keys = modify_other_keys_mode.is_some();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("PANIC: {info}");
        if panic_resets_modify_other_keys {
            let _ = std::io::Write::write_all(&mut io::stdout(), b"\x1b[>4;0m");
        }
        if crate::kitty_graphics::is_enabled() {
            let _ = crate::kitty_graphics::clear_all_host_graphics();
        }
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        ratatui::restore();
        original_hook(info);
    }));

    let config = &loaded_config.config;
    let config_diagnostic = config::config_diagnostic_summary(&loaded_config.diagnostics);
    logging::startup("app");

    // Background update check (non-blocking, best-effort)
    // Only checks for newer versions and notifies the TUI.
    // Skipped in --no-session mode (testing).

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    let result = rt.block_on(async {
        let mut terminal = ratatui::init();
        if config.ui.mouse_capture {
            execute!(io::stdout(), EnableMouseCapture)?;
        } else {
            execute!(io::stdout(), DisableMouseCapture)?;
        }
        execute!(
            io::stdout(),
            EnableBracketedPaste,
            EnableFocusChange,
            PushKeyboardEnhancementFlags(crate::input::ime_compatible_keyboard_enhancement_flags())
        )?;

        // Some hosts do not honor Kitty keyboard enhancement pushes for
        // Shift+Enter. Enable xterm modifyOtherKeys only on hosts where we
        // know it is needed and parseable, so modified Enter stays distinct.
        if let Some(mode) = modify_other_keys_mode {
            use std::io::Write;
            std::io::stdout().write_all(mode.set_sequence())?;
            std::io::stdout().flush()?;
        }

        let mut app = app::App::new(
            config,
            true, // no_session — monolithic mode never saves/restores sessions
            config_diagnostic,
            api_rx,
            event_hub,
        );
        let result = app.run(&mut terminal).await;

        // Reset modifyOtherKeys if we enabled it.
        if modify_other_keys_mode.is_some() {
            use std::io::Write;
            std::io::stdout().write_all(b"\x1b[>4;0m")?;
            std::io::stdout().flush()?;
        }

        if crate::kitty_graphics::is_enabled() {
            crate::kitty_graphics::clear_all_host_graphics()?;
        }
        execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture
        )?;
        ratatui::restore();

        // Drop app (and all workspaces/panes) before runtime shuts down
        drop(app);

        result
    });

    // Shut down runtime immediately — kills lingering PTY reader/writer tasks
    rt.shutdown_timeout(std::time::Duration::from_millis(100));

    logging::shutdown("app");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_output_parses_back_to_default_config() {
        // ADR-0002 phase (g): `flk --default-config` must print EXACTLY the
        // serde defaults — the hand-maintained TOML shadow drifted whenever a
        // field landed in Config without an edit here. Round-trip guard: parse
        // the printed text and compare against Config::default() structurally.
        let printed = default_config_toml();
        let parsed: config::Config =
            toml::from_str(&printed).expect("default config output must parse");
        let parsed_table = toml::Value::try_from(&parsed).expect("serialize parsed");
        let default_table =
            toml::Value::try_from(config::Config::default()).expect("serialize default");
        let (parsed_table, default_table) = (
            parsed_table.as_table().expect("table"),
            default_table.as_table().expect("table"),
        );
        for (key, default_value) in default_table {
            assert_eq!(
                parsed_table.get(key),
                Some(default_value),
                "--default-config output drifted from Config::default() at [{key}]"
            );
        }
        assert_eq!(parsed_table.len(), default_table.len(), "extra keys");
    }

    #[test]
    fn nested_flock_blocks_when_env_is_set() {
        let config = config::Config::default();
        assert!(should_block_nested_for_env(&config, Some(FLOCK_ENV_VALUE)));
    }

    #[test]
    fn switch_failure_label_names_the_target() {
        assert_eq!(switch_failure_label(&AttachLeg::Local), "home");
        let leg = AttachLeg::Remote(remote::RemoteLaunch {
            target: "lars@sage".to_string(),
            keybindings: remote::RemoteKeybindings::Local,
            live_handoff: false,
            fleet: None,
            context: remote::LaunchContext::Cli,
        });
        assert_eq!(switch_failure_label(&leg), "lars@sage");
    }

    #[test]
    fn switch_failure_reason_is_a_single_trimmed_line() {
        let err = io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused\nIs flk server running?",
        );
        assert_eq!(switch_failure_reason(&err), "connection refused");

        let empty = io::Error::other("");
        assert_eq!(switch_failure_reason(&empty), "connection failed");
    }

    fn remote_leg(target: &str) -> AttachLeg {
        AttachLeg::Remote(remote::RemoteLaunch {
            target: target.to_string(),
            keybindings: remote::RemoteKeybindings::Local,
            live_handoff: false,
            fleet: None,
            context: remote::LaunchContext::FederationSwitch,
        })
    }

    #[test]
    fn decide_next_leg_chains_into_requested_switch() {
        let switch = Some(client::RecordedSwitch {
            target: "lars@sage".to_string(),
            fleet: None,
            focus_workspace: None,
        });
        match decide_next_leg(&AttachLeg::Local, switch, Ok(()), None, false) {
            LegStep::Switch { next, previous } => {
                assert_eq!(next, remote_leg("lars@sage"));
                // Falls back to where we came from, labeled by the target.
                assert_eq!(previous.0, AttachLeg::Local);
                assert_eq!(previous.1, "lars@sage");
                // The switch leg MUST be non-interactive: the previous leg
                // may still hold the alt-screen for the seamless swap, so a
                // remote install/upgrade prompt here would corrupt the
                // terminal (#115). Verify the constructed leg's context.
                match &next {
                    AttachLeg::Remote(launch) => assert_eq!(
                        launch.context,
                        remote::LaunchContext::FederationSwitch,
                        "switch leg must run with non-interactive context"
                    ),
                    _ => panic!("switch leg must be Remote"),
                }
            }
            _ => panic!("expected Switch"),
        }
    }

    #[test]
    fn cli_remote_leg_uses_interactive_context() {
        // The explicit `flk --remote <target>` path retains its install
        // / upgrade prompt -- the user typed the command at a shell, has a
        // real TTY, and no alt-screen is held. Verify the CLI parser
        // produces an interactive RemoteLaunch.
        let args = vec![
            "flock".to_string(),
            "--remote".to_string(),
            "lars@sage".to_string(),
        ];
        let (_cleaned, remote) =
            remote::extract_remote_args(&args).expect("--remote parses cleanly");
        let remote = remote.expect("--remote produces a RemoteLaunch");
        assert_eq!(remote.context, remote::LaunchContext::Cli);
        assert!(remote.context.allows_install_prompt());
    }

    #[test]
    fn decide_next_leg_falls_back_with_notice_on_failed_switch() {
        // The switch leg (sage) died before its client attached: no switch
        // recorded, an error, and a previous leg to bounce back to.
        let err = io::Error::other("connection refused\nis flock running?");
        let previous = Some((AttachLeg::Local, "lars@sage".to_string()));
        match decide_next_leg(&remote_leg("lars@sage"), None, Err(err), previous, true) {
            LegStep::FallBack { to, notice, .. } => {
                assert_eq!(to, AttachLeg::Local);
                assert_eq!(notice, "switch to lars@sage failed: connection refused");
            }
            _ => panic!("expected FallBack"),
        }
    }

    #[test]
    fn decide_next_leg_clean_exit_finishes_without_restore() {
        match decide_next_leg(&AttachLeg::Local, None, Ok(()), None, false) {
            LegStep::Finish {
                result,
                restore_terminal,
            } => {
                assert!(result.is_ok());
                assert!(!restore_terminal);
            }
            _ => panic!("expected Finish"),
        }
    }

    #[test]
    fn decide_next_leg_failed_initial_leg_finishes_without_restore() {
        // First leg failed to launch, no switch ever fired: plain error, no
        // held alt-screen to reclaim.
        let err = io::Error::other("no server");
        match decide_next_leg(&AttachLeg::Local, None, Err(err), None, false) {
            LegStep::Finish {
                result,
                restore_terminal,
            } => {
                assert!(result.is_err());
                assert!(!restore_terminal, "nothing was held; no restore");
            }
            _ => panic!("expected Finish"),
        }
    }

    #[test]
    fn decide_next_leg_failed_fallback_restores_terminal() {
        // The fall-back leg ALSO failed: `previous` was already consumed
        // (None), but a switch fired earlier (handoff_held), so the chain must
        // reclaim the alt-screen instead of stranding the user.
        let err = io::Error::other("still unreachable");
        match decide_next_leg(&AttachLeg::Local, None, Err(err), None, true) {
            LegStep::Finish {
                result,
                restore_terminal,
            } => {
                assert!(result.is_err());
                assert!(restore_terminal, "a held alt-screen must be reclaimed");
            }
            _ => panic!("expected Finish"),
        }
    }

    #[test]
    fn nested_flock_does_not_block_when_allowed() {
        let config: config::Config =
            toml::from_str("[experimental]\nallow_nested = true\n").unwrap();
        assert!(!should_block_nested_for_env(&config, Some(FLOCK_ENV_VALUE)));
    }

    #[test]
    fn nested_flock_does_not_block_without_env() {
        let config = config::Config::default();
        assert!(!should_block_nested_for_env(&config, None));
    }

    #[test]
    fn random_nested_message_comes_from_known_set() {
        let message = random_nested_message();
        assert!(NESTED_FLOCK_MESSAGES.contains(&message));
    }

    #[test]
    fn nested_message_strings_no_longer_repeat_flock_prefix() {
        assert!(NESTED_FLOCK_MESSAGES
            .iter()
            .all(|message| !message.starts_with("flock:") && !message.starts_with("flk:")));
    }
}
