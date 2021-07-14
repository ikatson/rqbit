use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use librqbit_core::id20::Id20;
use log::debug;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
enum BucketTreeNode {
    Leaf(Vec<RoutingTableNode>),
    LeftRight(Box<BucketTree>, Box<BucketTree>),
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketTree {
    bits: u8,
    start: Id20,
    end_inclusive: Id20,
    data: BucketTreeNode,
}

pub struct BucketTreeNodeIterator<'a> {
    current: std::slice::Iter<'a, RoutingTableNode>,
    queue: Vec<&'a BucketTree>,
}

impl<'a> BucketTreeNodeIterator<'a> {
    fn new(mut tree: &'a BucketTree) -> Self {
        let mut queue = Vec::new();
        let current = loop {
            match &tree.data {
                BucketTreeNode::Leaf(nodes) => break nodes.iter(),
                BucketTreeNode::LeftRight(left, right) => {
                    queue.push(right.as_ref());
                    tree = left.as_ref()
                }
            }
        };
        BucketTreeNodeIterator { current, queue }
    }
}

impl<'a> Iterator for BucketTreeNodeIterator<'a> {
    type Item = &'a RoutingTableNode;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(v) = self.current.next() {
            return Some(v);
        };

        loop {
            let tree = self.queue.pop()?;
            match &tree.data {
                BucketTreeNode::Leaf(nodes) => {
                    self.current = nodes.iter();
                    match self.current.next() {
                        Some(v) => return Some(v),
                        None => continue,
                    }
                }
                BucketTreeNode::LeftRight(left, right) => {
                    self.queue.push(right.as_ref());
                    self.queue.push(left.as_ref());
                    continue;
                }
            }
        }
    }
}

pub struct BucketTreeNodeIteratorMut<'a> {
    current: std::slice::IterMut<'a, RoutingTableNode>,
    queue: Vec<&'a mut BucketTree>,
}

impl<'a> BucketTreeNodeIteratorMut<'a> {
    fn new(mut tree: &'a mut BucketTree) -> Self {
        let mut queue = Vec::new();
        let current = loop {
            match &mut tree.data {
                BucketTreeNode::Leaf(nodes) => break nodes.iter_mut(),
                BucketTreeNode::LeftRight(left, right) => {
                    queue.push(right.as_mut());
                    tree = left.as_mut()
                }
            }
        };
        BucketTreeNodeIteratorMut { current, queue }
    }
}

