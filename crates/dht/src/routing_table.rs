use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use librqbit_core::id20::Id20;
use serde::{ser::SerializeMap, Deserialize, Serialize};
use tracing::debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum BucketTreeNodeData {
    // TODO: maybe replace that with SmallVec<8>?
    Leaf(Vec<RoutingTableNode>),
    LeftRight(usize, usize),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BucketTreeNode {
    bits: u8,
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    start: Id20,
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    end_inclusive: Id20,
    data: BucketTreeNodeData,
}

#[derive(Debug, Clone)]
pub struct BucketTree {
    data: Vec<BucketTreeNode>,
}

impl<'de> Deserialize<'de> for BucketTree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BucketTree;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "a map with key \"flat\"")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut data: Option<Vec<BucketTreeNode>> = None;
                loop {
                    match map.next_key::<String>()?.as_deref() {
                        Some("flat") => {
                            let buckets = map.next_value::<Vec<BucketTreeNode>>()?;
                            data = Some(buckets)
                        }
                        Some(_) => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                        None => {
                            use serde::de::Error;
                            match data.take() {
                                Some(data) => return Ok(BucketTree { data }),
                                None => return Err(A::Error::missing_field("flat")),
                            }
                        }
                    }
                }
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

impl Serialize for BucketTree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        struct Node<'a> {
            tree: &'a BucketTree,
            idx: usize,
        }

        impl<'a> Serialize for Node<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut map = serializer.serialize_map(None)?;
                let node = &self.tree.data[self.idx];
                map.serialize_entry("bits", &node.bits)?;
                map.serialize_entry("start", &node.start.as_string())?;
                map.serialize_entry("end", &node.end_inclusive.as_string())?;
                match &node.data {
                    BucketTreeNodeData::Leaf(nodes) => {
                        map.serialize_entry("nodes", &nodes)?;
                    }
                    BucketTreeNodeData::LeftRight(l, r) => {
                        map.serialize_entry(
                            "left",
                            &(Node {
                                idx: *l,
                                tree: self.tree,
                            }),
                        )?;
                        map.serialize_entry(
                            "right",
                            &(Node {
                                idx: *r,
                                tree: self.tree,
                            }),
                        )?;
                    }
                }
                map.end()
            }
        }

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("nodes_len", &self.data.len())?;
        map.serialize_entry("nodes_capacity", &self.data.capacity())?;
        map.serialize_entry("node_memory_bytes", &std::mem::size_of::<BucketTreeNode>())?;
        map.serialize_entry(
            "nodes_memory_bytes",
            &(std::mem::size_of::<BucketTreeNode>() * self.data.capacity()),
        )?;
        map.serialize_entry("tree", &Node { tree: self, idx: 0 })?;
        map.serialize_entry("flat", &self.data)?;
        map.end()
    }
}

pub struct BucketTreeIterator<'a> {
    tree: &'a BucketTree,
    current: std::slice::Iter<'a, RoutingTableNode>,
    queue: Vec<usize>,
}

impl<'a> BucketTreeIterator<'a> {
    fn new(tree: &'a BucketTree) -> Self {
        let mut queue = Vec::new();
        let mut current = 0;
        let current_slice = loop {
            match &tree.data[current].data {
                BucketTreeNodeData::Leaf(nodes) => break nodes.iter(),
                BucketTreeNodeData::LeftRight(left, right) => {
                    queue.push(*right);
                    current = *left;
                }
            }
        };
        BucketTreeIterator {
            tree,
            current: current_slice,
            queue,
        }
    }
}

impl<'a> Iterator for BucketTreeIterator<'a> {
    type Item = &'a RoutingTableNode;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(v) = self.current.next() {
            return Some(v);
        };

        loop {
            let idx = self.queue.pop()?;
            match &self.tree.data[idx].data {
                BucketTreeNodeData::Leaf(nodes) => {
                    self.current = nodes.iter();
                    match self.current.next() {
                        Some(v) => return Some(v),
                        None => continue,
                    }
                }
                BucketTreeNodeData::LeftRight(left, right) => {
                    self.queue.push(*right);
                    self.queue.push(*left);
                    continue;
                }
            }
        }
    }
}

