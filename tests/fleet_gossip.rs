//! E2E: what a server can SEE of the fleet must not depend on which server you
//! happen to be attached to.
//!
//! Topology is a chain — `nodea` polls `nodeb`, `nodeb` polls `nodec` — driven
//! by real `flk` servers over a dispatching fake-`ssh` shim (see
//! `support::fleet`). `nodec` reaches `nodea` only by relay, which is exactly
//! the case the sidebar used to drop on the floor: gossip v3 carries relayed
//! peers into `relayed_fleet_cache` and forwards them on the outgoing snapshot,
//! but the local render never read them back.

// Integration tests exec real ssh/git/hostname invocations to set up their
// fake fleet — the TracedCommand funnel doesn't apply to test scaffolding.
#![allow(clippy::disallowed_methods)]

mod support;

use std::time::Duration;

use support::fleet;

/// The two-hop peer must render on the hub. `nodec` is known to `nodea` only
/// through `nodeb`'s relay — if the sidebar only reads locally-polled peers and
/// the carried snapshot, `nodea` sees a strictly smaller fleet than `nodeb`
/// does, and "the fleet" means something different on every machine.
#[test]
fn relayed_two_hop_peer_renders_on_the_hub() {
    let fleet = fleet::spawn("gossip-chain", fleet::CHAIN_ABC);
    // Tall enough that the servers band can show the whole chain — a short
    // band would hide the row for reasons that have nothing to do with gossip.
    let mut stream = fleet.node("nodea").attach_sized(120, 50);

    // Sanity: the one-hop peer folds in as it always has. Asserting both in
    // ONE frame keeps the two-hop claim honest — it can't pass on a frame
    // rendered before the fleet converged.
    let rows =
        fleet::wait_for_screen_matching(&mut stream, &["nodeb", "nodec"], Duration::from_secs(30))
            .expect("nodea should see the whole chain, including the relayed two-hop peer");

    let screen = rows.join("\n");
    assert!(
        screen.contains("nodec"),
        "two-hop peer missing from nodea's sidebar:\n{screen}"
    );
}

/// Selecting another server's SPACE must carry that space through the switch.
/// It used to be delivered out of band — a fire-and-forget
/// `ssh <peer> flk workspace focus` racing the attach — so you arrived on
/// whatever space that machine was last looking at.
#[test]
fn selecting_a_remote_space_carries_it_through_the_switch() {
    let fleet = fleet::spawn("switch-focus", fleet::CHAIN_ABC);
    let node_b = fleet.node("nodeb");
    // A second space on nodeb, focused: whatever we click must beat it.
    let other = fleet.base.join("beta-second");
    std::fs::create_dir_all(&other).unwrap();
    node_b.create_workspace(&other);

    let mut stream = fleet.node("nodea").attach_sized(120, 50);
    let beta = fleet::Fleet::project_needle("beta");
    // The local space grows its branch row ("nodea:main") once git info
    // resolves, which pushes every row below it down. Clicking the row index
    // from a frame drawn before that lands on a spacer, so wait for the layout
    // to be complete and then to stop moving.
    let row = fleet::wait_for_settled_row(
        &mut stream,
        &["nodea:main", &beta],
        &beta,
        Duration::from_secs(30),
    )
    .expect("nodeb's space should fold into nodea's sidebar");

    fleet::click_row(&mut stream, row, 3);
    let (target, tail) = fleet::wait_for_switch_server(&mut stream, Duration::from_secs(10))
        .expect("clicking a remote space should yield SwitchServer");
    assert_eq!(target, "nodeb");

    // The space that was CLICKED, not merely some space: nodeb is focused on
    // its second workspace, so a switch that carried the peer's own focus (or
    // a hardcoded first row) would look identical without this.
    let beta_id = fleet::workspace_id_by_label(node_b, "beta");
    assert_eq!(
        fleet::switch_focus_workspace(&tail).as_deref(),
        Some(beta_id.as_str()),
        "the switch must name the space that was clicked, not just the server"
    );
}

/// The receiving end: a client that arrives carrying a focus target lands on
/// that space. Drives the real `FocusWorkspace` message against a real server
/// and reads the answer back off the JSON API.
#[test]
fn focus_workspace_message_lands_the_arriving_client_on_that_space() {
    let fleet = fleet::spawn(
        "switch-focus-apply",
        &[fleet::NodeSpec::new("solo", "alpha", &[])],
    );
    let node = fleet.node("solo");

    // Two spaces; the second is focused because creating it focuses it.
    let second = fleet.base.join("alpha-second");
    std::fs::create_dir_all(&second).unwrap();
    node.create_workspace(&second);

    let first = fleet::workspace_ids(node)
        .into_iter()
        .next()
        .expect("the server should have workspaces");
    assert!(
        !fleet::workspace_focused(node, &first),
        "the newly created second space should hold focus before we ask"
    );

    let mut stream = node.attach();
    fleet::send_focus_workspace(&mut stream, &first);

    assert!(
        fleet::wait_until_focused(node, &first, Duration::from_secs(5)),
        "FocusWorkspace should land the client on the space the switch named"
    );
}

