//! Offline-only per-graph-key attribution, drained after each replay window.
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};
type Key = [u8; 32];
#[derive(Default, Serialize)]
pub struct Hotspot {
    pub key: String,
    pub events: u64,
    pub max_parents: usize,
    pub changed_parents: usize,
    pub path_depth_sum: u64,
    pub dirty_nodes: u64,
    pub scanned_edges: u64,
    pub nanos: u64,
    #[serde(skip)]
    parents: HashSet<Key>,
}
#[derive(Default)]
struct State {
    nodes: HashMap<Key, Hotspot>,
    direct_edges: u64,
    lookups: u64,
    last: Option<Key>,
    streak: u64,
    max_streak: u64,
    burst: u64,
    max_burst: u64,
    targets: HashSet<Key>,
    sampled: HashSet<(Key, Key)>,
    paths: Vec<PathSample>,
}
#[derive(Serialize)]
pub struct PathSample {
    key: String,
    parent: String,
    moves: Vec<crate::Move>,
    board: Vec<u8>,
}
#[derive(Serialize)]
pub struct Window {
    pub nodes: Vec<Hotspot>,
    pub direct_edges: u64,
    pub lookups: u64,
    pub max_same_node_streak: u64,
    pub max_catchups_between_nn: u64,
    pub paths: Vec<PathSample>,
}
thread_local! { static STATE:RefCell<State> = RefCell::new(State::default()); }
pub fn configure_paths(keys: &str) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        for key in keys.split(',').filter(|k| !k.is_empty()) {
            assert_eq!(key.len(), 64);
            let mut bytes = [0; 32];
            for i in 0..32 {
                bytes[i] = u8::from_str_radix(&key[i * 2..i * 2 + 2], 16).unwrap();
            }
            s.targets.insert(bytes);
        }
        assert!(s.targets.len() <= 128);
    });
}
pub(crate) fn path(key: Key, parent: Key, p: &crate::Position) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.targets.contains(&key) && s.sampled.insert((key, parent)) {
            let hex = |k: Key| k.iter().map(|b| format!("{b:02x}")).collect();
            s.paths.push(PathSample {
                key: hex(key),
                parent: hex(parent),
                moves: p.moves().to_vec(),
                board: p.board().to_vec(),
            });
        }
    });
}
pub(crate) fn edge(direct: bool) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if direct {
            s.direct_edges += 1
        } else {
            s.lookups += 1
        }
    });
}
pub(crate) fn leaf() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.last = None;
        s.streak = 0;
        s.burst = 0;
    });
}
pub(crate) fn catchup(
    key: Key,
    parent: Key,
    parent_count: usize,
    depth: usize,
    dirty: u64,
    edges: u64,
    nanos: u64,
) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.streak = if s.last == Some(key) { s.streak + 1 } else { 1 };
        s.last = Some(key);
        s.max_streak = s.max_streak.max(s.streak);
        s.burst += 1;
        s.max_burst = s.max_burst.max(s.burst);
        let n = s.nodes.entry(key).or_default();
        n.events += 1;
        n.max_parents = n.max_parents.max(parent_count);
        n.parents.insert(parent);
        n.path_depth_sum += depth as u64;
        n.dirty_nodes += dirty;
        n.scanned_edges += edges;
        n.nanos += nanos;
    });
}
pub fn take() -> Window {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let mut nodes: Vec<_> = std::mem::take(&mut s.nodes)
            .into_iter()
            .map(|(k, mut n)| {
                n.key = k.iter().map(|b| format!("{b:02x}")).collect();
                n.changed_parents = n.parents.len();
                n
            })
            .collect();
        nodes.sort_by(|a, b| b.events.cmp(&a.events).then(a.key.cmp(&b.key)));
        let result = Window {
            nodes,
            direct_edges: s.direct_edges,
            lookups: s.lookups,
            max_same_node_streak: s.max_streak,
            max_catchups_between_nn: s.max_burst,
            paths: std::mem::take(&mut s.paths),
        };
        s.direct_edges = 0;
        s.lookups = 0;
        s.max_streak = 0;
        s.max_burst = 0;
        // Runs carry across window boundaries until the next new NN leaf.
        result
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counters_and_nn_boundary() {
        take();
        leaf();
        catchup([1; 32], [2; 32], 2, 3, 4, 5, 6);
        catchup([1; 32], [3; 32], 3, 4, 5, 6, 7);
        edge(true);
        edge(false);
        let w = take();
        assert_eq!(w.nodes[0].events, 2);
        assert_eq!(w.nodes[0].changed_parents, 2);
        assert_eq!(w.nodes[0].dirty_nodes, 9);
        assert_eq!(w.nodes[0].scanned_edges, 11);
        assert_eq!(w.max_same_node_streak, 2);
        assert_eq!(w.max_catchups_between_nn, 2);
        leaf();
        catchup([1; 32], [2; 32], 2, 3, 4, 5, 6);
        let w = take();
        assert_eq!(w.max_same_node_streak, 1);
        assert_eq!(w.max_catchups_between_nn, 1);
    }
}