fn compute_split_start_end(
    start: Id20,
    end_inclusive: Id20,
    bits: u8,
) -> ((Id20, Id20), (Id20, Id20)) {
    let changing_bit = 160 - bits;
    let new_left_end = {
        let mut c = end_inclusive;
        c.set_bit(changing_bit, false);
        c
    };
    let new_right_start = {
        let mut c = start;
        c.set_bit(changing_bit, true);
        c
    };
    debug_assert!(
        start < new_left_end,
        "expected start({:?}) < new_left_end({:?}); start={:?}, end={:?}, bits={}",
        start,
        new_left_end,
        start,
        end_inclusive,
        bits
    );
    debug_assert!(
        new_left_end < new_right_start,
        "expected new_left_end({:?}) < new_right_start({:?}); start={:?}, end={:?}, bits={}",
        new_left_end,
        new_right_start,
        start,
        end_inclusive,
        bits
    );
    debug_assert!(
        new_right_start < end_inclusive,
        "expected new_right_start({:?}) < end_inclusive({:?}); start={:?}, end={:?}, bits={}",
        new_right_start,
        end_inclusive,
        start,
        end_inclusive,
        bits
    );
    ((start, new_left_end), (new_right_start, end_inclusive))
}

#[derive(Debug)]
pub enum InsertResult {
    WasExisting,
    ReplacedBad(RoutingTableNode),
    Added,
    Ignored,
}

impl BucketTree {
    pub fn new() -> Self {
        BucketTree {
            data: vec![BucketTreeNode {
                bits: 160,
                start: Id20([0u8; 20]),
                end_inclusive: Id20([0xff; 20]),
                data: BucketTreeNodeData::Leaf(Vec::new()),
            }],
        }
    }
    pub fn iter(&self) -> BucketTreeIterator<'_> {
        BucketTreeIterator::new(self)
    }

    fn get_leaf(&self, id: &Id20) -> usize {
        let mut idx = 0;
        loop {
            let node = &self.data[idx];
            match node.data {
                BucketTreeNodeData::Leaf(_) => return idx,
                BucketTreeNodeData::LeftRight(left_idx, right_idx) => {
                    let left = &self.data[left_idx];
                    if *id >= left.start && *id <= left.end_inclusive {
                        idx = left_idx;
                        continue;
                    };
                    idx = right_idx;
                }
            }
        }
    }

    pub fn get_mut(&mut self, id: &Id20) -> Option<&mut RoutingTableNode> {
        let idx = self.get_leaf(id);
        match &mut self.data[idx].data {
            BucketTreeNodeData::Leaf(nodes) => nodes.iter_mut().find(|b| b.id == *id),
            BucketTreeNodeData::LeftRight(_, _) => unreachable!(),
        }
    }

    pub fn add_node(&mut self, self_id: &Id20, id: Id20, addr: SocketAddr) -> InsertResult {
        let idx = self.get_leaf(&id);
        self.insert_into_leaf(idx, self_id, id, addr)
    }
    fn insert_into_leaf(
        &mut self,
        mut idx: usize,
        self_id: &Id20,
        id: Id20,
        addr: SocketAddr,
    ) -> InsertResult {
        // The loop here is for this case:
        // in case we split a node into two, and it degenerates into all the leaves
        // being on one side, we'll need to split again "recursively" until there's space
        // for the new node.
        // The loop is to remove the recursion. NOTE: it might have compiled to tail recursion
        // anyway, but whatever, did not check.
        loop {
            let leaf = &mut self.data[idx];
            let nodes = match &mut leaf.data {
                BucketTreeNodeData::Leaf(nodes) => nodes,
                BucketTreeNodeData::LeftRight(_, _) => unreachable!(),
            };
            // if already found, quit
            if nodes.iter().any(|r| r.id == id) {
                return InsertResult::WasExisting;
            }

            let mut new_node = RoutingTableNode {
                id,
                addr,
                last_request: None,
                last_response: None,
                outstanding_queries_in_a_row: 0,
            };

            if nodes.len() < 8 {
                nodes.push(new_node);
                nodes.sort_by_key(|n| n.id);
                return InsertResult::Added;
            }

            // Try replace a bad node
            if let Some(bad_node) = nodes
                .iter_mut()
                .find(|r| matches!(r.status(), NodeStatus::Bad))
            {
                std::mem::swap(bad_node, &mut new_node);
                nodes.sort_by_key(|n| n.id);
                debug!("replaced bad node {:?}", new_node);
                return InsertResult::ReplacedBad(new_node);
            }

            // if our id is not inside, don't bother.
            if *self_id < leaf.start || *self_id > leaf.end_inclusive {
                return InsertResult::Ignored;
            }

            // Split
            let ((ls, le), (rs, re)) =
                compute_split_start_end(leaf.start, leaf.end_inclusive, leaf.bits);
            let (mut ld, mut rd) = (Vec::new(), Vec::new());
            for node in nodes.drain(0..) {
                if node.id < rs {
                    ld.push(node);
                } else {
                    rd.push(node)
                }
            }

            let left = BucketTreeNode {
                bits: leaf.bits - 1,
                start: ls,
                end_inclusive: le,
                data: BucketTreeNodeData::Leaf(ld),
            };
            let right = BucketTreeNode {
                bits: leaf.bits - 1,
                start: rs,
                end_inclusive: re,
                data: BucketTreeNodeData::Leaf(rd),
            };

            let left_idx = {
                let l = self.data.len();
                self.data.push(left);
                l
            };
            let right_idx = {
                let l = self.data.len();
                self.data.push(right);
                l
            };

            self.data[idx].data = BucketTreeNodeData::LeftRight(left_idx, right_idx);
            if id < rs {
                idx = left_idx
            } else {
                idx = right_idx
            }
        }
    }
}