/// Restart uses the same durable directories, and the isolated shell sees
/// the sandbox rather than the developer's HOME or XDG state.
#[test]
fn mesh_harness_restart_preserves_state_and_sandboxes_remote_commands() {
    let mut fleet = fleet::spawn("mesh-restart", fleet::LAPTOP_HUB_SPOKE);
    let node = fleet.node("nodec");
    let before = fleet::workspace_ids(node);
    let marker = node.home.join("state/retained");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "retained").unwrap();
    let output = shim_command(
        &fleet,
        "nodea",
        "nodec",
        "printf '%s\\n' \"$HOME\" \"$XDG_STATE_HOME\" \"$FLOCK_SOCKET_PATH\"",
    )
    .output()
    .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{}\n{}\n{}\n",
            node.home.display(),
            node.home.join("state").display(),
            node.api_socket.display()
        )
    );
    fleet.node_mut("nodec").restart();
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "retained");
    assert_eq!(fleet::workspace_ids(fleet.node("nodec")), before);
}

fn shim_command(
    fleet: &fleet::Fleet,
    from: &str,
    to: &str,
    command: &str,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(fleet.base.join("bin/ssh"));
    cmd.env("FLOCK_FLEET_SOURCE", from).args([to, command]);
    cmd
}

/// Cut an actual held relay while a reverse edge remains usable. The marker
/// prevents reconnects until the test explicitly restores this direction.
#[test]
fn mesh_harness_kills_a_held_edge_and_restores_only_that_direction() {
    let fleet = fleet::spawn("mesh-cut", fleet::HUB_SPOKES);
    fleet.wait_for_edge("nodeb", "nodec", Duration::from_secs(30));
    fleet.refuse_edge("nodeb", "nodec");
    assert!(fleet.kill_edge("nodeb", "nodec", Duration::from_secs(30)) > 0);
    assert_eq!(
        shim_command(&fleet, "nodeb", "nodec", "exit 0")
            .status()
            .unwrap()
            .code(),
        Some(255)
    );
    assert!(shim_command(&fleet, "nodec", "nodeb", "exit 0")
        .status()
        .unwrap()
        .success());
    assert!(shim_command(&fleet, "nodeb", "nodea", "exit 0")
        .status()
        .unwrap()
        .success());
    fleet.allow_edge("nodeb", "nodec");
    // Open a fresh held edge directly, avoiding the production reconnect
    // backoff without changing its timing constants for the sake of the test.
    let reply = relay_probe(&fleet, "nodeb", "nodec", "ping");
    assert!(reply.get("result").is_some(), "{reply}");
}

fn relay_probe(fleet: &fleet::Fleet, from: &str, to: &str, method: &str) -> serde_json::Value {
    use std::io::{BufRead, Write};
    use std::process::Stdio;
    let mut child = shim_command(fleet, from, to, "flk peers relay")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"id":"probe", "method":method, "params":{}})
    )
    .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let line = std::io::BufReader::new(stdout)
            .lines()
            .next()
            .unwrap()
            .unwrap();
        let _ = tx.send(line);
    });
    let response = rx.recv_timeout(Duration::from_secs(10));
    drop(stdin);
    fleet::wait_until("relay exit", Duration::from_secs(10), || {
        child.try_wait().unwrap()
    });
    serde_json::from_str(&response.expect("relay probe deadline")).unwrap()
}

/// Fault modes refuse mesh without preventing legacy traffic from reaching
/// the real server. These are transport fixtures, not enrollment assertions.
#[test]
fn mesh_harness_legacy_and_version_mismatch_keep_ping_working() {
    use fleet::{MeshMode, NodeSpec};
    let fleet = fleet::spawn(
        "mesh-mixed",
        &[
            NodeSpec::new("nodea", "alpha", &[]).with_mesh(MeshMode::Disabled),
            NodeSpec::new("nodeb", "beta", &[]).with_mesh(MeshMode::VersionMismatch(999)),
        ],
    );
    for (node, code) in [
        ("nodea", "invalid_request"),
        ("nodeb", "mesh_version_mismatch"),
    ] {
        let reply = relay_probe(&fleet, "probe", node, "mesh.hello");
        assert_eq!(reply["id"], "probe");
        assert_eq!(reply["error"]["code"], code);
        let ping = relay_probe(&fleet, "probe", node, "ping");
        assert!(ping.get("result").is_some(), "{ping}");
    }
}
