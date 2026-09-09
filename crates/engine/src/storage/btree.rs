//! A B+ tree index over one column of one table.
//!
//! Zone maps and bloom filters both *reject* row groups; neither can point at a
//! row. A B+ tree can. For a predicate that matches a handful of rows out of a
//! million, walking four levels of tree and gathering the answers beats reading
//! every row group that survived pruning -- and for a predicate that matches a
//! third of the table it loses badly, because random gathers give up the
//! sequential access that makes columnar scans fast. Which is why this is a
//! *choice* the physical planner makes on estimated selectivity, not a
//! structure that is always used when present.
//!
//! ## Shape
//!
//! Values live only in the leaves; internal nodes hold separator keys that
//! route a search. Leaves are chained left to right, so a range scan descends
//! once and then walks the chain -- the property that makes a B+ tree, rather
//! than a plain B-tree, the right structure for `WHERE x BETWEEN a AND b`.
//!
//! Nodes are held in an arena and referred to by index. That sidesteps the
//! parent-pointer aliasing that makes tree surgery painful in Rust, and it
//! makes the tree trivially serializable for the index visualizer in the UI.
//!
//! ## NULLs are not in the index
//!
//! A NULL key is never inserted. Every predicate an index scan can serve --
//! `=`, `<`, `<=`, `>`, `>=`, `BETWEEN` -- evaluates to UNKNOWN, never TRUE,
//! when either side is NULL, so no such predicate can match a NULL row and
//! leaving those rows out of the tree cannot lose an answer. The obligation
//! this creates lands on the planner: it must never serve `IS NULL` from the
//! index, because that predicate *is* true for exactly the rows this tree
//! omits. See `exec::index` for where that is enforced.

use std::cmp::Ordering;

use crate::types::{compare, ScalarValue};

/// Maximum keys per node. A node splits when it exceeds this.
///
/// 64 keeps a million distinct keys within four levels while leaving each node
/// small enough that the binary search inside it stays in cache. The tree is
/// in memory, so the usual "match the disk page size" argument does not apply;
/// what matters is that the search is `log(n)` with a fat base.
const NODE_CAPACITY: usize = 64;

/// One end of a range scan.
#[derive(Debug, Clone, PartialEq)]
pub enum Bound {
    Unbounded,
    Included(ScalarValue),
    Excluded(ScalarValue),
}

#[derive(Debug, Clone)]
enum NodeKind {
    Internal {
        /// One more child than there are keys: `children[i]` holds keys less
        /// than `keys[i]`, and `children[keys.len()]` holds the rest.
        children: Vec<usize>,
    },
    Leaf {
        /// Row ids per key, parallel to `keys`. A `Vec` per key because a
        /// non-unique index is the common case -- a column with no duplicates
        /// is the exception, not the rule.
        rows: Vec<Vec<u32>>,
        /// The next leaf to the right, for range scans.
        next: Option<usize>,
    },
}

#[derive(Debug, Clone)]
struct Node {
    keys: Vec<ScalarValue>,
    kind: NodeKind,
}

impl Node {
    fn leaf() -> Node {
        Node {
            keys: Vec::new(),
            kind: NodeKind::Leaf {
                rows: Vec::new(),
                next: None,
            },
        }
    }

    fn is_leaf(&self) -> bool {
        matches!(self.kind, NodeKind::Leaf { .. })
    }
}

#[derive(Debug, Clone)]
pub struct BPlusTree {
    nodes: Vec<Node>,
    root: usize,
    /// Number of levels, counting the root. A tree holding only a root leaf has
    /// height 1.
    height: usize,
    first_leaf: usize,
    num_entries: usize,
    num_keys: usize,
}

impl Default for BPlusTree {
    fn default() -> Self {
        Self::new()
    }
}

impl BPlusTree {
    pub fn new() -> BPlusTree {
        BPlusTree {
            nodes: vec![Node::leaf()],
            root: 0,
            height: 1,
            first_leaf: 0,
            num_entries: 0,
            num_keys: 0,
        }
    }

