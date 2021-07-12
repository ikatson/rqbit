use std::{
    collections::BTreeMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

enum BucketTreeNode {
    Leaf(Vec<RoutingTableNode>),
    LeftRight(Box<BucketTree>, Box<BucketTree>),
}

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
    ((start, new_left_end), (new_right_start, end_inclusive))
}

impl BucketTree {
    pub fn new() -> Self {
        BucketTree {
            bits: 160,
            start: Id20([0u8; 20]),
            end_inclusive: Id20([0xff; 20]),
            data: BucketTreeNode::Leaf(Vec::new()),
        }
    }
    pub fn iter(&self) -> BucketTreeNodeIterator<'_> {
        BucketTreeNodeIterator::new(self)
    }
    pub fn add_node(&mut self, self_id: &Id20, id: Id20, addr: SocketAddr) {
        let mut tree = self;
        loop {
            match &mut tree.data {
                BucketTreeNode::Leaf(_) => {
                    assert!(id >= tree.start && id <= tree.end_inclusive);
                    tree.insert_into_leaf(self_id, id, addr)
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
    fn insert_into_leaf(&mut self, self_id: &Id20, id: Id20, addr: SocketAddr) {
        let nodes = match &mut self.data {
            BucketTreeNode::Leaf(nodes) => nodes,
            BucketTreeNode::LeftRight(_, _) => unreachable!(),
        };
        // if already found, quit
        if nodes.iter().find(|r| r.id == id).is_some() {
            return;
        }

        if nodes.len() < 8 {
            nodes.push(RoutingTableNode {
                id,
                addr,
                last_request: None,
                last_response: None,
                outstanding_queries_in_a_row: 0,
            });
            return;
        }

        // if our id is not inside, don't bother.
        if *self_id < self.start || *self_id > self.end_inclusive {
            return;
        }

        todo!()
    }
}

impl Default for BucketTree {
    fn default() -> Self {
        Self::new()
    }
}

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
    pub fn sorted_by_distance_from(&self, id: Id20) -> Vec<&RoutingTableNode> {
        let mut result = Vec::with_capacity(self.size);
        for node in self.buckets.iter() {
            result.push(node);
        }
        result.sort_by_key(|n| id.distance(&n.id));
        result
    }
    pub fn add_node(&mut self, id: Id20, addr: SocketAddr) -> bool {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use crate::{id20::Id20, routing_table::compute_split_start_end};

    #[test]
    fn compute_split_start_end_root() {
        let start = Id20([0u8; 20]);
        let end = Id20([0xffu8; 20]);
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
}
