//! Inbound mesh attachments, bound to individual relay processes.
use crate::mesh::{
    hello::{Enrollment, Pending},
    store::PinSource,
};
use std::collections::BTreeMap;

const MAX_INBOUND_EDGES: usize = 32;

pub(crate) struct InboundEdge {
    pub(crate) pending: Option<Pending>,
    pub(crate) enrollment: Enrollment,
}

impl Default for InboundEdge {
    fn default() -> Self {
        Self {
            pending: None,
            enrollment: Enrollment {
                peer: "unidentified SSH peer".into(),
                source: PinSource::Inbound,
                pin_origin: Default::default(),
                node_id: None,
                state: "pending".into(),
                reason: None,
            },
        }
    }
}

impl InboundEdge {
    pub(crate) fn enrolled(&self) -> bool {
        self.enrollment.state == "pinned" && self.enrollment.node_id.is_some()
    }

    pub(crate) fn clear(&mut self, reason: &str) {
        self.pending = None;
        self.enrollment.node_id = None;
        self.enrollment.state = "refused".into();
        self.enrollment.reason = Some(reason.into());
    }
}

#[derive(Default)]
pub(crate) struct InboundEdges {
    edges: BTreeMap<(u32, u64), InboundEdge>,
    generation: u64,
}

impl InboundEdges {
    pub(crate) fn inbound_generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn attach(
        &mut self,
        pid: u32,
        started: u64,
        start_of: impl Fn(u32) -> Option<u64>,
    ) -> Result<(), &'static str> {
        // Only this process is checked here. The one-second tick prunes the map.
        if start_of(pid) != Some(started) {
            return Err("relay_unattestable");
        }
        self.edge(Some(pid), &start_of);
        if self.edges.contains_key(&(pid, started)) {
            return Ok(());
        }
        if self.edges.len() >= MAX_INBOUND_EDGES {
            return Err("inbound_edges_full");
        }
        self.edges.insert((pid, started), InboundEdge::default());
        self.generation = self.inbound_generation().wrapping_add(1);
        Ok(())
    }

    pub(crate) fn edge(
        &mut self,
        pid: Option<u32>,
        start_of: impl Fn(u32) -> Option<u64>,
    ) -> Option<&InboundEdge> {
        self.edge_mut(pid, start_of).map(|edge| &*edge)
    }

    pub(crate) fn edge_mut(
        &mut self,
        pid: Option<u32>,
        start_of: impl Fn(u32) -> Option<u64>,
    ) -> Option<&mut InboundEdge> {
        let pid = pid?;
        let key = self
            .edges
            .range((pid, 0)..=(pid, u64::MAX))
            .next()
            .map(|(key, _)| *key)?;
        if start_of(pid) != Some(key.1) {
            self.edges.remove(&key);
            self.generation = self.inbound_generation().wrapping_add(1);
            return None;
        }
        self.edges.get_mut(&key)
    }

    pub(crate) fn prune(&mut self, start_of: impl Fn(u32) -> Option<u64>) -> Vec<String> {
        let mut removed = Vec::new();
        let before = self.edges.len();
        self.edges.retain(|&(pid, started), edge| {
            if start_of(pid) == Some(started) {
                return true;
            }
            if let Some(node) = &edge.enrollment.node_id {
                removed.push(node.clone());
            }
            false
        });
        if self.edges.len() != before {
            self.generation = self.inbound_generation().wrapping_add(1);
        }
        removed
    }

    pub(crate) fn enroll(
        &mut self,
        pid: u32,
        status: Enrollment,
        start_of: impl Fn(u32) -> Option<u64>,
    ) -> Result<(), String> {
        if self.edge(Some(pid), &start_of).is_none() {
            return Err("unattestable relay".into());
        }
        for (&(other, _), edge) in &mut self.edges {
            if other != pid && edge.enrollment.node_id == status.node_id {
                edge.clear("superseded by a newer edge for this node");
            }
        }
        let edge = self
            .edge_mut(Some(pid), start_of)
            .ok_or("unattestable relay")?;
        edge.enrollment = status;
        self.generation = self.inbound_generation().wrapping_add(1);
        Ok(())
    }

    pub(crate) fn reset(&mut self, peer: &str, node: Option<&str>) {
        for edge in self.edges.values_mut() {
            if edge.enrollment.peer == peer
                || node.is_some_and(|node| edge.enrollment.node_id.as_deref() == Some(node))
            {
                edge.clear("enrollment reset; repeat mesh.hello");
                self.generation = self.generation.wrapping_add(1);
            }
        }
    }

    pub(crate) fn live(
        &self,
        start_of: impl Fn(u32) -> Option<u64>,
    ) -> impl Iterator<Item = &InboundEdge> {
        self.edges
            .iter()
            .filter_map(move |(&(pid, started), edge)| {
                (start_of(pid) == Some(started)).then_some(edge)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(pid: u32) -> Option<u64> {
        Some(u64::from(pid) + 10)
    }
    fn status(peer: &str, node: &str) -> Enrollment {
        Enrollment {
            peer: peer.into(),
            node_id: Some(node.into()),
            state: "pinned".into(),
            ..InboundEdge::default().enrollment
        }
    }

    #[test]
    fn two_live_relays_attach_concurrently() {
        let mut edges = InboundEdges::default();
        assert_eq!(edges.attach(1, 11, start), Ok(()));
        assert_eq!(edges.attach(2, 12, start), Ok(()));
        assert!(edges.edge(Some(1), start).is_some());
        assert!(edges.edge(Some(2), start).is_some());
        assert!(edges.edge(None, start).is_none());
    }

    #[test]
    fn dead_relay_is_pruned_and_generation_bumps() {
        let mut edges = InboundEdges::default();
        edges.attach(1, 11, start).unwrap();
        edges.attach(2, 12, start).unwrap();
        edges.enroll(1, status("a.test", "node-a"), start).unwrap();
        edges.enroll(2, status("b.test", "node-b"), start).unwrap();
        let generation = edges.inbound_generation();
        assert_eq!(edges.prune(|pid| (pid == 2).then_some(12)), vec!["node-a"]);
        assert_eq!(edges.inbound_generation(), generation + 1);
        assert!(edges.edge(Some(2), start).unwrap().enrolled());
        assert!(edges.prune(start).is_empty());
        assert_eq!(edges.inbound_generation(), generation + 1);
    }

    #[test]
    fn pid_reuser_inherits_no_edge() {
        let mut edges = InboundEdges::default();
        edges.attach(1, 11, start).unwrap();
        edges.enroll(1, status("a.test", "node-a"), start).unwrap();
        let generation = edges.inbound_generation();
        assert!(edges.edge(Some(1), |_| Some(99)).is_none());
        assert_eq!(edges.inbound_generation(), generation + 1);
        edges.attach(1, 99, |_| Some(99)).unwrap();
        assert!(!edges.edge(Some(1), |_| Some(99)).unwrap().enrolled());
    }

    #[test]
    fn edge_name_is_fixed_by_its_own_handshake() {
        let mut edges = InboundEdges::default();
        edges.attach(1, 11, start).unwrap();
        edges.attach(2, 12, start).unwrap();
        edges.enroll(1, status("a.test", "node-a"), start).unwrap();
        assert!(!edges.edge(Some(2), start).unwrap().enrolled());
        edges.enroll(2, status("b.test", "node-b"), start).unwrap();
        edges.attach(1, 11, start).unwrap();
        assert_eq!(
            edges.edge(Some(1), start).unwrap().enrollment.peer,
            "a.test"
        );
        assert_eq!(
            edges.edge(Some(2), start).unwrap().enrollment.peer,
            "b.test"
        );
        edges.reset("b.test", Some("node-b"));
        assert!(edges.edge(Some(1), start).unwrap().enrolled());
        assert!(!edges.edge(Some(2), start).unwrap().enrolled());
    }

    #[test]
    fn newer_edge_for_same_node_supersedes_older() {
        let mut edges = InboundEdges::default();
        edges.attach(1, 11, start).unwrap();
        edges.enroll(1, status("a.test", "node-a"), start).unwrap();
        edges.attach(2, 12, start).unwrap();
        edges
            .enroll(2, status("alias.test", "node-a"), start)
            .unwrap();
        assert!(!edges.edge(Some(1), start).unwrap().enrolled());
        assert!(edges
            .edge(Some(1), start)
            .unwrap()
            .enrollment
            .node_id
            .is_none());
        assert!(edges.edge(Some(2), start).unwrap().enrolled());
        edges.reset("a.test", Some("node-a"));
        assert!(!edges.edge(Some(2), start).unwrap().enrolled());
    }

    #[test]
    fn cap_refuses_33rd_edge_transiently() {
        let mut edges = InboundEdges::default();
        for pid in 1..=32 {
            edges.attach(pid, u64::from(pid) + 10, start).unwrap();
        }
        assert_eq!(edges.attach(33, 43, start), Err("inbound_edges_full"));
        assert_eq!(edges.attach(1, 11, start), Ok(()));
        edges.prune(|pid| if pid == 1 { None } else { start(pid) });
        assert_eq!(edges.attach(33, 43, start), Ok(()));
    }
}