    /// Insert one (key, row) pair.
    ///
    /// NULL keys are dropped: see the module comment for why that is sound and
    /// what it obliges the planner to avoid.
    pub fn insert(&mut self, key: ScalarValue, row: u32) {
        if key.is_null() {
            return;
        }

        // Descend to a leaf, remembering the path so a split can be propagated
        // back up without parent pointers.
        let mut path: Vec<usize> = Vec::with_capacity(self.height);
        let mut current = self.root;
        while !self.nodes[current].is_leaf() {
            path.push(current);
            let slot = self.child_slot(current, &key);
            let NodeKind::Internal { children } = &self.nodes[current].kind else {
                unreachable!("loop condition guarantees an internal node")
            };
            current = children[slot];
        }

        let pos = self.nodes[current]
            .keys
            .partition_point(|k| cmp_keys(k, &key) == Ordering::Less);
        let node = &mut self.nodes[current];
        let NodeKind::Leaf { rows, .. } = &mut node.kind else {
            unreachable!("descent ends at a leaf")
        };
        self.num_entries += 1;
        if pos < node.keys.len() && cmp_keys(&node.keys[pos], &key) == Ordering::Equal {
            // A duplicate: the key already routes correctly, so only the row
            // list grows and no split can be needed.
            rows[pos].push(row);
            return;
        }
        node.keys.insert(pos, key);
        rows.insert(pos, vec![row]);
        self.num_keys += 1;

        if node.keys.len() > NODE_CAPACITY {
            self.split(current, path);
        }
    }

    /// Split an overfull node and propagate upward, growing the root if the
    /// split reaches it.
    fn split(&mut self, mut node: usize, mut path: Vec<usize>) {
        loop {
            let (separator, right) = if self.nodes[node].is_leaf() {
                self.split_leaf(node)
            } else {
                self.split_internal(node)
            };

            let Some(parent) = path.pop() else {
                // The root split. A new root one level up is the only way a
                // B+ tree ever gets taller, which is what keeps every leaf at
                // the same depth.
                let root = self.nodes.len();
                self.nodes.push(Node {
                    keys: vec![separator],
                    kind: NodeKind::Internal {
                        children: vec![node, right],
                    },
                });
                self.root = root;
                self.height += 1;
                return;
            };

            let slot = self
                .nodes[parent]
                .keys
                .partition_point(|k| cmp_keys(k, &separator) != Ordering::Greater);
            self.nodes[parent].keys.insert(slot, separator);
            let NodeKind::Internal { children } = &mut self.nodes[parent].kind else {
                unreachable!("a node on the descent path is internal")
            };
            children.insert(slot + 1, right);

            if self.nodes[parent].keys.len() <= NODE_CAPACITY {
                return;
            }
            node = parent;
        }
    }

    /// Move the upper half of a leaf into a new leaf, and return the key to
    /// promote.
    ///
    /// The separator is a *copy* of the new leaf's first key, not a removal:
    /// in a B+ tree every value must remain in a leaf, because the leaf chain
    /// is what range scans read.
    fn split_leaf(&mut self, node: usize) -> (ScalarValue, usize) {
        let mid = self.nodes[node].keys.len() / 2;
        let right = self.nodes.len();

        let keys = self.nodes[node].keys.split_off(mid);
        let NodeKind::Leaf { rows, next } = &mut self.nodes[node].kind else {
            unreachable!("caller checked this is a leaf")
        };
        let right_rows = rows.split_off(mid);
        let right_next = *next;
        *next = Some(right);

        let separator = keys[0].clone();
        self.nodes.push(Node {
            keys,
            kind: NodeKind::Leaf {
                rows: right_rows,
                next: right_next,
            },
        });
        (separator, right)
    }

    /// Move the upper half of an internal node into a new node, and return the
    /// key to promote.
    ///
    /// Here the middle key *moves* up rather than being copied. It is a
    /// separator, not a value, and duplicating it would waste a slot and put
    /// the same routing decision in two places.
    fn split_internal(&mut self, node: usize) -> (ScalarValue, usize) {
        let mid = self.nodes[node].keys.len() / 2;
        let mut keys = self.nodes[node].keys.split_off(mid);
        let separator = keys.remove(0);
        let NodeKind::Internal { children } = &mut self.nodes[node].kind else {
            unreachable!("caller checked this is internal")
        };
        let right_children = children.split_off(mid + 1);

        let right = self.nodes.len();
        self.nodes.push(Node {
            keys,
            kind: NodeKind::Internal {
                children: right_children,
            },
        });
        (separator, right)
    }

