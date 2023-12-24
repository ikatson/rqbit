use std::{net::SocketAddr, time::Instant};

use librqbit_core::hash_id::Id20;
use rand::RngCore;
use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};
use tracing::{debug, trace};

use crate::INACTIVITY_TIMEOUT;

#[derive(Clone, Debug)]
pub struct LeafBucket {
    pub nodes: Vec<RoutingTableNode>,
    pub last_refreshed: Instant,
}

impl Serialize for LeafBucket {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("LeafBucket", 2)?;
        s.serialize_field("nodes", &self.nodes)?;
        s.serialize_field(
            "last_refreshed",
            &format!("{:?}", self.last_refreshed.elapsed()),
        )?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for LeafBucket {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Tmp {
            nodes: Vec<RoutingTableNode>,
        }
        Tmp::deserialize(deserializer).map(|t| Self {
            nodes: t.nodes,
            last_refreshed: Instant::now(),
        })
    }
}

impl Default for LeafBucket {
    fn default() -> Self {
        Self {
            nodes: Default::default(),
            last_refreshed: Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum BucketTreeNodeData {
    Leaf(LeafBucket),
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BucketTree {
    data: Vec<BucketTreeNode>,
    size: usize,
    max_size: usize,
}

pub struct BucketTreeIteratorItem<'a> {
    pub bits: u8,
    pub start: &'a Id20,
    pub end_inclusive: &'a Id20,
    pub leaf: &'a LeafBucket,
}

impl<'a> BucketTreeIteratorItem<'a> {
    pub fn random_within(&self) -> Id20 {
        generate_random_id(self.start, self.bits)
    }
}

struct BucketTreeIterator<'a> {
    tree: &'a BucketTree,
    queue: Vec<usize>,
}

impl<'a> BucketTreeIterator<'a> {
    fn new(tree: &'a BucketTree) -> Self {
        let queue = vec![0];
        BucketTreeIterator { tree, queue }
    }
}

impl<'a> Iterator for BucketTreeIterator<'a> {
    type Item = BucketTreeIteratorItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let idx = self.queue.pop()?;
            match self.tree.data.get(idx) {
                Some(node) => match &node.data {
                    BucketTreeNodeData::Leaf(leaf) => {
                        return Some(BucketTreeIteratorItem {
                            bits: node.bits,
                            start: &node.start,
                            end_inclusive: &node.end_inclusive,
                            leaf,
                        });
                    }
                    BucketTreeNodeData::LeftRight(left, right) => {
                        self.queue.push(*right);
                        self.queue.push(*left);
                        continue;
                    }
                },
                None => continue,
            }
        }
    }
}