impl Default for BucketTree {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingTableNode {
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    id: Id20,
    addr: SocketAddr,
    #[serde(skip)]
    last_request: Option<Instant>,
    #[serde(skip)]
    last_response: Option<Instant>,
    #[serde(skip)]
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
        if self.outstanding_queries_in_a_row > 0 && last_request.elapsed() > Duration::from_secs(10)
        {
            return NodeStatus::Bad;
        }
        if self.last_response.is_some() {
            return NodeStatus::Good;
        }
        NodeStatus::Questionable
    }

    pub fn mark_outgoing_request(&mut self) {
        self.last_request = Some(Instant::now());
        self.outstanding_queries_in_a_row += 1;
    }

    pub fn mark_response(&mut self) {
        let now = Instant::now();
        self.last_response = Some(now);
        if self.last_request.is_none() {
            self.last_request = Some(now);
        }
        self.outstanding_queries_in_a_row = 0;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingTable {
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    id: Id20,
    size: usize,
    buckets: BucketTree,
}

impl RoutingTable {
    pub fn new(id: Id20) -> Self {
        Self {
            id,
            buckets: BucketTree::new(),
            size: 0,
        }
    }
    pub fn id(&self) -> Id20 {
        self.id
    }
    pub fn len(&self) -> usize {
        self.size
    }
    pub fn sorted_by_distance_from(&self, id: Id20) -> Vec<&RoutingTableNode> {
        let mut result = Vec::with_capacity(self.size);
        for node in self.buckets.iter() {
            result.push(node);
        }
        result.sort_by_key(|n| id.distance(&n.id));
        result
    }

    pub fn add_node(&mut self, id: Id20, addr: SocketAddr) -> InsertResult {
        let res = self.buckets.add_node(&self.id, id, addr);
        let replaced = match &res {
            InsertResult::WasExisting => false,
            InsertResult::ReplacedBad(..) => true,
            InsertResult::Added => true,
            InsertResult::Ignored => false,
        };
        if replaced {
            self.size += 1;
        }
        res
    }
    pub fn mark_outgoing_request(&mut self, id: &Id20) -> bool {
        let r = match self.buckets.get_mut(id) {
            Some(r) => r,
            None => return false,
        };
        r.mark_outgoing_request();
        true
    }

    pub fn mark_response(&mut self, id: &Id20) -> bool {
        let r = match self.buckets.get_mut(id) {
            Some(r) => r,
            None => return false,
        };
        r.mark_response();
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        net::{Ipv4Addr, SocketAddr, SocketAddrV4},
        str::FromStr,
    };

    use librqbit_core::id20::Id20;
    use rand::Rng;

    use crate::routing_table::compute_split_start_end;

    use super::RoutingTable;

    #[test]
    fn compute_split_start_end_root() {
        let start = Id20([0u8; 20]);
        let end = Id20([0xff; 20]);
        assert_eq!(
            compute_split_start_end(start, end, 160),
            (
                (
                    start,
                    Id20::from_str("7fffffffffffffffffffffffffffffffffffffff").unwrap()
                ),
                (
                    Id20::from_str("8000000000000000000000000000000000000000").unwrap(),
                    end
                )
            )
        )
    }

    #[test]
    fn compute_split_start_end_second_split() {
        let start = Id20::from_str("8000000000000000000000000000000000000000").unwrap();
        let end = Id20([0xff; 20]);
        assert_eq!(
            compute_split_start_end(start, end, 159),
            (
                (
                    start,
                    Id20::from_str("bfffffffffffffffffffffffffffffffffffffff").unwrap()
                ),
                (
                    Id20::from_str("c000000000000000000000000000000000000000").unwrap(),
                    end
                )
            )
        )
    }

    #[test]
    fn compute_split_start_end_3() {
        let start = Id20::from_str("8000000000000000000000000000000000000000").unwrap();
        let end = Id20([0xff; 20]);
        assert_eq!(
            compute_split_start_end(start, end, 159),
            (
                (
                    start,
                    Id20::from_str("bfffffffffffffffffffffffffffffffffffffff").unwrap()
                ),
                (
                    Id20::from_str("c000000000000000000000000000000000000000").unwrap(),
                    end
                )
            )
        )
    }

    fn random_id_20() -> Id20 {
        let mut id20 = [0u8; 20];
        rand::thread_rng().fill(&mut id20);
        Id20(id20)
    }

    fn generate_socket_addr() -> SocketAddr {
        let mut ipv4_addr = [0u8; 6];
        rand::thread_rng().fill(&mut ipv4_addr);
        let ip = Ipv4Addr::new(ipv4_addr[0], ipv4_addr[1], ipv4_addr[2], ipv4_addr[3]);
        let port = ((ipv4_addr[4] as u16) << 8) + (ipv4_addr[5] as u16);
        SocketAddrV4::new(ip, port).into()
    }

    fn generate_table(length: Option<usize>) -> RoutingTable {
        let my_id = random_id_20();
        let mut rtable = RoutingTable::new(my_id);
        for _ in 0..length.unwrap_or(16536) {
            let other_id = random_id_20();
            let addr = generate_socket_addr();
            rtable.add_node(other_id, addr);
        }
        rtable
    }

    #[test]
    fn test_iter_is_ordered() {
        let table = generate_table(None);
        let mut it = table.buckets.iter();
        let mut previous = it.next().unwrap();
        for node in it {
            assert!(node.id() > previous.id());
            previous = node;
        }
    }

    #[test]
    fn test_sorted_by_distance_from() {
        let id = random_id_20();
        let rtable = generate_table(None);
        assert_eq!(rtable.sorted_by_distance_from(id).len(), rtable.size);
    }

    #[test]
    fn serialize_deserialize_routing_table() {
        let table = generate_table(Some(1000));
        let v = serde_json::to_vec(&table).unwrap();
        let _: RoutingTable = serde_json::from_reader(Cursor::new(v)).unwrap();
    }
}