    /// Which child of an internal node routes `key`.
    fn child_slot(&self, node: usize, key: &ScalarValue) -> usize {
        // The number of separators at or below `key`: separator `k` sends
        // everything `>= k` to its right, so this is the child to follow.
        self.nodes[node]
            .keys
            .partition_point(|k| cmp_keys(k, key) != Ordering::Greater)
    }

    /// Every row whose key equals `key`, in insertion order.
    pub fn lookup(&self, key: &ScalarValue) -> &[u32] {
        if key.is_null() {
            return &[];
        }
        let (leaf, pos) = self.seek(key);
        let node = &self.nodes[leaf];
        let NodeKind::Leaf { rows, .. } = &node.kind else {
            unreachable!("seek returns a leaf")
        };
        if pos < node.keys.len() && cmp_keys(&node.keys[pos], key) == Ordering::Equal {
            &rows[pos]
        } else {
            &[]
        }
    }

    /// Every row whose key falls in the range, **sorted ascending by row id**.
    ///
    /// Sorting matters for two reasons. The gather that follows reads row
    /// groups in order instead of jumping between them, and -- more
    /// importantly -- the operator above sees rows in the same order a full
    /// scan would have produced them. An index scan is meant to be a faster
    /// route to the same answer, and "same answer" includes the order of a
    /// query that never asked for one.
    pub fn range(&self, lower: &Bound, upper: &Bound) -> Vec<u32> {
        self.collect(lower, upper, usize::MAX)
            .expect("an uncapped range never gives up")
    }

    /// The same, abandoning the scan and returning `None` once more than
    /// `limit` rows qualify.
    ///
    /// This is how the planner decides whether to use the index at all: it asks
    /// for the answer, capped at the point where a full scan would have been
    /// cheaper, and takes a `None` as "too many, scan instead". The decision is
    /// then made on the true count rather than on an estimate, and the wasted
    /// work is bounded by the cap.
    pub fn range_limited(&self, lower: &Bound, upper: &Bound, limit: usize) -> Option<Vec<u32>> {
        self.collect(lower, upper, limit)
    }

    fn collect(&self, lower: &Bound, upper: &Bound, limit: usize) -> Option<Vec<u32>> {
        let (mut leaf, mut pos) = match lower {
            Bound::Unbounded => (self.first_leaf, 0),
            Bound::Included(k) | Bound::Excluded(k) => {
                let (leaf, mut pos) = self.seek(k);
                // For an exclusive bound, step past an exact hit.
                if matches!(lower, Bound::Excluded(_))
                    && pos < self.nodes[leaf].keys.len()
                    && cmp_keys(&self.nodes[leaf].keys[pos], k) == Ordering::Equal
                {
                    pos += 1;
                }
                (leaf, pos)
            }
        };

        let mut out = Vec::new();
        loop {
            let node = &self.nodes[leaf];
            let NodeKind::Leaf { rows, next } = &node.kind else {
                unreachable!("the chain holds only leaves")
            };
            while pos < node.keys.len() {
                if !in_upper_bound(&node.keys[pos], upper) {
                    out.sort_unstable();
                    return Some(out);
                }
                if out.len() + rows[pos].len() > limit {
                    return None;
                }
                out.extend_from_slice(&rows[pos]);
                pos += 1;
            }
            match next {
                Some(n) => {
                    leaf = *n;
                    pos = 0;
                }
                None => break,
            }
        }
        out.sort_unstable();
        Some(out)
    }

    /// The leaf and slot where `key` is or would be.
    fn seek(&self, key: &ScalarValue) -> (usize, usize) {
        let mut current = self.root;
        while !self.nodes[current].is_leaf() {
            let slot = self.child_slot(current, key);
            let NodeKind::Internal { children } = &self.nodes[current].kind else {
                unreachable!("loop condition guarantees an internal node")
            };
            current = children[slot];
        }
        let pos = self.nodes[current]
            .keys
            .partition_point(|k| cmp_keys(k, key) == Ordering::Less);
        (current, pos)
    }