pub fn generate_random_id(start: &Id20, bits: u8) -> Id20 {
    let mut data = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut data);
    let mut data = Id20::new(data);
    let remaining_bits = 160 - bits;
    for bit in 0..remaining_bits {
        data.set_bit(bit, start.get_bit(bit));
    }
    data
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
    pub fn new(max_size: usize) -> Self {
        BucketTree {
            data: vec![BucketTreeNode {
                bits: 160,
                start: Id20::new([0u8; 20]),
                end_inclusive: Id20::new([0xff; 20]),
                data: BucketTreeNodeData::Leaf(Default::default()),
            }],
            size: 0,
            max_size,
        }
    }

    fn iter_leaves(&self) -> BucketTreeIterator<'_> {
        BucketTreeIterator::new(self)
    }

    fn iter(&self) -> impl Iterator<Item = &'_ RoutingTableNode> + '_ {
        self.iter_leaves().flat_map(|l| l.leaf.nodes.iter())
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

    pub fn get_mut(&mut self, id: &Id20, refresh: bool) -> Option<&mut RoutingTableNode> {
        let idx = self.get_leaf(id);
        match &mut self.data[idx].data {
            BucketTreeNodeData::Leaf(leaf) => {
                let r = leaf.nodes.iter_mut().find(|b| b.id == *id);
                if r.is_some() && refresh {
                    leaf.last_refreshed = Instant::now()
                }
                r
            }
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
            if nodes.nodes.iter().any(|r| r.id == id) {
                return InsertResult::WasExisting;
            }

            let mut new_node = RoutingTableNode {
                id,
                addr,
                last_request: None,
                last_response: None,
                last_query: None,
                errors_in_a_row: 0,
            };

            // Try replace a bad node
            if let Some(bad_node) = nodes
                .nodes
                .iter_mut()
                .find(|r| matches!(r.status(), NodeStatus::Bad))
            {
                std::mem::swap(bad_node, &mut new_node);
                nodes.nodes.sort_by_key(|n| n.id);
                debug!("replaced bad node {:?}", new_node);
                nodes.last_refreshed = Instant::now();
                return InsertResult::ReplacedBad(new_node);
            }

            // if max size reached, don't bother
            if self.size == self.max_size {
                trace!(
                    "can't add node to routing table, max size of {} reached",
                    self.max_size
                );
                return InsertResult::Ignored;
            }

            if nodes.nodes.len() < 8 {
                nodes.nodes.push(new_node);
                nodes.nodes.sort_by_key(|n| n.id);
                nodes.last_refreshed = Instant::now();
                self.size += 1;
                return InsertResult::Added;
            }

            // if our id is not inside, don't bother.
            if *self_id < leaf.start || *self_id > leaf.end_inclusive {
                return InsertResult::Ignored;
            }

            // Split
            let ((ls, le), (rs, re)) =
                compute_split_start_end(leaf.start, leaf.end_inclusive, leaf.bits);
            let (mut ld, mut rd) = (Vec::new(), Vec::new());
            for node in nodes.nodes.drain(0..) {
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
                data: BucketTreeNodeData::Leaf(LeafBucket {
                    nodes: ld,
                    ..Default::default()
                }),
            };
            let right = BucketTreeNode {
                bits: leaf.bits - 1,
                start: rs,
                end_inclusive: re,
                data: BucketTreeNodeData::Leaf(LeafBucket {
                    nodes: rd,
                    ..Default::default()
                }),
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

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingTableNode {
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    id: Id20,
    addr: SocketAddr,
    #[serde(skip)]
    last_request: Option<Instant>,
    #[serde(skip)]
    last_response: Option<Instant>,
    #[serde(skip)]
    last_query: Option<Instant>,
    #[serde(skip)]
    errors_in_a_row: usize,
}

impl Serialize for RoutingTableNode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_struct("RoutingTableNode", 3)?;
        s.serialize_field("id", &self.id.as_string())?;
        s.serialize_field("addr", &self.addr)?;
        s.serialize_field("status", &self.status())?;
        if let Some(l) = self.last_request {
            s.serialize_field("last_request_ago", &l.elapsed())?;
        }
        if let Some(l) = self.last_response {
            s.serialize_field("last_response_ago", &l.elapsed())?;
        }
        if let Some(l) = self.last_query {
            s.serialize_field("last_query_ago", &l.elapsed())?;
        }
        s.serialize_field("errors_in_a_row", &self.errors_in_a_row)?;
        s.end()
    }
}

#[derive(Serialize, Debug)]
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
        match (self.last_request, self.last_response, self.last_query) {
            // Nodes become bad when they fail to respond to multiple queries in a row.
            (Some(_), _, _) if self.errors_in_a_row >= 2 => NodeStatus::Bad,

            // A good node is a node has responded to one of our queries within the last 15 minutes.
            // A node is also good if it has ever responded to one of our queries and has sent
            // us a query within the last 15 minutes.
            (Some(_), Some(last_incoming), _) | (Some(_), Some(_), Some(last_incoming))
                if last_incoming.elapsed() < INACTIVITY_TIMEOUT =>
            {
                NodeStatus::Good
            }

            // After 15 minutes of inactivity, a node becomes questionable.
            // The moment we send a request to it, it stops becoming questionable and becomes Unknown / Bad.
            (last_outgoing, _, Some(last_incoming)) | (last_outgoing, Some(last_incoming), _)
                if last_incoming.elapsed() > INACTIVITY_TIMEOUT
                    && last_outgoing
                        .map(|e| e.elapsed() > INACTIVITY_TIMEOUT)
                        .unwrap_or(true) =>
            {
                NodeStatus::Questionable
            }
            _ => NodeStatus::Unknown,
        }
    }

    pub fn mark_outgoing_request(&mut self) {
        self.last_request = Some(Instant::now());
    }

    pub fn mark_last_query(&mut self) {
        self.last_query = Some(Instant::now());
    }

    pub fn mark_response(&mut self) {
        let now = Instant::now();
        self.last_response = Some(now);
        if self.last_request.is_none() {
            self.last_request = Some(now);
        }
        self.errors_in_a_row = 0;
    }

    pub fn mark_error(&mut self) {
        self.errors_in_a_row += 1;
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
    const DEFAULT_MAX_SIZE: usize = 512;

    pub fn new(id: Id20, max_size: Option<usize>) -> Self {
        Self {
            id,
            buckets: BucketTree::new(max_size.unwrap_or(Self::DEFAULT_MAX_SIZE)),
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
        result.sort_by_key(|n| {
            // Query decent nodes first.
            let status = match n.status() {
                NodeStatus::Good => 0,
                NodeStatus::Questionable => 0,
                NodeStatus::Unknown => 2,
                NodeStatus::Bad => 3,
            };
            (status, id.distance(&n.id))
        });
        result
    }

    pub fn iter_buckets(&self) -> impl Iterator<Item = BucketTreeIteratorItem<'_>> + '_ {
        self.buckets.iter_leaves()
    }

    pub fn iter(&self) -> impl Iterator<Item = &'_ RoutingTableNode> + '_ {
        self.buckets.iter()
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
        let r = match self.buckets.get_mut(id, false) {
            Some(r) => r,
            None => return false,
        };
        r.mark_outgoing_request();
        true
    }

    pub fn mark_response(&mut self, id: &Id20) -> bool {
        let r = match self.buckets.get_mut(id, true) {
            Some(r) => r,
            None => return false,
        };
        r.mark_response();
        true
    }

    pub fn mark_error(&mut self, id: &Id20) -> bool {
        let r = match self.buckets.get_mut(id, false) {
            Some(r) => r,
            None => return false,
        };
        r.mark_error();
        true
    }

    pub fn mark_last_query(&mut self, id: &Id20) -> bool {
        let r = match self.buckets.get_mut(id, false) {
            Some(r) => r,
            None => return false,
        };
        r.mark_last_query();
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

    use librqbit_core::hash_id::Id20;
    use rand::Rng;

    use crate::routing_table::compute_split_start_end;

    use super::{generate_random_id, RoutingTable};

    #[test]
    fn compute_split_start_end_root() {
        let start = Id20::new([0u8; 20]);
        let end = Id20::new([0xff; 20]);
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
        let end = Id20::new([0xff; 20]);
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
        let end = Id20::new([0xff; 20]);
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
        Id20::new(id20)
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
        let mut rtable = RoutingTable::new(my_id, None);
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

    #[test]
    fn test_generate_random_id() {
        let start = Id20::from_str("3000000000000000000000000000000000000000").unwrap();
        let end = Id20::from_str("3fffffffffffffffffffffffffffffffffffffff").unwrap();
        let bits = 156;
        for _ in 0..100 {
            let id = dbg!(generate_random_id(&start, bits));
            assert!(id >= start && id <= end, "{:?}", id);
        }
    }
}