impl<'a> Iterator for BucketTreeNodeIteratorMut<'a> {
    type Item = &'a mut RoutingTableNode;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(v) = self.current.next() {
            return Some(v);
        };

        loop {
            let tree = self.queue.pop()?;
            match &mut tree.data {
                BucketTreeNode::Leaf(nodes) => {
                    self.current = nodes.iter_mut();
                    match self.current.next() {
                        Some(v) => return Some(v),
                        None => continue,
                    }
                }
                BucketTreeNode::LeftRight(left, right) => {
                    self.queue.push(right.as_mut());
                    self.queue.push(left.as_mut());
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
            bits: 160,
            start: Id20([0u8; 20]),
            end_inclusive: Id20([0xff; 20]),
            data: BucketTreeNode::Leaf(Vec::with_capacity(8)),
        }
    }
    pub fn iter(&self) -> BucketTreeNodeIterator<'_> {
        BucketTreeNodeIterator::new(self)
    }

    pub fn iter_mut(&mut self) -> BucketTreeNodeIteratorMut<'_> {
        BucketTreeNodeIteratorMut::new(self)
    }

    pub fn get_mut(&mut self, id: &Id20) -> Option<&mut RoutingTableNode> {
        if !(*id >= self.start && *id <= self.end_inclusive) {
            return None;
        }
        match &mut self.data {
            BucketTreeNode::Leaf(nodes) => nodes.iter_mut().find(|b| b.id == *id),
            BucketTreeNode::LeftRight(left, right) => {
                left.get_mut(id).or_else(move || right.get_mut(id))
            }
        }
    }

    pub fn add_node(&mut self, self_id: &Id20, id: Id20, addr: SocketAddr) -> InsertResult {
        let mut tree = self;
        loop {
            match &mut tree.data {
                BucketTreeNode::Leaf(_) => {
                    assert!(id >= tree.start && id <= tree.end_inclusive);
                    return tree.insert_into_leaf(self_id, id, addr);
                }
                BucketTreeNode::LeftRight(left, right) => {
                    if id >= right.start {
                        // Erase lifetime.
                        // Safety: this is safe as it's a tree, not a DAG or Graph.
                        tree = unsafe { &mut *(right.as_mut() as *mut _) };
                        continue;
                    }
                    tree = unsafe { &mut *(left.as_mut() as *mut _) };
                }
            }
        }
    }
    fn insert_into_leaf(&mut self, self_id: &Id20, id: Id20, addr: SocketAddr) -> InsertResult {
        let nodes = match &mut self.data {
            BucketTreeNode::Leaf(nodes) => nodes,
            BucketTreeNode::LeftRight(_, _) => unreachable!(),
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
        if *self_id < self.start || *self_id > self.end_inclusive {
            return InsertResult::Ignored;
        }

        // Split
        let ((ls, le), (rs, re)) =
            compute_split_start_end(self.start, self.end_inclusive, self.bits);
        let (mut ld, mut rd) = (Vec::with_capacity(8), Vec::with_capacity(8));
        for node in nodes.drain(0..) {
            if node.id < rs {
                ld.push(node);
            } else {
                rd.push(node)
            }
        }
        let mut left = BucketTree {
            bits: self.bits - 1,
            start: ls,
            end_inclusive: le,
            data: BucketTreeNode::Leaf(ld),
        };
        let mut right = BucketTree {
            bits: self.bits - 1,
            start: rs,
            end_inclusive: re,
            data: BucketTreeNode::Leaf(rd),
        };

        let result = if id < rs {
            left.add_node(self_id, id, addr)
        } else {
            right.add_node(self_id, id, addr)
        };

        self.data = BucketTreeNode::LeftRight(Box::new(left), Box::new(right));
        result
    }
}

impl Default for BucketTree {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutingTableNode {
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

#[derive(Debug, Clone, Serialize)]
pub struct RoutingTable {
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

    pub fn sorted_by_distance_from_mut(&mut self, id: Id20) -> Vec<&mut RoutingTableNode> {
        let mut result = Vec::with_capacity(self.size);
        for node in self.buckets.iter_mut() {
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
    use std::net::SocketAddrV4;

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
                    Id20([
                        0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
                    ])
                ),
                (
                    Id20([
                        0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
                    ]),
                    end
                )
            )
        )
    }

    #[test]
    fn compute_split_start_end_second_split() {
        let start = Id20([
            0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        let end = Id20([0xff; 20]);
        assert_eq!(
            compute_split_start_end(start, end, 159),
            (
                (
                    start,
                    Id20([
                        0xbf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
                    ])
                ),
                (
                    Id20([
                        0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
                    ]),
                    end
                )
            )
        )
    }

    #[test]
    fn compute_split_start_end_3() {
        let start = Id20([
            0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        let end = Id20([0xff; 20]);
        assert_eq!(
            compute_split_start_end(start, end, 159),
            (
                (
                    start,
                    Id20([
                        0xbf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
                    ])
                ),
                (
                    Id20([
                        0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
                    ]),
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

    #[test]
    fn simulate_tree() {
        let my_id = random_id_20();
        let mut rtable = RoutingTable::new(my_id);
        for i in 0..u16::MAX {
            let other_id = random_id_20();
            let addr = std::net::SocketAddr::V4(SocketAddrV4::new("0.0.0.0".parse().unwrap(), i));
            rtable.add_node(other_id, addr);
        }
        dbg!(&rtable);
        assert_eq!(rtable.sorted_by_distance_from(my_id).len(), rtable.size);
    }
}