    /// The node indices visited descending to `key`, root first.
    ///
    /// Only the index visualizer needs this -- it draws the tree and highlights
    /// the path a query took.
    pub fn path_to(&self, key: &ScalarValue) -> Vec<usize> {
        let mut path = vec![self.root];
        let mut current = self.root;
        while !self.nodes[current].is_leaf() {
            let slot = self.child_slot(current, key);
            let NodeKind::Internal { children } = &self.nodes[current].kind else {
                unreachable!("loop condition guarantees an internal node")
            };
            current = children[slot];
            path.push(current);
        }
        path
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn num_entries(&self) -> usize {
        self.num_entries
    }

    pub fn num_keys(&self) -> usize {
        self.num_keys
    }

    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn num_leaves(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_leaf()).count()
    }

    /// Rough resident size. Keys are counted shallowly except for strings,
    /// whose bytes dominate when they are the indexed column.
    pub fn byte_size(&self) -> usize {
        let mut total = 0;
        for node in &self.nodes {
            total += node.keys.capacity() * std::mem::size_of::<ScalarValue>();
            for k in &node.keys {
                if let ScalarValue::Utf8(s) = k {
                    total += s.len();
                }
            }
            match &node.kind {
                NodeKind::Internal { children } => total += children.capacity() * 8,
                NodeKind::Leaf { rows, .. } => {
                    total += rows.iter().map(|r| r.capacity() * 4 + 24).sum::<usize>()
                }
            }
        }
        total
    }

    /// A flattened description of the tree, for the index visualizer.
    ///
    /// One entry per node: where it sits, how full it is, and the range of keys
    /// it routes. Enough to draw the structure without exposing the arena or
    /// letting the UI hold references into it.
    ///
    /// A tree over a million rows has tens of thousands of nodes, which no
    /// screen wants, so `max_per_level` caps how many are returned at each
    /// depth -- evenly spaced across the level, so the sample still spans the
    /// whole key range. `keep` is always included whatever the cap, which is
    /// how the traversal path survives the sampling.
    pub fn describe(&self, max_per_level: usize, keep: &[usize]) -> Vec<NodeSummary> {
        let mut out = Vec::new();
        let mut level = vec![self.root];
        let mut depth = 0;

        while !level.is_empty() {
            let total = level.len();
            let step = total.div_ceil(max_per_level.max(1));
            for (i, &id) in level.iter().enumerate() {
                if !i.is_multiple_of(step) && !keep.contains(&id) {
                    continue;
                }
                let node = &self.nodes[id];
                out.push(NodeSummary {
                    id,
                    level: depth,
                    keys: node.keys.len(),
                    first: node.keys.first().cloned(),
                    last: node.keys.last().cloned(),
                    leaf: node.is_leaf(),
                    children: match &node.kind {
                        NodeKind::Internal { children } => children.clone(),
                        NodeKind::Leaf { .. } => Vec::new(),
                    },
                    // How many nodes this one stands in for, so the UI can say
                    // "and 40 more" rather than implying the tree is small.
                    level_total: total,
                });
            }
            let mut next = Vec::new();
            for &n in &level {
                if let NodeKind::Internal { children } = &self.nodes[n].kind {
                    next.extend_from_slice(children);
                }
            }
            level = next;
            depth += 1;
        }
        out
    }

    /// Node counts per level, root first. For the storage inspector.
    pub fn level_widths(&self) -> Vec<usize> {
        let mut widths = Vec::new();
        let mut level = vec![self.root];
        while !level.is_empty() {
            widths.push(level.len());
            let mut next = Vec::new();
            for &n in &level {
                if let NodeKind::Internal { children } = &self.nodes[n].kind {
                    next.extend_from_slice(children);
                }
            }
            level = next;
        }
        widths
    }

