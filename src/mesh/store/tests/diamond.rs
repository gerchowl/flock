//! Fixed-clock replay of the diamond's independent withdrawal and offer steps.
use super::*;
use crate::mesh::{
    collect::OutboundCollect,
    identity::NodeIdentity,
    routes::{Adjacency, Advert, RouteTable},
};
use std::collections::BTreeMap;

fn advert(node: &NodeIdentity, sequence: u32, neighbors: &[&NodeIdentity]) -> Advert {
    Advert::signed(
        node,
        "fixture.example".into(),
        (1, sequence),
        neighbors
            .iter()
            .map(|neighbor| Adjacency {
                node_id: neighbor.node_id(),
                name: "fixture.example".into(),
            })
            .collect(),
    )
}

fn routes(table: &RouteTable) -> BTreeMap<String, String> {
    table
        .routes()
        .into_iter()
        .map(|route| (route.node, route.next_hop))
        .collect()
}

fn attempts(store: &Store<Disk>, key: &MessageKey) -> i64 {
    store
        .connection
        .query_row(
            "SELECT collect_attempts FROM envelopes WHERE origin=?1 AND id=?2",
            params![key.origin_node, key.message_id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn diamond_reassignment_preserves_prior_offer_count_when_stale_hop_is_rejected() {
    let origin = NodeIdentity::fixture([41; 32]);
    let spoke = NodeIdentity::fixture([42; 32]);
    let mut hubs = [
        NodeIdentity::fixture([43; 32]),
        NodeIdentity::fixture([44; 32]),
    ];
    hubs.sort_by_key(NodeIdentity::node_id);
    let [first, second] = &hubs;
    let first_id = first.node_id();
    let second_id = second.node_id();
    let origin_id = origin.node_id();
    let spoke_id = spoke.node_id();
    let a = advert(&origin, 1, &[first, second]);
    let b = advert(first, 1, &[&origin, &spoke]);
    let c = advert(second, 1, &[&origin, &spoke]);
    let mut table = RouteTable::default();
    table.set_own(advert(&spoke, 1, &[first, second]));
    table.learn(&first_id, [a.clone(), b.clone(), c.clone()]);
    table.learn(&second_id, [a.clone(), b, c.clone()]);
    assert_eq!(table.next_hop(&origin_id), Some(first_id.clone()));

    let fixture = Fixture::new();
    let mut store = fixture.open(0);
    store.set_local_node(&spoke_id);
    let mut reply = envelope();
    reply.key.origin_node = spoke_id.clone();
    reply.return_binding.request = reply.key.clone();
    reply.return_binding.recipient_node = origin_id.clone();
    reply.request_key = Some(MessageKey::mint(origin_id.clone(), 0).unwrap());
    crate::mesh::sign::seal(&mut reply, &spoke);
    store
        .accept_origin(&reply, &first_id, Admission::Held, 8, 0)
        .unwrap();
    let query = OutboundCollect::default();

    // Gating the first collector leaves the second free to poll. It cannot
    // lease this row while the actual assignment and route still name first.
    assert!(store
        .collect_outbound(
            Offer::All,
            &spoke_id,
            &second_id,
            &query,
            &routes(&table),
            0
        )
        .unwrap()
        .is_empty());
    assert_eq!(store.get(&reply.key).unwrap().unwrap().next_hop, first_id);
    assert_eq!(attempts(&store, &reply.key), 0);

    // First's withdrawal arrives before second's. The still-current signed
    // adverts at the spoke offer a reciprocal path through second to origin.
    let withdrawn_b = advert(first, 2, &[&spoke]);
    table.learn(&first_id, [a.clone(), withdrawn_b.clone(), c]);
    assert_eq!(table.next_hop(&origin_id), Some(second_id.clone()));
    let current = routes(&table);
    // App::retry_mesh_mail uses these store operations in this order.
    store.withdraw_request_hops(&current).unwrap();
    assert!(store.get(&reply.key).unwrap().unwrap().next_hop.is_empty());
    let (keys, more) = store
        .routable_requests(&current.keys().cloned().collect::<Vec<_>>(), None, 256)
        .unwrap();
    assert_eq!(keys, vec![reply.key.clone()]);
    assert!(!more);
    store
        .route_custody(&keys[0], &current[&origin_id], Admission::Held)
        .unwrap();
    assert_eq!(store.get(&reply.key).unwrap().unwrap().next_hop, second_id);
    assert!(store
        .collect_outbound(Offer::All, &spoke_id, &first_id, &query, &current, 0)
        .unwrap()
        .is_empty());
    let offered = store
        .collect_outbound(Offer::All, &spoke_id, &second_id, &query, &current, 0)
        .unwrap();
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].envelope, reply);
    assert_eq!(attempts(&store, &reply.key), 1);
    // No acknowledgement is sent: an offer is not durable downstream custody.
    let held = store.get(&reply.key).unwrap().unwrap();
    assert_eq!(held.state, "held");
    assert_eq!(held.envelope.body, reply.body);

    // The delayed withdrawal removes the last route. The routing pass clears
    // second's assignment, but neither that pass nor lease rejection erases history.
    table.learn(&second_id, [a, withdrawn_b, advert(second, 2, &[&spoke])]);
    assert_eq!(table.next_hop(&origin_id), None);
    let current = routes(&table);
    store.withdraw_request_hops(&current).unwrap();
    assert!(store.get(&reply.key).unwrap().unwrap().next_hop.is_empty());
    let generation = table.generation();
    let before_stale_poll = attempts(&store, &reply.key);
    assert_eq!(before_stale_poll, 1);
    store.connection.execute(
        "UPDATE envelopes SET next_hop=?1,retry_at=0 WHERE origin=?2 AND id=?3 AND state='held' AND next_hop=''",
        params![first_id,reply.key.origin_node,reply.key.message_id],
    ).unwrap();
    assert!(store
        .collect_outbound(Offer::All, &spoke_id, &first_id, &query, &current, 0)
        .unwrap()
        .is_empty());
    assert!(store.get(&reply.key).unwrap().unwrap().next_hop.is_empty());
    assert_eq!(table.generation(), generation);
    assert_eq!(
        attempts(&store, &reply.key),
        before_stale_poll,
        "lease-time rejection adds no offer and preserves earlier history"
    );
}
