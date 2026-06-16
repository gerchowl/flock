use crate::api::schema::{EmptyParams, Method, PeersCheckoutPrepareParams, Request};

pub(super) fn run_peers_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_peers_help();
        return Ok(2);
    };

    match subcommand {
        "summary" => peers_summary(&args[1..]),
        "checkout-prepare" => peers_checkout_prepare(&args[1..]),
        "help" | "--help" | "-h" => {
            print_peers_help();
            Ok(0)
        }
        _ => {
            print_peers_help();
            Ok(2)
        }
    }
}

/// This server's federated summary (workspaces + agent statuses). Peer
/// servers run this over SSH to fold our workspaces into their sidebars.
fn peers_summary(args: &[String]) -> std::io::Result<i32> {
    for arg in args {
        match arg.as_str() {
            // Output is always JSON; the flag is accepted for symmetry with
            // the other read-only subcommands.
            "--json" => {}
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:peers:summary".into(),
        method: Method::PeersSummary(EmptyParams {}),
    })?)
}

/// Prepare one of this server's workspaces for a cross-machine checkout (#125):
/// the hub invokes this over SSH, the local server resolves the repo + branch
/// and acts on its own git, then the hub fetches the branch from origin. With
/// `--push` the branch is pushed to origin; without it this is a read-only
/// probe (dirty / unpushed state) for the hub's pre-action confirmation.
fn peers_checkout_prepare(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id: Option<String> = None;
    let mut push = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" | "--workspace-id" => {
                let Some(value) = iter.next() else {
                    eprintln!("{arg} requires a workspace id");
                    return Ok(2);
                };
                workspace_id = Some(value.clone());
            }
            "--push" => push = true,
            // Output is always JSON; the flag is accepted for symmetry.
            "--json" => {}
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let Some(workspace_id) = workspace_id else {
        eprintln!("usage: flock peers checkout-prepare --workspace <id> [--push] [--json]");
        return Ok(2);
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:peers:checkout_prepare".into(),
        method: Method::PeersCheckoutPrepare(PeersCheckoutPrepareParams { workspace_id, push }),
    })?)
}

fn print_peers_help() {
    eprintln!("usage: flock peers summary [--json]");
    eprintln!("       flock peers checkout-prepare --workspace <id> [--push] [--json]");
}
