//! Signed topology learned from live enrolled edges. Silence is not withdrawal.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_RECORDS: usize = 256;
pub const MAX_ADJACENCIES: usize = 64;
pub const MAX_HOPS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adjacency {
    pub node_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advert {
    pub node_id: String,
    pub public_key: [u8; 32],
    pub name: String,
    pub mesh: u32,
    pub generation: (u64, u32),
    pub adjacencies: Vec<Adjacency>,
    pub signature: Vec<u8>,
}

pub fn decode_adverts(values: Vec<serde_json::Value>) -> Vec<Advert> {
    values
        .into_iter()
        .take(MAX_RECORDS)
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(advert) => Some(advert),
            Err(reason) => {
                crate::logging::mesh_routing_failed("decode_advert", "", &reason.to_string());
                None
            }
        })
        .collect()
}

fn node_id(key: &[u8; 32]) -> String {
    Sha256::digest(key)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control)
}

impl Advert {
    fn canonical(&self) -> Vec<u8> {
        serde_json::to_vec(&(
            "flock-mesh-advert-v1",
            &self.node_id,
            self.public_key,
            &self.name,
            self.mesh,
            self.generation,
            &self.adjacencies,
        ))
        .expect("advert tuple is JSON serializable")
    }

    pub(crate) fn signed(
        identity: &super::identity::NodeIdentity,
        name: String,
        generation: (u64, u32),
        adjacencies: Vec<Adjacency>,
    ) -> Self {
        let mut advert = Self {
            node_id: identity.node_id(),
            public_key: identity.public_key(),
            name,
            mesh: super::hello::version(),
            generation,
            adjacencies,
            signature: Vec::new(),
        };
        advert.signature = identity.sign(&advert.canonical());
        advert
    }

    pub fn verify(&self) -> Result<(), &'static str> {
        if self.node_id != node_id(&self.public_key) {
            return Err("advert key does not match node id");
        }
        // The signed v1 advert format is independent of the edge protocol version.
        if !valid_name(&self.name)
            || self.adjacencies.len() > MAX_ADJACENCIES
            || self.adjacencies.iter().any(|a| {
                !valid_name(&a.name)
                    || a.node_id.len() != 64
                    || !a.node_id.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err("invalid advert fields or limits");
        }
        let key = VerifyingKey::from_bytes(&self.public_key).map_err(|_| "invalid advert key")?;
        let sig = Signature::from_slice(&self.signature).map_err(|_| "invalid advert signature")?;
        key.verify_strict(&self.canonical(), &sig)
            .map_err(|_| "invalid advert signature")
    }
}

#[derive(Clone, Debug)]
struct Record {
    advert: Advert,
    suppliers: BTreeSet<String>,
}

#[derive(Default)]
pub struct RouteTable {
    own: Option<Advert>,
    records: BTreeMap<String, Record>,
    live_edges: BTreeSet<String>,
    generation: u64,
    cap_warned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub node: String,
    pub name: String,
    pub next_hop: String,
    pub hops: usize,
}

impl RouteTable {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn own(&self) -> Option<&Advert> {
        self.own.as_ref()
    }

    pub fn set_live_edges(&mut self, edges: BTreeSet<String>) {
        if self.live_edges != edges {
            self.live_edges = edges;
            self.generation = self.generation.wrapping_add(1);
        }
    }

