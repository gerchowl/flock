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
    let row = fleet::wait_for_row(&mut stream, &beta, Duration::from_secs(30))
        .expect("nodeb's space should fold into nodea's sidebar");

    fleet::click_row(&mut stream, row, 3);
    let (target, tail) = fleet::wait_for_switch_server(&mut stream, Duration::from_secs(10))
        .expect("clicking a remote space should yield SwitchServer");
    assert_eq!(target, "nodeb");

    let focus = fleet::switch_focus_workspace(&tail);
    assert!(
        focus.is_some(),
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