    /// Check every structural invariant. Debug-only: the tests call it after
    /// thousands of inserts, where a silent violation would otherwise surface
    /// much later as a wrong query result.
    #[cfg(test)]
    fn validate(&self) {
        let depth = self.check(self.root, None, None);
        assert_eq!(depth, self.height, "height disagrees with actual depth");

        // The leaf chain must visit every key in ascending order exactly once.
        let mut leaf = Some(self.first_leaf);
        let mut seen = 0;
        let mut previous: Option<ScalarValue> = None;
        let mut leaves = 0;
        while let Some(l) = leaf {
            leaves += 1;
            let node = &self.nodes[l];
            let NodeKind::Leaf { rows, next } = &node.kind else {
                panic!("the chain must hold only leaves")
            };
            assert_eq!(node.keys.len(), rows.len());
            for k in &node.keys {
                if let Some(p) = &previous {
                    assert_eq!(cmp_keys(p, k), Ordering::Less, "chain is out of order");
                }
                previous = Some(k.clone());
                seen += 1;
            }
            leaf = *next;
        }
        assert_eq!(seen, self.num_keys, "chain lost keys");
        assert_eq!(leaves, self.num_leaves(), "unreachable leaves");
    }

    /// Recursively check one subtree lies strictly within `(low, high)` and
    /// return its depth.
    #[cfg(test)]
    fn check(&self, node: usize, low: Option<&ScalarValue>, high: Option<&ScalarValue>) -> usize {
        let n = &self.nodes[node];
        assert!(
            n.keys.len() <= NODE_CAPACITY,
            "node {node} is over capacity"
        );
        for w in n.keys.windows(2) {
            assert_eq!(cmp_keys(&w[0], &w[1]), Ordering::Less, "keys out of order");
        }
        if let Some(l) = low {
            if let Some(first) = n.keys.first() {
                assert_ne!(cmp_keys(first, l), Ordering::Less, "key below separator");
            }
        }
        if let Some(h) = high {
            if let Some(last) = n.keys.last() {
                assert_eq!(cmp_keys(last, h), Ordering::Less, "key above separator");
            }
        }
        match &n.kind {
            NodeKind::Leaf { rows, .. } => {
                assert_eq!(n.keys.len(), rows.len());
                assert!(rows.iter().all(|r| !r.is_empty()), "key with no rows");
                1
            }
            NodeKind::Internal { children } => {
                assert_eq!(children.len(), n.keys.len() + 1, "child/key mismatch");
                assert!(!n.keys.is_empty(), "internal node with no separator");
                let mut depth = None;
                for (i, &c) in children.iter().enumerate() {
                    let lo = if i == 0 { low } else { Some(&n.keys[i - 1]) };
                    let hi = if i == n.keys.len() {
                        high
                    } else {
                        Some(&n.keys[i])
                    };
                    let d = self.check(c, lo, hi);
                    // Every leaf at the same depth is the balance invariant --
                    // the thing that makes lookups uniformly cheap.
                    match depth {
                        None => depth = Some(d),
                        Some(prev) => assert_eq!(prev, d, "unbalanced tree"),
                    }
                }
                depth.unwrap() + 1
            }
        }
    }
}

/// One node, as the index visualizer needs it.
#[derive(Debug, Clone)]
pub struct NodeSummary {
    pub id: usize,
    /// Distance from the root.
    pub level: usize,
    pub keys: usize,
    pub first: Option<ScalarValue>,
    pub last: Option<ScalarValue>,
    pub leaf: bool,
    pub children: Vec<usize>,
    /// How many nodes are on this level in total, sampled or not.
    pub level_total: usize,
}

/// Compare two index keys.
///
/// `ScalarValue::compare` returns `None` for UNKNOWN, which here means a NULL
/// or a type mismatch. Neither can reach the tree: NULLs are refused at
/// `insert`, and the planner only builds an index over one column's own type.
/// Treating the impossible case as `Equal` keeps the search total instead of
/// panicking deep inside a descent.
fn cmp_keys(a: &ScalarValue, b: &ScalarValue) -> Ordering {
    compare(a, b).unwrap_or(Ordering::Equal)
}

