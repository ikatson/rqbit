use std::{
    collections::BTreeMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

use crate::id20::Id20;

pub struct RoutingTableNode {
    id: Id20,
    addr: SocketAddr,
    last_request: Option<Instant>,
    last_response: Option<Instant>,
    outstanding_queries_in_a_row: usize,
}

pub enum NodeStatus {
    Good,
    Questionable,
    Bad,
    Unknown,
}

impl RoutingTableNode {
    pub fn id(&self) -> Id20 {
        self.id
    }
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    pub fn status(&self) -> NodeStatus {
        // TODO: this is just a stub with simpler logic
        let last_request = match self.last_request {
            Some(v) => v,
            None => return NodeStatus::Unknown,
        };
        if self.last_response.is_some() {
            return NodeStatus::Good;
        }
        NodeStatus::Questionable
    }
}

struct Bucket {
    bits: u8,
    nodes: Vec<RoutingTableNode>,
    end: Id20,
}

pub struct RoutingTable {
    id: Id20,
    size: usize,
    buckets: BTreeMap<Id20, Bucket>,
}

impl RoutingTable {
    pub fn new(id: Id20) -> Self {
        let initial_bucket = Id20([0u8; 20]);
        let mut buckets = BTreeMap::new();
        buckets.insert(
            initial_bucket,
            Bucket {
                bits: 160,
                nodes: Vec::new(),
            },
        );
        Self {
            id,
            buckets,
            size: 0,
        }
    }
    pub fn sorted_by_distance_from(&self, id: Id20) -> Vec<&RoutingTableNode> {
        let mut result = Vec::with_capacity(self.size);
        for bucket in self.buckets.values() {
            for node in bucket.nodes.iter() {
                result.push(node);
            }
        }
        result.sort_by_key(|n| id.distance(&n.id));
        result
    }
    pub fn add_node(&mut self, id: Id20, addr: SocketAddr) -> bool {}
}