    pub fn set_own(&mut self, advert: Advert) {
        self.set_live_edges(
            advert
                .adjacencies
                .iter()
                .map(|a| a.node_id.clone())
                .collect(),
        );
        if self.own.as_ref() != Some(&advert) {
            self.records.remove(&advert.node_id);
            self.own = Some(advert);
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// A batch is the supplier's complete bounded view. Invalid records never prevent valid siblings
    /// being learned, and a neighbor cannot replace another node's signature.
    pub fn learn(&mut self, supplier_edge: &str, adverts: impl IntoIterator<Item = Advert>) {
        let adverts: Vec<_> = adverts
            .into_iter()
            .filter(|advert| {
                if let Err(reason) = advert.verify() {
                    crate::logging::mesh_routing_failed("verify_advert", supplier_edge, reason);
                    false
                } else {
                    true
                }
            })
            .collect();
        let listed: BTreeSet<_> = adverts
            .iter()
            .map(|advert| advert.node_id.as_str())
            .collect();
        let mut changed = false;
        self.records.retain(|node, record| {
            if !listed.contains(node.as_str()) {
                changed |= record.suppliers.remove(supplier_edge);
            }
            !record.suppliers.is_empty()
        });
        if changed {
            self.invalidate();
        }
        for advert in adverts {
            if self
                .own
                .as_ref()
                .is_some_and(|own| own.node_id == advert.node_id)
            {
                continue;
            }
            if let Some(record) = self.records.get_mut(&advert.node_id) {
                if advert.generation < record.advert.generation {
                    continue;
                }
                if advert.generation == record.advert.generation && advert != record.advert {
                    crate::logging::mesh_routing_failed(
                        "learn_advert",
                        &advert.node_id,
                        "conflicting generation",
                    );
                    continue;
                }
                let mut changed = record.suppliers.insert(supplier_edge.into());
                if advert.generation > record.advert.generation {
                    record.advert = advert;
                    // Only the new generation's actual suppliers can retain it.
                    record.suppliers = BTreeSet::from([supplier_edge.into()]);
                    changed = true;
                }
                if changed {
                    self.generation = self.generation.wrapping_add(1);
                }
            } else if self.records.len() < MAX_RECORDS - 1 {
                self.records.insert(
                    advert.node_id.clone(),
                    Record {
                        advert,
                        suppliers: BTreeSet::from([supplier_edge.into()]),
                    },
                );
                self.generation = self.generation.wrapping_add(1);
            } else if !self.cap_warned {
                self.cap_warned = true;
                crate::logging::mesh_routing_failed(
                    "learn_advert",
                    supplier_edge,
                    "route table cap reached; additional adverts dropped",
                );
            }
        }
    }

    pub fn withdraw_edge(&mut self, edge: &str) {
        let mut changed = self.live_edges.remove(edge);
        self.records.retain(|_, record| {
            changed |= record.suppliers.remove(edge);
            !record.suppliers.is_empty()
        });
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Split horizon prevents a neighbor from becoming its own indirect supplier.
    pub fn adverts_for_peer(&self, peer: &str) -> Vec<Advert> {
        self.own
            .iter()
            .cloned()
            .chain(
                self.records
                    .values()
                    .filter(|r| r.suppliers.len() != 1 || !r.suppliers.contains(peer))
                    .map(|r| r.advert.clone()),
            )
            .take(MAX_RECORDS)
            .collect()
    }

    pub fn next_hop(&self, target: &str) -> Option<String> {
        self.routes()
            .into_iter()
            .find(|r| r.node == target)
            .map(|r| r.next_hop)
    }

    /// Sorted neighbors make BFS select the lowest node id among equal paths.
    /// Our own adjacency is rebuilt exclusively from live enrolled edges.
    pub fn routes(&self) -> Vec<Route> {
        let Some(own) = &self.own else {
            return Vec::new();
        };
        let get = |id: &str| {
            if id == own.node_id {
                Some(own)
            } else {
                self.records.get(id).map(|r| &r.advert)
            }
        };
        let mut seen = BTreeSet::from([own.node_id.clone()]);
        let mut pending = VecDeque::from([(own.node_id.clone(), String::new(), 0)]);
        let mut routes = Vec::new();
        while let Some((node, first, hops)) = pending.pop_front() {
            if hops == MAX_HOPS {
                continue;
            }
            let Some(advert) = get(&node) else { continue };
            let neighbors: BTreeSet<_> = advert.adjacencies.iter().map(|a| &a.node_id).collect();
            for neighbor in neighbors {
                if (hops == 0 && !self.live_edges.contains(neighbor)) || seen.contains(neighbor) {
                    continue;
                }
                let Some(other) = get(neighbor) else { continue };
                if !other.adjacencies.iter().any(|a| a.node_id == node) {
                    continue;
                }
                seen.insert(neighbor.clone());
                let next = if hops == 0 {
                    neighbor.clone()
                } else {
                    first.clone()
                };
                routes.push(Route {
                    node: neighbor.clone(),
                    name: other.name.clone(),
                    next_hop: next.clone(),
                    hops: hops + 1,
                });
                pending.push_back((neighbor.clone(), next, hops + 1));
            }
        }
        routes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn advert(seed: u16, neighbors: &[u16]) -> Advert {
        let mut bytes = [0; 32];
        bytes[..2].copy_from_slice(&seed.to_le_bytes());
        let key = SigningKey::from_bytes(&bytes);
        let mut a = Advert {
            node_id: node_id(&key.verifying_key().to_bytes()),
            public_key: key.verifying_key().to_bytes(),
            name: format!("node-{seed}.test"),
            mesh: super::super::hello::version(),
            generation: (1, 1),
            adjacencies: neighbors
                .iter()
                .map(|n| Adjacency {
                    node_id: advert(*n, &[]).node_id,
                    name: format!("node-{n}.test"),
                })
                .collect(),
            signature: Vec::new(),
        };
        a.signature = key.sign(&a.canonical()).to_bytes().to_vec();
        a
    }

    fn table(own: Advert, others: Vec<Advert>) -> RouteTable {
        let mut table = RouteTable::default();
        table.set_own(own);
        table.learn("edge", others);
        table
    }

    #[test]
    fn bidirectional_exchange_does_not_retain_a_closed_edge_record() {
        let x = advert(3, &[2]);
        let b_advert = advert(2, &[1, 3]);
        let a_advert = advert(1, &[2]);
        for reverse_order in [false, true] {
            let mut a = RouteTable::default();
            let mut b = RouteTable::default();
            a.set_own(a_advert.clone());
            b.set_own(b_advert.clone());
            b.learn(&x.node_id, vec![x.clone()]);
            // Exchange in both directions repeatedly, allowing the old echo loop
            // to form if either side forgets split horizon.
            for _ in 0..3 {
                a.learn(&b_advert.node_id, b.adverts_for_peer(&a_advert.node_id));
                b.learn(&a_advert.node_id, a.adverts_for_peer(&b_advert.node_id));
            }
            assert!(a.records.contains_key(&x.node_id));
            assert!(b.records.contains_key(&x.node_id));
            b.withdraw_edge(&x.node_id);
            // One round must remove X regardless of which direction runs first.
            if reverse_order {
                b.learn(&a_advert.node_id, a.adverts_for_peer(&b_advert.node_id));
                a.learn(&b_advert.node_id, b.adverts_for_peer(&a_advert.node_id));
            } else {
                a.learn(&b_advert.node_id, b.adverts_for_peer(&a_advert.node_id));
                b.learn(&a_advert.node_id, a.adverts_for_peer(&b_advert.node_id));
            }
            assert!(!a.records.contains_key(&x.node_id));
            assert!(!b.records.contains_key(&x.node_id));
        }
    }

    #[test]
    fn split_horizon_preserves_own_and_independently_supplied_records() {
        let own = advert(1, &[2]);
        let remote = advert(3, &[2]);
        let mut t = RouteTable::default();
        t.set_own(own.clone());
        t.learn("peer", vec![remote.clone()]);
        assert_eq!(t.adverts_for_peer("peer"), vec![own.clone()]);
        t.learn("independent", vec![remote.clone()]);
        assert_eq!(t.adverts_for_peer("peer"), vec![own, remote]);
    }

    #[test]
    fn full_view_withdraws_omitted_records_only_for_that_supplier() {
        let mut t = RouteTable::default();
        t.learn("one", vec![advert(2, &[]), advert(3, &[])]);
        t.learn("two", vec![advert(3, &[])]);
        t.learn("one", vec![]);
        assert_eq!(t.records.len(), 1);
        t.learn("two", vec![]);
        assert!(t.records.is_empty());
    }

    #[test]
    fn unchanged_advert_format_accepts_another_mesh_version() {
        let mut a = advert(2, &[]);
        a.mesh += 1;
        let mut bytes = [0; 32];
        bytes[..2].copy_from_slice(&2_u16.to_le_bytes());
        a.signature = SigningKey::from_bytes(&bytes)
            .sign(&a.canonical())
            .to_bytes()
            .to_vec();
        assert!(a.verify().is_ok());
    }

    #[test]
    fn shortest_path_with_node_id_tie_break() {
        let a = advert(1, &[2, 3]);
        let b = advert(2, &[1, 4]);
        let c = advert(3, &[1, 4]);
        let d = advert(4, &[2, 3]);
        let expected = b.node_id.clone().min(c.node_id.clone());
        let t = table(a, vec![c, d.clone(), b]);
        assert_eq!(t.next_hop(&d.node_id), Some(expected));
        assert_eq!(
            t.routes()
                .iter()
                .find(|r| r.node == d.node_id)
                .unwrap()
                .hops,
            2
        );
    }

    #[test]
    fn one_sided_adjacency_is_not_a_route() {
        let b = advert(2, &[]);
        let t = table(advert(1, &[2]), vec![b.clone()]);
        assert_eq!(t.next_hop(&b.node_id), None);
    }

    #[test]
    fn advert_with_bad_signature_or_key_id_mismatch_is_dropped_alone() {
        let good = advert(2, &[1]);
        let mut bad = advert(3, &[1]);
        bad.signature[0] ^= 1;
        let mut mismatch = advert(4, &[1]);
        mismatch.node_id = good.node_id.clone();
        let t = table(advert(1, &[2, 3, 4]), vec![bad, good.clone(), mismatch]);
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.next_hop(&good.node_id), Some(good.node_id));
    }

    #[test]
    fn advert_signed_by_a_claiming_b_is_rejected() {
        let mut claim = advert(2, &[1]);
        let key = SigningKey::from_bytes(&[19; 32]);
        claim.signature = key.sign(&claim.canonical()).to_bytes().to_vec();
        assert!(claim.verify().is_err());
    }

    #[test]
    fn withdraw_edge_removes_only_routes_it_alone_supplied() {
        let mut t = RouteTable::default();
        t.learn("one", vec![advert(2, &[]), advert(3, &[])]);
        t.learn("two", vec![advert(3, &[])]);
        let generation = t.generation();
        t.withdraw_edge("one");
        assert_eq!(t.records.len(), 1);
        assert!(t.records.contains_key(&advert(3, &[]).node_id));
        assert!(t.generation() > generation);
        t.withdraw_edge("two");
        assert!(t.records.is_empty());
    }

    #[test]
    fn retained_adverts_cannot_route_over_a_closed_first_hop() {
        let b = advert(2, &[1]);
        let mut t = table(advert(1, &[2]), vec![b.clone()]);
        t.learn(&b.node_id, vec![b.clone()]);
        assert!(t.next_hop(&b.node_id).is_some());
        t.withdraw_edge(&b.node_id);
        assert!(t.records.contains_key(&b.node_id));
        assert!(t.next_hop(&b.node_id).is_none());
    }

    #[test]
    fn no_api_expires_routes_by_time() {
        let b = advert(2, &[1]);
        let mut t = table(advert(1, &[2]), vec![b.clone()]);
        let generation = t.generation();
        for _ in 0..1000 {
            assert_eq!(t.next_hop(&b.node_id), Some(b.node_id.clone()));
            t.learn("edge", vec![b.clone()]);
        }
        assert_eq!(t.generation(), generation);
    }

    #[test]
    fn caps_and_hop_limit_eight() {
        let mut t = RouteTable::default();
        t.set_own(advert(0, &[1]));
        t.learn("edge", (1..10).map(|i| advert(i, &[i - 1, i + 1])));
        assert!(t.next_hop(&advert(8, &[]).node_id).is_some());
        assert!(t.next_hop(&advert(9, &[]).node_id).is_none());
        t.learn("edge", (10..300).map(|i| advert(i, &[])));
        assert_eq!(t.adverts_for_peer("other").len(), MAX_RECORDS);
        assert!(advert(1, &(0..65).collect::<Vec<_>>()).verify().is_err());
        let mut bad = advert(1, &[]);
        bad.name = "a".repeat(256);
        assert!(bad.verify().is_err());
        bad.name = "bad\nname".into();
        assert!(bad.verify().is_err());
    }

    #[test]
    fn higher_generation_replaces_and_stale_supplier_cannot_retain_it() {
        let old = advert(2, &[1]);
        let mut new = old.clone();
        new.generation.1 += 1;
        let mut bytes = [0; 32];
        bytes[..2].copy_from_slice(&2_u16.to_le_bytes());
        new.signature = SigningKey::from_bytes(&bytes)
            .sign(&new.canonical())
            .to_bytes()
            .to_vec();
        let mut t = table(advert(1, &[2]), vec![old.clone()]);
        t.learn("new", vec![new.clone()]);
        t.learn("edge", vec![old]);
        assert_eq!(t.records[&new.node_id].advert, new);
        t.withdraw_edge("new");
        assert!(t.records.is_empty());
    }
}