fn in_upper_bound(key: &ScalarValue, upper: &Bound) -> bool {
    match upper {
        Bound::Unbounded => true,
        Bound::Included(k) => cmp_keys(key, k) != Ordering::Greater,
        Bound::Excluded(k) => cmp_keys(key, k) == Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i64) -> ScalarValue {
        ScalarValue::Int64(i)
    }

    /// A deterministic shuffle, so a failure is reproducible without a
    /// dependency on a random-number crate.
    fn scrambled(n: u32) -> Vec<u32> {
        let mut v: Vec<u32> = (0..n).collect();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for i in (1..v.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            v.swap(i, (state % (i as u64 + 1)) as usize);
        }
        v
    }

    #[test]
    fn an_empty_tree_finds_nothing() {
        let t = BPlusTree::new();
        assert!(t.lookup(&int(1)).is_empty());
        assert!(t.range(&Bound::Unbounded, &Bound::Unbounded).is_empty());
        assert_eq!(t.height(), 1);
        t.validate();
    }

    #[test]
    fn stays_balanced_through_thousands_of_splits() {
        let mut t = BPlusTree::new();
        for row in scrambled(20_000) {
            t.insert(int(row as i64), row);
        }
        t.validate();
        assert_eq!(t.num_keys(), 20_000);
        assert_eq!(t.num_entries(), 20_000);
        // log_64(20000) is a little under 3, so with splits landing nodes
        // half-full the tree should be three or four levels -- never a chain.
        assert!(
            (3..=4).contains(&t.height()),
            "height was {}",
            t.height()
        );
    }

    #[test]
    fn ascending_inserts_split_the_same_way() {
        // The pathological case for a naive split: every insert lands at the
        // right edge.
        let mut t = BPlusTree::new();
        for i in 0..5_000u32 {
            t.insert(int(i as i64), i);
        }
        t.validate();
        for i in 0..5_000u32 {
            assert_eq!(t.lookup(&int(i as i64)), &[i], "lost {i}");
        }
    }

    #[test]
    fn descending_inserts_split_the_same_way() {
        let mut t = BPlusTree::new();
        for i in (0..5_000u32).rev() {
            t.insert(int(i as i64), i);
        }
        t.validate();
        assert_eq!(t.num_keys(), 5_000);
        let all = t.range(&Bound::Unbounded, &Bound::Unbounded);
        assert_eq!(all, (0..5_000).collect::<Vec<u32>>());
    }

    #[test]
    fn duplicate_keys_collect_every_row() {
        let mut t = BPlusTree::new();
        for row in 0..3_000u32 {
            t.insert(int((row % 50) as i64), row);
        }
        t.validate();
        assert_eq!(t.num_keys(), 50);
        assert_eq!(t.num_entries(), 3_000);
        let rows = t.lookup(&int(7));
        assert_eq!(rows.len(), 60);
        assert!(rows.iter().all(|r| r % 50 == 7));
    }

    #[test]
    fn ranges_agree_with_a_linear_filter() {
        let keys: Vec<i64> = scrambled(4_000).iter().map(|r| (*r as i64) % 900).collect();
        let mut t = BPlusTree::new();
        for (row, key) in keys.iter().enumerate() {
            t.insert(int(*key), row as u32);
        }
        t.validate();

        let brute = |f: &dyn Fn(i64) -> bool| -> Vec<u32> {
            keys.iter()
                .enumerate()
                .filter(|(_, k)| f(**k))
                .map(|(i, _)| i as u32)
                .collect()
        };

        /// A range and the predicate it is supposed to be equivalent to.
        type Case = (Bound, Bound, Box<dyn Fn(i64) -> bool>);

        let cases: Vec<Case> = vec![
            (
                Bound::Included(int(100)),
                Bound::Included(int(200)),
                Box::new(|k| (100..=200).contains(&k)),
            ),
            (
                Bound::Excluded(int(100)),
                Bound::Excluded(int(200)),
                Box::new(|k| k > 100 && k < 200),
            ),
            (
                Bound::Unbounded,
                Bound::Excluded(int(50)),
                Box::new(|k| k < 50),
            ),
            (
                Bound::Included(int(880)),
                Bound::Unbounded,
                Box::new(|k| k >= 880),
            ),
            (
                Bound::Included(int(7)),
                Bound::Included(int(7)),
                Box::new(|k| k == 7),
            ),
            // Empty and inverted ranges.
            (
                Bound::Included(int(2_000)),
                Bound::Unbounded,
                Box::new(|k| k >= 2_000),
            ),
            (
                Bound::Included(int(500)),
                Bound::Included(int(400)),
                Box::new(|_| false),
            ),
        ];

        for (lo, hi, want) in cases {
            assert_eq!(t.range(&lo, &hi), brute(&want), "range {lo:?}..{hi:?}");
        }
    }

    #[test]
    fn a_capped_range_gives_up_rather_than_collecting() {
        let mut t = BPlusTree::new();
        for row in scrambled(5_000) {
            t.insert(int(row as i64), row);
        }
        assert!(t
            .range_limited(&Bound::Unbounded, &Bound::Unbounded, 100)
            .is_none());
        let narrow = t
            .range_limited(&Bound::Included(int(10)), &Bound::Included(int(19)), 100)
            .expect("ten rows is under the cap");
        assert_eq!(narrow, (10..20).collect::<Vec<u32>>());
        // Exactly at the cap still succeeds.
        assert!(t
            .range_limited(&Bound::Included(int(0)), &Bound::Excluded(int(100)), 100)
            .is_some());
    }

    #[test]
    fn ranges_come_back_in_row_order() {
        // The operator above an index scan must see rows in the order a full
        // scan would have produced them.
        let mut t = BPlusTree::new();
        for row in scrambled(2_000) {
            t.insert(int((row % 300) as i64), row);
        }
        let rows = t.range(&Bound::Included(int(10)), &Bound::Included(int(20)));
        assert!(rows.windows(2).all(|w| w[0] < w[1]), "not sorted");
    }

    #[test]
    fn indexes_strings_and_floats() {
        let mut t = BPlusTree::new();
        for i in 0..2_000u32 {
            t.insert(ScalarValue::Utf8(format!("k{i:05}")), i);
        }
        t.validate();
        assert_eq!(t.lookup(&ScalarValue::Utf8("k01234".into())), &[1234]);
        let rows = t.range(
            &Bound::Included(ScalarValue::Utf8("k00010".into())),
            &Bound::Excluded(ScalarValue::Utf8("k00013".into())),
        );
        assert_eq!(rows, vec![10, 11, 12]);

        let mut f = BPlusTree::new();
        for i in 0..500u32 {
            f.insert(ScalarValue::Float64(i as f64 / 4.0), i);
        }
        f.validate();
        assert_eq!(f.lookup(&ScalarValue::Float64(2.5)), &[10]);
    }

    #[test]
    fn nulls_are_refused() {
        // Not an optimization -- a correctness boundary. See the module docs.
        let mut t = BPlusTree::new();
        t.insert(ScalarValue::Null, 0);
        t.insert(int(1), 1);
        assert_eq!(t.num_entries(), 1);
        assert!(t.lookup(&ScalarValue::Null).is_empty());
        assert_eq!(t.range(&Bound::Unbounded, &Bound::Unbounded), vec![1]);
    }

    #[test]
    fn describe_samples_a_wide_level_but_keeps_the_path() {
        let mut t = BPlusTree::new();
        for row in scrambled(20_000) {
            t.insert(int(row as i64), row);
        }
        let path = t.path_to(&int(9_999));
        let nodes = t.describe(8, &path);

        // Every level is represented, and none exceeds the cap by more than the
        // path nodes forced back in.
        let levels: Vec<usize> = (0..t.height())
            .map(|d| nodes.iter().filter(|n| n.level == d).count())
            .collect();
        assert_eq!(levels.len(), t.height());
        assert_eq!(levels[0], 1, "the root is alone on its level");
        assert!(levels.iter().all(|n| *n <= 8 + 1), "{levels:?}");

        // The path survived the sampling, which is the whole point.
        for id in &path {
            assert!(nodes.iter().any(|n| n.id == *id), "path node {id} was sampled away");
        }
        // And a sampled level reports how many it stood in for.
        let leaf_level = nodes.iter().find(|n| n.leaf).unwrap();
        assert!(leaf_level.level_total > 8);
    }

    #[test]
    fn the_traversal_path_is_one_node_per_level() {
        let mut t = BPlusTree::new();
        for row in scrambled(10_000) {
            t.insert(int(row as i64), row);
        }
        let path = t.path_to(&int(4_242));
        assert_eq!(path.len(), t.height());
        assert_eq!(path[0], t.root);
        assert_eq!(t.level_widths().len(), t.height());
        assert_eq!(t.level_widths()[0], 1);
    }
}
