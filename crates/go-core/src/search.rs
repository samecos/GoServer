use crate::position::Key;
use crate::profiling::{Span, Stage};
use crate::value::{expected_score_value, value_weight_cdf};
use crate::{Color, Evaluation, Move, NodeStats, Position, SearchSimd, Terminal};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::time::Instant;
use thiserror::Error;

mod reclamation;
mod scoring;
pub use reclamation::ReclamationSnapshot;
use reclamation::{Reclaimer, RetiredGraph};

#[cfg(test)]
mod root_collection_tests;

/// Deterministic KataGo-compatible parameter profile. Optional neural uncertainty
/// weighting uses the evaluator's short-term error outputs. Dynamic score utility,
/// noise pruning, subtree bias, eval-cache and human-SL features are disabled.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub simd: SearchSimd,
    pub max_nodes: usize,
    pub max_memory_bytes: usize,
    /// Move large detached graphs to a bounded, exclusively owning drop worker.
    pub background_reclamation: bool,
    pub max_in_flight: usize,
    pub max_depth: usize,
    pub selection_work_budget: usize,
    pub cpuct_exploration: f64,
    pub cpuct_exploration_log: f64,
    pub cpuct_exploration_base: f64,
    pub fpu_reduction_max: f64,
    pub value_weight_exponent: f64,
    pub static_score_utility_factor: f64,
    pub use_uncertainty: bool,
    pub uncertainty_coeff: f64,
    pub uncertainty_exponent: f64,
    pub uncertainty_max_weight: f64,
    pub virtual_losses_per_path: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_edge_order_matches_full_sort_even_after_every_branch_is_blocked() {
        let mut seed = 0x159ad34u64;
        for count in [0, 1, 2, 19, 362] {
            for _ in 0..16 {
                let scored: Vec<_> = (0..count)
                    .map(|index| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        let score = match index % 13 {
                            0 => -0.0,
                            1 => 0.0,
                            2 => f64::NEG_INFINITY,
                            3 => f64::INFINITY,
                            _ => (seed % 31) as f64 / 7.0,
                        };
                        ScoredEdge { index, score }
                    })
                    .collect();
                let mut reference = scored.clone();
                reference.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.index.cmp(&b.index)));
                assert_eq!(
                    EdgeOrder::new(scored).collect::<Vec<_>>(),
                    reference.iter().map(|edge| edge.index).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn sparse_edges_keep_order_and_distinct_edges_when_rebound() {
        let mut node = Node::new(&Position::new(19, 7.5).unwrap());
        for i in 0..362 {
            node.edges.push(Edge {
                point: Some(i),
                prior: 0.0,
                child: None,
                visits: 0,
                in_flight: 0,
            });
        }
        for (edge, child) in [(361, 1), (7, 2), (2, 2), (7, 3), (0, 4)] {
            node.set_child(edge, child);
        }
        assert_eq!(node.linked_edges, vec![0, 2, 7, 361]);
        assert_eq!(
            node.connected_edges()
                .map(|e| e.child.unwrap())
                .collect::<Vec<_>>(),
            vec![4, 2, 3, 1]
        );
        assert!(
            std::mem::size_of::<Node>() <= 512,
            "fixed metadata charge must cover sparse index metadata"
        );
    }

    fn stats(visits: u64, q: f64, weight: f64) -> NodeStats {
        NodeStats {
            visits,
            win_loss_value: q,
            utility: q,
            utility_sq: q * q,
            weight_sum: weight,
            weight_sq_sum: weight,
            ..Default::default()
        }
    }
    fn add_node(s: &mut Search, q: f64, visits: u64) -> usize {
        let mut n = Node::new(&s.position);
        n.state = State::Expanded;
        n.stats = stats(visits, q, visits as f64);
        n.raw = Some(stats(1, q, 1.0));
        let id = s.nodes.len();
        s.nodes.push(n);
        id
    }
    fn link(s: &mut Search, parent: usize, child: usize, visits: u64, point: u16) {
        let edge = s.nodes[parent].edges.len();
        s.nodes[parent].edges.push(Edge {
            point: Some(point),
            prior: 0.5,
            child: Some(child),
            visits,
            in_flight: 0,
        });
        s.nodes[parent].set_child(edge, child);
        s.nodes[child].parents.insert(parent);
    }

    #[test]
    fn sparse_edges_survive_root_collection_and_child_id_remapping() {
        let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
        add_node(&mut s, 0.0, 1); // unreachable node is reclaimed
        let root = add_node(&mut s, 0.1, 4);
        let leaf = add_node(&mut s, 0.2, 8);
        link(&mut s, 0, root, 1, 0);
        link(&mut s, root, leaf, 1, 1);
        link(&mut s, root, leaf, 2, 2); // distinct edges must remain distinct
        s.root = root;
        s.collect_unreachable();
        assert_eq!(s.root, 0);
        assert_eq!(s.nodes.len(), 2);
        assert_eq!(
            s.nodes[0]
                .edges
                .iter()
                .filter_map(|e| e.child)
                .collect::<Vec<_>>(),
            vec![1, 1]
        );
        for n in &s.nodes {
            assert_eq!(
                n.linked_edges,
                n.edges
                    .iter()
                    .enumerate()
                    .filter_map(|(i, e)| e.child.map(|_| i as u16))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn catch_up_refresh_matches_full_refresh_with_shared_ancestors() {
        fn graph(extra_parent: bool) -> Search {
            let mut s =
                Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
            s.nodes[0].state = State::Expanded;
            s.nodes[0].raw = Some(stats(1, 0.1, 1.0));
            s.nodes[0].stats = stats(1, 0.1, 1.0);
            let parent = add_node(&mut s, -0.2, 1);
            let child = add_node(&mut s, 0.6, 100);
            link(&mut s, 0, parent, 0, 0);
            link(&mut s, parent, child, 0, 1);
            let other = add_node(&mut s, -0.4, 2);
            // Shared child is unchanged by catch-up, so it does not prevent
            // skipping the refresh. A shared changed parent does prevent it.
            link(
                &mut s,
                other,
                if extra_parent { parent } else { child },
                1,
                2,
            );
            s.recompute(other);
            s
        }
        for extra_parent in [false, true] {
            let mut baseline = graph(extra_parent);
            let mut candidate = graph(extra_parent);
            let path = [(0, 0), (1, 0)];
            assert_eq!(
                candidate.path_covers_ancestors(1, &path[..1]),
                !extra_parent
            );
            for _ in 0..40 {
                baseline.commit_path(&path);
                baseline.refresh_ancestors(1);
                candidate.commit_catch_up_path(1, &path);
                for (a, b) in baseline.nodes.iter().zip(&candidate.nodes) {
                    assert_eq!(a.stats, b.stats);
                    for (ae, be) in a.edges.iter().zip(&b.edges) {
                        assert_eq!(ae.visits, be.visits);
                        assert_eq!(ae.in_flight, be.in_flight);
                    }
                }
            }
            // Cycles / incoming root edges must never enter the shortcut.
            link(&mut candidate, 2, 0, 1, 3);
            assert!(!candidate.path_covers_ancestors(1, &path[..1]));
        }
    }

    #[test]
    fn shared_tactic_changes_other_parent_value_without_copying_visits() {
        // GraphSearch.md's counterexample: A prefers C; B discovers a better
        // tactic inside shared C. A's expectation must improve while A->C's
        // policy count stays unchanged. A running playout average fails this.
        let mut s = Search::new(
            Position::new(19, 7.5).unwrap(),
            SearchConfig {
                value_weight_exponent: 0.0,
                ..Default::default()
            },
        )
        .unwrap();
        let a = add_node(&mut s, 0.39, 31);
        let b = add_node(&mut s, 0.4, 71);
        let c = add_node(&mut s, 0.39, 100);
        link(&mut s, a, c, 30, 0);
        link(&mut s, b, c, 70, 1);
        s.recompute(a);
        let before = s.nodes[a].stats;
        let tactic = add_node(&mut s, 0.51, 199);
        s.nodes[tactic].state = State::Terminal;
        link(&mut s, c, tactic, 199, 2);
        s.nodes[c].stats.visits = 200;
        #[cfg(feature = "search-profiling")]
        crate::profiling::take_activity();
        s.refresh_ancestors(c);
        #[cfg(feature = "search-profiling")]
        {
            let activity = crate::profiling::take_activity();
            assert_eq!(activity.ancestor_dirty_nodes, 3);
            assert_eq!(activity.ancestor_dirty_max, 3);
            assert_eq!(activity.ancestor_edges_scanned, 3);
        }
        let expected_c = (0.39 + 199.0 * 0.51) / 200.0;
        let after = s.nodes[a].stats;
        assert!((after.utility - (0.39 + 30.0 * expected_c) / 31.0).abs() < 1e-12);
        assert!(after.utility > before.utility);
        assert_eq!(after.visits, before.visits);
        assert_eq!(s.nodes[a].edges[0].visits, 30);
        assert_eq!(s.nodes[b].edges[0].visits, 70);
    }

    #[test]
    fn direct_refresh_preserves_dfs_order_for_shared_nodes_and_cycles() {
        fn reference(
            s: &mut Search,
            node: usize,
            dirty: &HashSet<usize>,
            visited: &mut HashSet<usize>,
            active: &mut HashSet<usize>,
        ) {
            if visited.contains(&node) || !active.insert(node) {
                return;
            }
            let children: Vec<_> = s.nodes[node]
                .edges
                .iter()
                .filter_map(|e| e.child)
                .filter(|c| dirty.contains(c))
                .collect();
            for child in children {
                reference(s, child, dirty, visited, active);
            }
            active.remove(&node);
            visited.insert(node);
            s.recompute(node);
        }
        fn graph(cycle: bool) -> Search {
            let mut s =
                Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
            for i in 0..8 {
                add_node(&mut s, (i as f64 - 4.0) / 8.0, 30);
            }
            for (a, b) in [
                (1, 2),
                (1, 3),
                (2, 4),
                (3, 4),
                (4, 5),
                (4, 6),
                (5, 7),
                (6, 7),
                (3, 8),
            ] {
                link(&mut s, a, b, 3, b as u16);
            }
            if cycle {
                link(&mut s, 7, 2, 2, 10);
            }
            s
        }
        for cycle in [false, true] {
            for reverse in [false, true] {
                let mut expected = graph(cycle);
                let mut actual = graph(cycle);
                let dirty: HashSet<_> = (1..8).collect(); // node 8 must stay untouched
                let mut order: Vec<_> = (1..8).collect();
                if reverse {
                    order.reverse();
                }
                for _ in 0..8 {
                    let (mut ev, mut ea, mut av, mut aa) = (
                        HashSet::new(),
                        HashSet::new(),
                        HashSet::new(),
                        HashSet::new(),
                    );
                    for &node in &order {
                        reference(&mut expected, node, &dirty, &mut ev, &mut ea);
                        actual.refresh_recursive(node, &dirty, &mut av, &mut aa);
                    }
                    assert_eq!(ev, av);
                    assert!(ea.is_empty() && aa.is_empty());
                    for (a, b) in expected.nodes.iter().zip(&actual.nodes) {
                        assert_eq!(a.stats, b.stats);
                    }
                }
            }
        }
    }

    #[test]
    fn low_value_transposition_does_not_drag_parent_policy_toward_other_searches() {
        // The other GraphSearch.md counterexample: D prefers E (.56), F (.30)
        // receives 100 visits elsewhere. It still gets only D's one-edge weight.
        let mut s = Search::new(
            Position::new(19, 7.5).unwrap(),
            SearchConfig {
                value_weight_exponent: 0.0,
                ..Default::default()
            },
        )
        .unwrap();
        let d = add_node(&mut s, 0.55, 32);
        let e = add_node(&mut s, 0.56, 30);
        let f = add_node(&mut s, 0.30, 10);
        link(&mut s, d, e, 30, 0);
        link(&mut s, d, f, 1, 1);
        s.recompute(d);
        let before = s.nodes[d].stats;
        s.nodes[f].stats = stats(110, 0.30, 110.0);
        s.recompute(d);
        assert_eq!(s.nodes[d].stats.utility, before.utility);
        assert_eq!(s.nodes[d].stats.visits, before.visits);
        assert_eq!(s.nodes[d].stats.weight_sum, before.weight_sum);
        assert!(s.nodes[d].stats.weight_sq_sum < before.weight_sq_sum);
        assert!((before.utility - (0.55 + 30.0 * 0.56 + 0.30) / 32.0).abs() < 1e-12);
    }

    #[test]
    fn weighted_shared_child_uses_edge_fraction_and_squared_weight_scaling() {
        let mut s = Search::new(
            Position::new(19, 7.5).unwrap(),
            SearchConfig {
                value_weight_exponent: 0.0,
                ..Default::default()
            },
        )
        .unwrap();
        let p = add_node(&mut s, 0.1, 3);
        let c = add_node(&mut s, 0.9, 10);
        s.nodes[c].stats.weight_sum = 30.0;
        s.nodes[c].stats.weight_sq_sum = 90.0;
        link(&mut s, p, c, 2, 0);
        s.recompute(p);
        let result = s.nodes[p].stats;
        assert!((result.weight_sum - 7.0).abs() < 1e-12);
        assert!((result.utility - (0.1 + 6.0 * 0.9) / 7.0).abs() < 1e-12);
        assert!((result.weight_sq_sum - 4.6).abs() < 1e-12);
        for _ in 0..10 {
            s.recompute(p);
        }
        assert_eq!(result, s.nodes[p].stats);
    }

    #[test]
    fn value_downweight_preserves_total_weight_and_favors_current_player() {
        let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
        let p = add_node(&mut s, 0.0, 21);
        let good = add_node(&mut s, -0.8, 10);
        let bad = add_node(&mut s, 0.8, 10);
        link(&mut s, p, good, 10, 0);
        link(&mut s, p, bad, 10, 1);
        s.recompute(p);
        assert!((s.nodes[p].stats.weight_sum - 21.0).abs() < 1e-12);
        assert!(s.nodes[p].stats.utility < 0.0);
        let before = s.nodes[p].stats;
        s.recompute(p);
        assert_eq!(before, s.nodes[p].stats);
        s.nodes[p].color = Color::White;
        s.recompute(p);
        assert!(s.nodes[p].stats.utility > 0.0);
    }

    #[test]
    fn selection_uses_parent_edge_weight_not_shared_child_total_visits() {
        let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
        let p = add_node(&mut s, 0.0, 3);
        let c = add_node(&mut s, 0.0, 1000);
        let d = add_node(&mut s, 0.0, 2);
        link(&mut s, p, c, 1, 0);
        link(&mut s, p, d, 2, 1);
        assert_eq!(s.ordered_edges(p).next(), Some(0));
        s.nodes[p].edges[0].visits = 3;
        assert_eq!(s.ordered_edges(p).next(), Some(1));
    }

    #[test]
    fn full_graph_stops_before_catch_up_events_mask_the_limit() {
        for byte_limit in [false, true] {
            let mut s =
                Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
            s.nodes[0].state = State::Expanded;
            s.nodes[0].raw = Some(stats(1, 0.0, 1.0));
            s.nodes[0].stats = stats(1, 0.0, 1.0);
            let shared = add_node(&mut s, 0.0, 100);
            link(&mut s, 0, shared, 0, 0);
            if byte_limit {
                s.config.max_memory_bytes = s.memory_bytes() + s.node_charge() - 1;
            } else {
                s.config.max_nodes = s.nodes.len();
            }
            let before = s.snapshot(1, 1);
            for _ in 0..10 {
                assert!(matches!(
                    s.next_evaluation().unwrap(),
                    SearchStep::MemoryLimited
                ));
            }
            let after = s.snapshot(1, 1);
            assert_eq!(after.root, before.root);
            assert_eq!(after.version, before.version);
            assert_eq!(after.catch_up_visits, before.catch_up_visits);
        }
    }

    #[test]
    fn pending_memory_tracks_errors_cancellation_and_root_changes() {
        let mut s = Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
        let evaluation = Evaluation {
            policy: vec![1.0 / 362.0; 362],
            white_win_prob: 0.5,
            white_loss_prob: 0.5,
            white_no_result_prob: 0.0,
            white_score_mean: 0.0,
            white_score_mean_sq: 1.0,
            white_lead: 0.0,
            has_shortterm_error: false,
            shortterm_winloss_error: -1.0,
            shortterm_score_error: -1.0,
            ownership: vec![],
        };
        let check = |s: &Search| {
            let pending: usize = s
                .pending
                .values()
                .map(|p| s.position_charge(&p.position) * 2 + p.path.len() * 32)
                .sum();
            assert_eq!(s.pending_memory_bytes, pending);
            assert_eq!(
                s.memory_bytes(),
                s.nodes.len() * s.node_charge() + s.position_charge(&s.position) + pending
            );
        };
        let mut tasks = Vec::new();
        for _ in 0..64 {
            if let SearchStep::Evaluate(request) = s.next_evaluation().unwrap() {
                check(&s);
                if tasks.len() < 8 {
                    s.complete(request.token, evaluation.clone()).unwrap();
                } else {
                    let mut invalid = evaluation.clone();
                    invalid.policy.clear();
                    assert!(s.complete(request.token, invalid).is_err());
                }
                tasks.push(request.token);
                check(&s);
            }
        }
        assert!(s.in_flight() > 1);
        for &token in tasks.iter().rev().take(3) {
            assert_eq!(s.fail(token), Completion::Applied);
            check(&s);
            assert_eq!(s.fail(token), Completion::Stale);
            check(&s);
        }
        let &token = tasks.iter().find(|t| s.pending.contains_key(t)).unwrap();
        assert_eq!(
            s.complete(token, evaluation.clone()).unwrap(),
            Completion::Applied
        );
        check(&s);
        assert_eq!(
            s.complete(token, evaluation.clone()).unwrap(),
            Completion::Stale
        );
        check(&s);
        let cancelled = s.set_root(Position::new(19, 6.5).unwrap()).unwrap();
        assert!(!cancelled.is_empty());
        assert_eq!(s.pending_memory_bytes, 0);
        check(&s);
        for token in cancelled {
            assert_eq!(
                s.complete(token, evaluation.clone()).unwrap(),
                Completion::Stale
            );
            check(&s);
        }
    }

    #[test]
    fn pending_replay_memory_is_recoverable_and_allocated_nodes_can_retry() {
        fn evaluation() -> Evaluation {
            Evaluation {
                policy: vec![1.0 / 82.0; 82],
                white_win_prob: 0.5,
                white_loss_prob: 0.5,
                white_no_result_prob: 0.0,
                white_score_mean: 0.0,
                white_score_mean_sq: 1.0,
                white_lead: 0.0,
                has_shortterm_error: false,
                shortterm_winloss_error: -1.0,
                shortterm_score_error: -1.0,
                ownership: vec![],
            }
        }
        fn task(s: &mut Search) -> EvaluationRequest {
            match s.next_evaluation().unwrap() {
                SearchStep::Evaluate(request) => request,
                other => panic!("expected evaluation, got {other:?}"),
            }
        }
        let mut s = Search::new(Position::new(9, 7.5).unwrap(), SearchConfig::default()).unwrap();
        let root = task(&mut s);
        s.complete(root.token, evaluation()).unwrap();
        let first = task(&mut s);
        s.config.max_memory_bytes = s.memory_bytes() + s.node_charge() + 512;
        assert!(!s.graph_budget_exhausted());
        assert!(matches!(
            s.next_evaluation().unwrap(),
            SearchStep::MemoryLimited
        ));
        s.complete(first.token, evaluation()).unwrap();
        assert!(!s.graph_budget_exhausted());
        let second = task(&mut s);
        s.config.max_nodes = s.nodes.len();
        assert!(!s.graph_budget_exhausted());
        s.fail(second.token);
        assert!(!s.graph_budget_exhausted());
        let retry = task(&mut s);
        s.complete(retry.token, evaluation()).unwrap();
        assert!(s.graph_budget_exhausted());
        assert!(matches!(
            s.next_evaluation().unwrap(),
            SearchStep::MemoryLimited
        ));

        // The remaining slack can fit a bare node and a short replay, while
        // failing to fit the selected deep leaf. Allocation must be atomic and
        // must report the limit even when the root-level lower bound still fits.
        let mut deep =
            Search::new(Position::new(9, 7.5).unwrap(), SearchConfig::default()).unwrap();
        for _ in 0..8 {
            let request = task(&mut deep);
            let mut value = evaluation();
            value.policy.fill(0.0);
            value.policy[request.position.moves().len()] = 1.0;
            deep.complete(request.token, value).unwrap();
        }
        let before = deep.snapshot(1, 12);
        deep.config.max_memory_bytes = deep.memory_bytes()
            + deep.node_charge()
            + deep.position_charge(&deep.position) * 2
            + 512;
        assert!(!deep.graph_budget_exhausted());
        for _ in 0..3 {
            assert!(matches!(
                deep.next_evaluation().unwrap(),
                SearchStep::MemoryLimited
            ));
        }
        assert_eq!(deep.nodes.len(), before.nodes);
        assert!(deep.nodes.iter().all(|n| n.state == State::Expanded));
        assert_eq!(deep.snapshot(1, 12).root, before.root);
        deep.config.max_memory_bytes += 10_000;
        assert_eq!(task(&mut deep).position.moves().len(), 8);
    }
}
impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            simd: SearchSimd::Auto,
            max_nodes: 100_000_000,
            max_memory_bytes: 32 * 1024 * 1024 * 1024,
            background_reclamation: true,
            max_in_flight: 128,
            max_depth: 1000,
            selection_work_budget: 4096,
            cpuct_exploration: 1.0,
            cpuct_exploration_log: 0.0,
            cpuct_exploration_base: 500.0,
            fpu_reduction_max: 0.2,
            value_weight_exponent: 0.5,
            static_score_utility_factor: 0.3,
            use_uncertainty: false,
            uncertainty_coeff: 0.2,
            uncertainty_exponent: 1.0,
            uncertainty_max_weight: 8.0,
            virtual_losses_per_path: 3.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct EvalToken {
    pub generation: u64,
    pub id: u64,
}
#[derive(Clone, Debug)]
pub struct EvaluationRequest {
    pub token: EvalToken,
    pub input_hash: Key,
    pub position: Position,
    pub allow_terminal_search_history: bool,
    pub force_non_terminal: bool,
}
#[derive(Clone, Debug)]
// Return the owned replay without an extra allocation on every scheduled leaf.
#[allow(clippy::large_enum_variant)]
pub enum SearchStep {
    Evaluate(EvaluationRequest),
    Advanced,
    Waiting,
    MemoryLimited,
    DepthLimited,
    Terminal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    Applied,
    Stale,
}
#[derive(Debug, Error)]
pub enum SearchError {
    #[error("invalid search configuration: {0}")]
    Config(&'static str),
    #[error("invalid evaluation: {0}")]
    Evaluation(&'static str),
    #[error("root cannot fit configured graph memory budget")]
    RootBudget,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub mv: Move,
    pub prior: f64,
    pub visits: u64,
    pub weight: f64,
    pub in_flight: u32,
    pub stats: NodeStats,
    pub pv: Vec<Move>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchSnapshot {
    pub generation: u64,
    pub version: u64,
    pub root: NodeStats,
    pub candidates: Vec<Candidate>,
    pub ownership: Vec<f64>,
    pub nodes: usize,
    /// Conservative charged storage budget, not the allocator's RSS.
    pub memory_bytes: usize,
    #[serde(default)]
    pub reclamation: ReclamationSnapshot,
    #[serde(default)]
    pub root_change: Option<RootChangeMetrics>,
    pub in_flight: usize,
    pub evaluations_completed: u64,
    pub transposition_hits: u64,
    pub catch_up_visits: u64,
    pub terminal: Option<Terminal>,
}

/// Milliseconds measured on the owner thread, except destruction, which is
/// reported separately in ReclamationSnapshot. No per-playout graph traversal.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RootChangeMetrics {
    pub generation: u64,
    pub old_nodes: usize,
    pub retained_nodes: usize,
    pub cancel_ms: f64,
    pub mark_ms: f64,
    pub compact_ms: f64,
    pub relink_ms: f64,
    pub index_ms: f64,
    pub retire_ms: f64,
    pub total_ms: f64,
    pub workspace_bytes: usize,
    pub first_request_ms: Option<f64>,
    pub first_completion_ms: Option<f64>,
    pub first_analysis_ms: Option<f64>,
}

#[derive(Clone, Debug)]
struct Edge {
    point: Option<u16>,
    prior: f64,
    child: Option<usize>,
    visits: u64,
    in_flight: u32,
}

// Most descents consume only the best edge. Defer ordering the rest until a
// blocked/illegal branch requires it, retaining exactly the previous tie order.
#[derive(Clone, Copy)]
struct ScoredEdge {
    index: usize,
    score: f64,
}
impl PartialEq for ScoredEdge {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for ScoredEdge {}
impl PartialOrd for ScoredEdge {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScoredEdge {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then(other.index.cmp(&self.index))
    }
}
struct EdgeOrder {
    first: Option<usize>,
    remaining: Vec<ScoredEdge>,
    heap: Option<BinaryHeap<ScoredEdge>>,
}
impl EdgeOrder {
    fn new(scored: Vec<ScoredEdge>) -> Self {
        let best = scored
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(i, _)| i);
        Self {
            first: best,
            remaining: scored,
            heap: None,
        }
    }
}
impl Iterator for EdgeOrder {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        if let Some(best) = self.first.take() {
            return Some(self.remaining.swap_remove(best).index);
        }
        let _timer = Span::new(Stage::Selection);
        self.heap
            .get_or_insert_with(|| BinaryHeap::from(std::mem::take(&mut self.remaining)))
            .pop()
            .map(|edge| edge.index)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Unevaluated,
    Evaluating(EvalToken),
    Expanded,
    Terminal,
}
#[derive(Debug)]
struct Node {
    key: Key,
    color: Color,
    state: State,
    raw: Option<NodeStats>,
    stats: NodeStats,
    edges: Vec<Edge>,
    linked_edges: Vec<u16>,
    parents: HashSet<usize>,
    in_flight: u32,
    ownership: Vec<f64>,
    force_non_terminal: bool,
}
impl Node {
    fn new(p: &Position) -> Self {
        Self {
            key: p.graph_key(),
            color: p.to_move(),
            state: State::Unevaluated,
            raw: None,
            stats: NodeStats::default(),
            edges: Vec::new(),
            linked_edges: Vec::new(),
            parents: HashSet::new(),
            in_flight: 0,
            ownership: Vec::new(),
            force_non_terminal: false,
        }
    }
    fn set_child(&mut self, edge: usize, child: usize) {
        self.edges[edge].child = Some(child);
        {
            let edge = u16::try_from(edge).expect("19x19 edge index");
            if let Err(at) = self.linked_edges.binary_search(&edge) {
                self.linked_edges.insert(at, edge);
            }
        }
    }
    // Keep the original legal-edge order, including distinct edges sharing a
    // child. Rebinding an edge must not duplicate its index.
    fn connected_edges(&self) -> impl ExactSizeIterator<Item = &Edge> {
        self.linked_edges.iter().map(|&i| &self.edges[i as usize])
    }
}
#[derive(Clone, Debug)]
struct Pending {
    node: usize,
    path: Vec<(usize, usize)>,
    position: Position,
}

/// One owner mutates this graph. Network and GPU waits happen outside this type.
/// Each valid task owns exactly one initialization; concurrent search events
/// avoid its leaf and can continue down independent paths.
pub struct Search {
    config: SearchConfig,
    simd: SearchSimd,
    selection_scratch: Option<Box<scoring::Prepared>>,
    position: Position,
    nodes: Vec<Node>,
    index: HashMap<Key, usize>,
    root: usize,
    pending: HashMap<EvalToken, Pending>,
    pending_memory_bytes: usize,
    recompute_scratch: Vec<(NodeStats, f64)>,
    generation: u64,
    next_id: u64,
    version: u64,
    evaluations_completed: u64,
    transposition_hits: u64,
    catch_up_visits: u64,
    has_shortterm_error: Option<bool>,
    reclaimer: Reclaimer,
    root_change: Option<RootChangeMetrics>,
    root_changed_at: Option<Instant>,
}
impl Search {
    pub fn new(position: Position, config: SearchConfig) -> Result<Self, SearchError> {
        if config.max_nodes == 0
            || config.max_in_flight == 0
            || config.max_depth == 0
            || config.selection_work_budget == 0
        {
            return Err(SearchError::Config("zero capacity"));
        }
        for x in [
            config.cpuct_exploration,
            config.cpuct_exploration_log,
            config.fpu_reduction_max,
            config.value_weight_exponent,
            config.static_score_utility_factor,
            config.virtual_losses_per_path,
        ] {
            if !x.is_finite() || !(0.0..=1000.0).contains(&x) {
                return Err(SearchError::Config("negative/nonfinite coefficient"));
            }
        }
        if config.uncertainty_exponent > 2.0 {
            return Err(SearchError::Config(
                "uncertainty exponent exceeds supported range",
            ));
        }
        for x in [
            config.cpuct_exploration_base,
            config.uncertainty_coeff,
            config.uncertainty_exponent,
            config.uncertainty_max_weight,
        ] {
            if !x.is_finite() || !(0.000001..=1_000_000.0).contains(&x) {
                return Err(SearchError::Config("nonpositive coefficient"));
            }
        }
        let key = position.graph_key();
        let node = Node::new(&position);
        let simd = config.simd.resolve().map_err(SearchError::Config)?;
        let s = Self {
            simd,
            selection_scratch: (simd != SearchSimd::Scalar).then(Box::default),
            config,
            position,
            nodes: vec![node],
            index: HashMap::from([(key, 0)]),
            root: 0,
            pending: HashMap::new(),
            pending_memory_bytes: 0,
            recompute_scratch: Vec::new(),
            generation: 1,
            next_id: 1,
            version: 0,
            evaluations_completed: 0,
            transposition_hits: 0,
            catch_up_visits: 0,
            has_shortterm_error: None,
            reclaimer: Reclaimer::default(),
            root_change: None,
            root_changed_at: None,
        };
        if s.memory_bytes() > s.config.max_memory_bytes {
            return Err(SearchError::RootBudget);
        }
        Ok(s)
    }
    pub fn position(&self) -> &Position {
        &self.position
    }
    pub fn config(&self) -> &SearchConfig {
        &self.config
    }
    pub fn simd_backend(&self) -> SearchSimd {
        self.simd
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }
    pub fn root_visits(&self) -> u64 {
        self.nodes[self.root].stats.visits
    }

    /// Called by the publisher before composing a frame. An immediate retained
    /// snapshot is not counted as fresh analysis until a new result is applied.
    pub fn note_analysis_publication(&mut self) {
        if let (Some(change), Some(start)) = (&mut self.root_change, self.root_changed_at) {
            if change.first_completion_ms.is_some() && change.first_analysis_ms.is_none() {
                change.first_analysis_ms = Some(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }

    // Charge all potential edge slots (including unallocated slots), reverse
    // links, ownership and table overhead. Pending replay histories have their
    // own budget. This deliberately overestimates logical live storage.
    fn node_charge(&self) -> usize {
        self.node_payload_charge() + Self::collection_reserve_per_node()
    }
    fn node_payload_charge(&self) -> usize {
        512 + self.position.board().len() * (std::mem::size_of::<Edge>() + 64)
    }
    // Reserve temporary compaction storage as the graph grows, so a full tree
    // still has room for the dense mark/remap/stack, new node Vec and new index.
    // The stack contains each node once and may have up to 2N Vec capacity.
    fn collection_reserve_per_node() -> usize {
        2 * std::mem::size_of::<Node>() + 4 * std::mem::size_of::<usize>() + 128
    }
    fn position_charge(&self, p: &Position) -> usize {
        1024 + p.board().len() + p.moves().len() * 128
    }
    pub fn memory_bytes(&self) -> usize {
        let _timer = Span::new(Stage::Memory);
        self.active_memory_bytes()
            .saturating_add(self.reclaimer.bytes())
    }

    fn active_memory_bytes(&self) -> usize {
        self.nodes.len().saturating_mul(self.node_charge())
            + self.position_charge(&self.position)
            + self.pending_memory_bytes
    }

    /// No further NN work can fit until the root changes. Pending replay storage
    /// is transient, so it must drain before declaring this a permanent limit.
    /// Already allocated, unevaluated nodes must still be allowed to initialize.
    pub fn graph_budget_exhausted(&self) -> bool {
        if !self.pending.is_empty() || self.reclaimer.bytes() != 0 {
            return false;
        }
        let used = self.memory_bytes();
        if used.saturating_add(self.position_charge(&self.position) * 2)
            > self.config.max_memory_bytes
        {
            return true;
        }
        let cannot_grow = self.nodes.len() >= self.config.max_nodes
            || used.saturating_add(self.node_charge()) > self.config.max_memory_bytes;
        cannot_grow && self.nodes.iter().all(|n| n.state != State::Unevaluated)
    }

    pub fn next_evaluation(&mut self) -> Result<SearchStep, SearchError> {
        let _timer = Span::new(Stage::NextEvaluation);
        let retiring = self.reclaimer.bytes() != 0;
        let mut result = self.next_evaluation_inner();
        // A transient retirement budget must not latch the session actor into
        // memory_limited. Retry on its normal bounded wakeup after drops finish.
        if retiring && matches!(result, Ok(SearchStep::MemoryLimited)) {
            self.reclaimer.expedite();
            result = Ok(SearchStep::Waiting);
        }
        if matches!(result, Ok(SearchStep::Evaluate(_))) {
            if let (Some(change), Some(start)) = (&mut self.root_change, self.root_changed_at) {
                change
                    .first_request_ms
                    .get_or_insert_with(|| start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        #[cfg(feature = "search-profiling")]
        crate::profiling::record_step(&result);
        result
    }

    fn next_evaluation_inner(&mut self) -> Result<SearchStep, SearchError> {
        if let Some(outcome) = self.position.terminal() {
            if self.nodes[self.root].stats.visits == 0 {
                self.initialize_terminal(self.root, outcome);
                self.version += 1;
            }
            return Ok(SearchStep::Terminal);
        }
        // Catch-up edge events can otherwise keep returning Advanced forever
        // after the graph fills, masking the limit and starving NN dispatch.
        if self.graph_budget_exhausted() {
            return Ok(SearchStep::MemoryLimited);
        }
        if self.pending.len() >= self.config.max_in_flight {
            return Ok(SearchStep::Waiting);
        }
        let mut budget = self.config.selection_work_budget;
        let mut path = Vec::new();
        let mut active = HashSet::from([self.root]);
        let mut limited = (false, false);
        let result = self.select(
            self.root,
            self.position.clone(),
            &mut path,
            &mut active,
            &mut budget,
            &mut limited,
        );
        Ok(result.unwrap_or(if limited.0 {
            SearchStep::MemoryLimited
        } else if limited.1 {
            SearchStep::DepthLimited
        } else {
            SearchStep::Waiting
        }))
    }

    fn select(
        &mut self,
        node: usize,
        position: Position,
        path: &mut Vec<(usize, usize)>,
        active: &mut HashSet<usize>,
        budget: &mut usize,
        limited: &mut (bool, bool),
    ) -> Option<SearchStep> {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        if let Some(outcome) = position
            .terminal()
            .filter(|_| !self.nodes[node].force_non_terminal)
        {
            if self.nodes[node].state != State::Terminal {
                self.initialize_terminal(node, outcome);
            } else {
                self.nodes[node].stats.visits += 1;
                self.recompute(node);
            }
            self.commit_path(path);
            self.refresh_ancestors(node);
            self.version += 1;
            return Some(SearchStep::Advanced);
        }
        match self.nodes[node].state {
            State::Evaluating(_) => return None,
            State::Unevaluated => {
                let extra = self.position_charge(&position) * 2 + path.len() * 32;
                if self.memory_bytes().saturating_add(extra) > self.config.max_memory_bytes {
                    limited.0 = true;
                    return self.pending.is_empty().then_some(SearchStep::MemoryLimited);
                }
                let token = EvalToken {
                    generation: self.generation,
                    id: self.next_id,
                };
                self.next_id += 1;
                self.nodes[node].state = State::Evaluating(token);
                self.reserve_path(node, path, true);
                let request = EvaluationRequest {
                    token,
                    input_hash: position.input_hash(),
                    position: position.clone(),
                    allow_terminal_search_history: position.has_post_terminal_play(),
                    force_non_terminal: self.nodes[node].force_non_terminal,
                };
                self.pending.insert(
                    token,
                    Pending {
                        node,
                        path: path.clone(),
                        position,
                    },
                );
                self.pending_memory_bytes += extra;
                #[cfg(feature = "search-profiling")]
                crate::profiling::record_leaf(path.len());
                return Some(SearchStep::Evaluate(request));
            }
            State::Terminal => return None,
            State::Expanded => {}
        }
        if path.len() >= self.config.max_depth {
            limited.1 = true;
            return None;
        }
        let candidates = self.ordered_edges(node);
        for edge_idx in candidates {
            if *budget == 0 {
                break;
            }
            let m = Move {
                color: position.to_move(),
                point: self.nodes[node].edges[edge_idx].point,
            };
            let replay_timer = Span::new(Stage::Replay);
            let clone_timer = Span::new(Stage::PositionClone);
            let mut child_position = position.clone();
            drop(clone_timer);
            // GraphHash's bounded history can intentionally share representatives;
            // actual path legality/terminal status is always checked, never copied.
            let can_force_friendly =
                m.point.is_none() && position.friendly_pass_would_force_non_terminal();
            let play_timer = Span::new(Stage::BoardPlay);
            let played = child_position.play_search(m);
            drop(play_timer);
            if played.is_err() {
                continue;
            }
            let force_non_terminal = can_force_friendly && child_position.terminal().is_some();
            let key = {
                let _timer = Span::new(Stage::GraphKey);
                Self::node_key(&child_position, force_non_terminal)
            };
            drop(replay_timer);
            let child = match self.nodes[node].edges[edge_idx].child {
                Some(c) if self.nodes[c].key == key => {
                    #[cfg(feature = "search-hotspots")]
                    crate::profiling::hotspots::edge(true);
                    c
                }
                _ => {
                    #[cfg(feature = "search-hotspots")]
                    crate::profiling::hotspots::edge(false);
                    let cached = {
                        let _timer = Span::new(Stage::GraphLookup);
                        self.index.get(&key).copied()
                    };
                    let child = if let Some(c) = cached {
                        self.transposition_hits += 1;
                        c
                    } else {
                        // A new graph node is useful only if its initialization
                        // request fits too. Do not leave an unevaluated deep leaf
                        // permanently stranded by allocating its node first.
                        let replay_charge =
                            if child_position.terminal().is_some() && !force_non_terminal {
                                0
                            } else {
                                self.position_charge(&child_position) * 2 + (path.len() + 1) * 32
                            };
                        if self.nodes.len() >= self.config.max_nodes
                            || self
                                .memory_bytes()
                                .saturating_add(self.node_charge())
                                .saturating_add(replay_charge)
                                > self.config.max_memory_bytes
                        {
                            limited.0 = true;
                            // With no outstanding replay to release memory, stop
                            // at this selected frontier instead of chasing less
                            // preferred branches/catch-ups simply because they fit.
                            if self.pending.is_empty() {
                                return Some(SearchStep::MemoryLimited);
                            }
                            continue;
                        }
                        let c = self.nodes.len();
                        let mut child_node = Node::new(&child_position);
                        child_node.key = key;
                        child_node.force_non_terminal = force_non_terminal;
                        self.nodes.push(child_node);
                        self.index.insert(key, c);
                        c
                    };
                    self.nodes[node].set_child(edge_idx, child);
                    self.nodes[child].parents.insert(node);
                    child
                }
            };
            #[cfg(feature = "search-hotspots")]
            crate::profiling::hotspots::path(
                self.nodes[child].key,
                self.nodes[node].key,
                &child_position,
            );
            if matches!(self.nodes[child].state, State::Evaluating(_)) {
                continue;
            }
            path.push((node, edge_idx));
            // KataGo maybeCatchUpEdgeVisits with leak probability 0: one real
            // parent-edge event can reuse child information without a new playout.
            if self.nodes[node].edges[edge_idx].visits < self.nodes[child].stats.visits {
                self.catch_up_visits += 1;
                self.commit_catch_up_path(node, path);
                self.version += 1;
                path.pop();
                return Some(SearchStep::Advanced);
            }
            if !active.insert(child) {
                // Cycle stopping follows upstream: edge event, no duplicate child
                // visit, then idempotent recomputation with existing child stats.
                self.commit_path(path);
                self.refresh_ancestors(node);
                self.version += 1;
                path.pop();
                return Some(SearchStep::Advanced);
            }
            let result = self.select(child, child_position, path, active, budget, limited);
            active.remove(&child);
            path.pop();
            if result.is_some() {
                return result;
            }
        }
        None
    }

    fn ordered_edges(&mut self, node: usize) -> EdgeOrder {
        let _timer = Span::new(Stage::Selection);
        let n = &self.nodes[node];
        if self.simd != SearchSimd::Scalar {
            return scoring::prepared_scores(
                self.simd,
                n,
                &self.nodes,
                &self.config,
                self.selection_scratch.as_deref_mut().expect("SIMD scratch"),
            );
        }
        let total: f64 = n
            .edges
            .iter()
            .map(|e| {
                e.child
                    .map_or(0.0, |c| self.nodes[c].stats.child_weight(e.visits))
            })
            .sum();
        let mass: f64 = n
            .edges
            .iter()
            .filter(|e| e.child.is_some())
            .map(|e| e.prior)
            .sum();
        let fpu =
            n.stats.utility - n.color.white_sign() * self.config.fpu_reduction_max * mass.sqrt();
        let scale = (self.config.cpuct_exploration
            + self.config.cpuct_exploration_log
                * ((total + self.config.cpuct_exploration_base)
                    / self.config.cpuct_exploration_base)
                    .ln())
            * (total + 0.01).sqrt();
        let scored: Vec<ScoredEdge> = n
            .edges
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let (mut weight, mut utility, flights) = e.child.map_or((0.0, fpu, 0), |c| {
                    let c = &self.nodes[c];
                    let w = c.stats.child_weight(e.visits);
                    (w, if w > 0.0 { c.stats.utility } else { fpu }, c.in_flight)
                });
                if flights > 0 {
                    let virtual_weight = flights as f64 * self.config.virtual_losses_per_path;
                    let loss =
                        -n.color.white_sign() * (1.0 + self.config.static_score_utility_factor);
                    utility +=
                        (loss - utility) * virtual_weight / (virtual_weight + weight.max(0.25));
                    weight += virtual_weight;
                }
                ScoredEdge {
                    index: i,
                    score: n.color.white_sign() * utility + scale * e.prior / (1.0 + weight),
                }
            })
            .collect();
        EdgeOrder::new(scored)
    }

    /// Invalid outputs leave ownership intact so the caller can fail/retry the
    /// task deliberately; duplicates and old generations are side-effect free.
    pub fn complete(
        &mut self,
        token: EvalToken,
        evaluation: Evaluation,
    ) -> Result<Completion, SearchError> {
        let _timer = Span::new(Stage::CompleteEvaluation);
        let Some(pending) = self.pending.get(&token) else {
            return Ok(Completion::Stale);
        };
        if token.generation != self.generation
            || self.nodes[pending.node].state != State::Evaluating(token)
        {
            return Ok(Completion::Stale);
        }
        evaluation
            .validate(pending.position.board().len())
            .map_err(SearchError::Evaluation)?;
        if self
            .has_shortterm_error
            .is_some_and(|supported| supported != evaluation.has_shortterm_error)
        {
            return Err(SearchError::Evaluation(
                "inconsistent model short-term error capability",
            ));
        }
        let legal = pending.position.legal_search_moves();
        let area = pending.position.board().len();
        // Positive illegal policy mass is masked. Legal mass must be nonzero;
        // silently substituting a uniform/mock policy would hide broken workers.
        let mass: f64 = legal
            .iter()
            .map(|m| evaluation.policy[m.point.map_or(area, usize::from)].max(0.0))
            .sum();
        if mass <= 0.0 {
            return Err(SearchError::Evaluation("no positive legal policy mass"));
        }
        let p = self.pending.remove(&token).expect("validated pending task");
        self.pending_memory_bytes -= self.position_charge(&p.position) * 2 + p.path.len() * 32;
        self.has_shortterm_error = Some(evaluation.has_shortterm_error);
        let raw = self.raw_stats(&evaluation);
        let edges = legal
            .into_iter()
            .map(|m| Edge {
                point: m.point,
                prior: evaluation.policy[m.point.map_or(area, usize::from)].max(0.0) / mass,
                child: None,
                visits: 0,
                in_flight: 0,
            })
            .collect();
        self.nodes[p.node].raw = Some(raw);
        self.nodes[p.node].stats = raw;
        self.nodes[p.node].state = State::Expanded;
        self.nodes[p.node].edges = edges;
        self.nodes[p.node].linked_edges.clear();
        self.nodes[p.node].ownership = evaluation.ownership;
        self.reserve_path(p.node, &p.path, false);
        // Finish the leaf's idempotent normalization before updating its parents.
        // On an unshared chain, commit_path then updates every ancestor already.
        self.recompute(p.node);
        self.commit_path(&p.path);
        if !self.path_covers_ancestors(p.node, &p.path) {
            self.refresh_ancestors(p.node);
        }
        self.evaluations_completed += 1;
        self.version += 1;
        if let (Some(change), Some(start)) = (&mut self.root_change, self.root_changed_at) {
            change
                .first_completion_ms
                .get_or_insert_with(|| start.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(Completion::Applied)
    }
    pub fn fail(&mut self, token: EvalToken) -> Completion {
        let Some(p) = self.pending.remove(&token) else {
            return Completion::Stale;
        };
        self.pending_memory_bytes -= self.position_charge(&p.position) * 2 + p.path.len() * 32;
        if self.nodes[p.node].state == State::Evaluating(token) {
            self.nodes[p.node].state = State::Unevaluated;
        }
        self.reserve_path(p.node, &p.path, false);
        Completion::Applied
    }
    pub fn cancel_all(&mut self) -> Vec<EvalToken> {
        let tokens: Vec<_> = self.pending.keys().copied().collect();
        for t in &tokens {
            self.fail(*t);
        }
        tokens
    }
    fn reserve_path(&mut self, node: usize, path: &[(usize, usize)], reserve: bool) {
        let adjust = |v: &mut u32| {
            if reserve {
                *v += 1;
            } else {
                *v = v.checked_sub(1).expect("balanced in-flight reservation");
            }
        };
        adjust(&mut self.nodes[node].in_flight);
        for (parent, edge) in path {
            adjust(&mut self.nodes[*parent].in_flight);
            adjust(&mut self.nodes[*parent].edges[*edge].in_flight);
        }
    }
    fn commit_path(&mut self, path: &[(usize, usize)]) {
        for (parent, edge) in path.iter().rev() {
            self.nodes[*parent].edges[*edge].visits += 1;
            self.nodes[*parent].stats.visits += 1;
            self.recompute(*parent);
        }
    }

    fn commit_catch_up_path(&mut self, node: usize, path: &[(usize, usize)]) {
        #[cfg(feature = "search-hotspots")]
        let before = crate::profiling::peek_activity();
        #[cfg(feature = "search-hotspots")]
        let started = std::time::Instant::now();
        self.commit_path(path);
        // The final edge belongs to `node`: catch-up changes its edge count,
        // not the shared child's value. commit_path already recomputed node and
        // every parent on this path. Only skip the second pass when that path
        // covers ALL parents of node, including the no-parent root boundary.
        // Shared parents and cycles retain the original full graph refresh.
        let covered = cfg!(feature = "catchup-path-refresh")
            && self.path_covers_ancestors(node, &path[..path.len() - 1]);
        if !covered {
            self.refresh_ancestors(node);
        }
        #[cfg(feature = "search-hotspots")]
        {
            let nanos = started.elapsed().as_nanos() as u64;
            let after = crate::profiling::peek_activity();
            let child = self.nodes[node].edges[path.last().unwrap().1]
                .child
                .unwrap();
            crate::profiling::hotspots::catchup(
                self.nodes[child].key,
                self.nodes[node].key,
                self.nodes[child].parents.len(),
                path.len(),
                after.ancestor_dirty_nodes - before.ancestor_dirty_nodes,
                after.ancestor_edges_scanned - before.ancestor_edges_scanned,
                nanos,
            );
        }
    }
    fn raw_stats(&self, e: &Evaluation) -> NodeStats {
        let score_utility = self.config.static_score_utility_factor
            * expected_score_value(
                e.white_score_mean,
                (e.white_score_mean_sq - e.white_score_mean.powi(2))
                    .max(0.0)
                    .sqrt(),
                self.position.board_size(),
            );
        let utility = e.white_win_prob - e.white_loss_prob + score_utility;
        let weight = if self.config.use_uncertainty && e.has_shortterm_error {
            let scale = 2.0 * self.position.board_size() as f64;
            let deriv =
                self.config.static_score_utility_factor * std::f64::consts::FRAC_2_PI * scale
                    / (scale * scale + e.white_score_mean.powi(2));
            let uncertainty = e.shortterm_winloss_error + deriv * e.shortterm_score_error;
            self.config.uncertainty_coeff
                / (uncertainty.powf(self.config.uncertainty_exponent)
                    + self.config.uncertainty_coeff / self.config.uncertainty_max_weight)
        } else {
            1.0
        };
        NodeStats {
            visits: 1,
            win_loss_value: e.white_win_prob - e.white_loss_prob,
            no_result_value: e.white_no_result_prob,
            score_mean: e.white_score_mean,
            score_mean_sq: e.white_score_mean_sq,
            lead: e.white_lead,
            utility,
            utility_sq: utility * utility,
            weight_sum: weight,
            weight_sq_sum: weight * weight,
        }
    }
    fn initialize_terminal(&mut self, node: usize, outcome: Terminal) {
        let (win, loss, noresult, score, sq) = match outcome {
            Terminal::NoResult => (0.0, 0.0, 1.0, 0.0, 0.0),
            Terminal::Score {
                white_minus_black: s,
            } => (
                if s > 0.0 {
                    1.0
                } else if s < 0.0 {
                    0.0
                } else {
                    0.5
                },
                if s < 0.0 {
                    1.0
                } else if s > 0.0 {
                    0.0
                } else {
                    0.5
                },
                0.0,
                s,
                s * s + if s.fract() == 0.0 { 0.25 } else { 0.0 },
            ),
        };
        let e = Evaluation {
            policy: Vec::new(),
            white_win_prob: win,
            white_loss_prob: loss,
            white_no_result_prob: noresult,
            white_score_mean: score,
            white_score_mean_sq: sq,
            white_lead: score,
            has_shortterm_error: self.has_shortterm_error.unwrap_or(false),
            shortterm_winloss_error: 0.0,
            shortterm_score_error: 0.0,
            ownership: Vec::new(),
        };
        let raw = self.raw_stats(&e);
        let n = &mut self.nodes[node];
        n.raw = Some(raw);
        n.stats = raw;
        n.state = State::Terminal;
    }

    /// The MCGS invariant: recompute from CURRENT shared child values, using this
    /// parent's edge statistics. Never append a descendant sample to a Q sum.
    /// KataGo searchupdatehelpers.cpp::recomputeNodeStats / downweightBadChildren.
    fn recompute(&mut self, node: usize) {
        let _timer = Span::new(Stage::Recompute);
        let Some(raw) = self.nodes[node].raw else {
            return;
        };
        let visits = self.nodes[node].stats.visits;
        if self.nodes[node].state == State::Terminal {
            let mut stats = raw;
            stats.visits = visits;
            stats.weight_sum *= visits as f64;
            stats.weight_sq_sum *= visits as f64;
            self.nodes[node].stats = stats;
            return;
        }
        let sign = self.nodes[node].color.white_sign();
        // Recompute does not recurse. Keep the allocation between backups while
        // preserving child order and every floating-point operation.
        let mut children = std::mem::take(&mut self.recompute_scratch);
        children.clear();
        children.extend(
            self.nodes[node]
                .connected_edges()
                .filter_map(|e| e.child.map(|c| (self.nodes[c].stats, e.visits)))
                .filter(|(s, v)| s.visits > 0 && s.weight_sum > 0.0 && *v > 0)
                .map(|(s, v)| (s, s.child_weight(v))),
        );
        let total: f64 = children.iter().map(|x| x.1).sum();
        if self.config.value_weight_exponent != 0.0 && total > 0.0 {
            let mean = children
                .iter()
                .map(|(s, w)| sign * s.utility * w)
                .sum::<f64>()
                / total;
            for (s, w) in &mut children {
                let stdev = (1e-8 + 1.0 / (1.5 * w.sqrt())).sqrt();
                let z = (sign * s.utility - mean) / stdev;
                *w *= (value_weight_cdf(z) + 0.0001).powf(self.config.value_weight_exponent);
            }
            let norm = total / children.iter().map(|x| x.1).sum::<f64>();
            for (_, w) in &mut children {
                *w *= norm;
            }
        }
        let mut stats = NodeStats {
            visits,
            ..Default::default()
        };
        stats.add_weighted(raw, raw.weight_sum);
        for &(child, weight) in &children {
            stats.add_weighted(child, weight);
        }
        stats.normalize();
        self.nodes[node].stats = stats;
        self.recompute_scratch = children;
    }
    fn path_covers_ancestors(&self, mut child: usize, path: &[(usize, usize)]) -> bool {
        for &(parent, edge) in path.iter().rev() {
            if self.nodes[child].parents.len() != 1
                || !self.nodes[child].parents.contains(&parent)
                || self.nodes[parent].edges[edge].child != Some(child)
            {
                return false;
            }
            child = parent;
        }
        child == self.root && self.nodes[child].parents.is_empty()
    }

    // Only Q/weights are refreshed for other parents. They receive NO visits.
    fn refresh_ancestors(&mut self, changed: usize) {
        let _timer = Span::new(Stage::Ancestors);
        let mut dirty = HashSet::new();
        let mut stack = vec![changed];
        while let Some(n) = stack.pop() {
            if dirty.insert(n) {
                stack.extend(self.nodes[n].parents.iter().copied());
            }
        }
        let mut visited = HashSet::new();
        let mut active = HashSet::new();
        #[cfg(feature = "search-profiling")]
        crate::profiling::record_ancestor_dirty(dirty.len());
        for n in dirty.iter().copied() {
            self.refresh_recursive(n, &dirty, &mut visited, &mut active);
        }
    }
    fn refresh_recursive(
        &mut self,
        node: usize,
        dirty: &HashSet<usize>,
        visited: &mut HashSet<usize>,
        active: &mut HashSet<usize>,
    ) {
        // Both active and completed nodes are skipped by the reference DFS.
        // Marking on entry represents their union without changing edge order,
        // postorder recomputation, or how back edges break cycles.
        let already_seen = if cfg!(feature = "ancestor-single-set") {
            !visited.insert(node)
        } else {
            visited.contains(&node) || !active.insert(node)
        };
        if already_seen {
            return;
        }
        #[cfg(feature = "search-profiling")]
        crate::profiling::record_ancestor_edges(self.nodes[node].connected_edges().len());
        let children: Vec<_> = self.nodes[node]
            .connected_edges()
            .filter_map(|e| e.child)
            .filter(|c| dirty.contains(c))
            .collect();
        for c in children {
            self.refresh_recursive(c, dirty, visited, active);
        }
        if !cfg!(feature = "ancestor-single-set") {
            active.remove(&node);
            visited.insert(node);
        }
        self.recompute(node);
    }

    /// Switching roots invalidates every old task before reclaiming anything.
    /// Reuses reachable nodes only when board size/komi/context match. The caller
    /// must create a fresh Search for a different model/evaluation profile.
    pub fn set_root(&mut self, position: Position) -> Result<Vec<EvalToken>, SearchError> {
        let started = Instant::now();
        let minimum_charge = 512
            + position.board().len() * (std::mem::size_of::<Edge>() + 64)
            + Self::collection_reserve_per_node()
            + self.position_charge(&position);
        if minimum_charge > self.config.max_memory_bytes {
            return Err(SearchError::RootBudget);
        }
        let cancel_start = Instant::now();
        let canceled = self.cancel_all();
        self.generation += 1;
        self.root_change = Some(RootChangeMetrics {
            generation: self.generation,
            old_nodes: self.nodes.len(),
            cancel_ms: cancel_start.elapsed().as_secs_f64() * 1000.0,
            ..RootChangeMetrics::default()
        });
        self.root_changed_at = Some(started);
        let compatible = position.board_size() == self.position.board_size()
            && position.komi() == self.position.komi();
        let existing = if compatible {
            self.index.get(&position.graph_key()).copied()
        } else {
            None
        };
        self.position = position;
        if let Some(root) = existing {
            self.root = root;
            self.collect_unreachable();
        } else {
            let old_nodes = std::mem::replace(&mut self.nodes, vec![Node::new(&self.position)]);
            self.root = 0;
            let old_index = std::mem::replace(
                &mut self.index,
                HashMap::from([(self.position.graph_key(), 0)]),
            );
            self.retire_graph(old_nodes, old_index);
        }
        self.version += 1;
        if self.active_memory_bytes() > self.config.max_memory_bytes {
            // A longer root replay can leave insufficient budget for the old
            // subtree. Keep the valid root NN value and safely reclaim children.
            let mut root = self.nodes.swap_remove(self.root);
            root.edges.clear();
            root.linked_edges.clear();
            root.parents.clear();
            if let Some(raw) = root.raw {
                root.stats = raw;
            }
            let old_nodes = std::mem::replace(&mut self.nodes, vec![root]);
            self.root = 0;
            let old_index = std::mem::replace(
                &mut self.index,
                HashMap::from([(self.position.graph_key(), 0)]),
            );
            self.retire_graph(old_nodes, old_index);
        }
        let change = self.root_change.as_mut().unwrap();
        change.retained_nodes = if existing.is_some() {
            self.nodes.len()
        } else {
            0
        };
        change.total_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(canceled)
    }

    fn retire_graph(&mut self, nodes: Vec<Node>, index: HashMap<Key, usize>) {
        let start = Instant::now();
        // Node charge covers payload; separately charge the old Vec's unused
        // capacity and the detached hash table until the entire batch is gone.
        let bytes = nodes
            .len()
            .saturating_mul(self.node_payload_charge())
            .saturating_add(
                (nodes.capacity() - nodes.len()).saturating_mul(std::mem::size_of::<Node>()),
            )
            .saturating_add(index.capacity().saturating_mul(64));
        self.reclaimer.retire(
            RetiredGraph {
                nodes,
                index,
                bytes,
            },
            self.config.background_reclamation,
            self.config.max_nodes.saturating_sub(self.nodes.len()),
            self.config
                .max_memory_bytes
                .saturating_sub(self.active_memory_bytes()),
        );
        // Existing queued work may itself exceed the room left by a longer
        // replay/new root. This is an explicit memory-pressure safe point.
        if self.memory_bytes() > self.config.max_memory_bytes {
            self.reclaimer.drain();
        }
        if let Some(change) = &mut self.root_change {
            change.retire_ms += start.elapsed().as_secs_f64() * 1000.0;
        }
    }

    fn collect_unreachable(&mut self) {
        let start = Instant::now();
        const UNSEEN: usize = usize::MAX;
        const MARKED: usize = usize::MAX - 1;
        let mut remap = vec![UNSEEN; self.nodes.len()];
        let mut stack = vec![self.root];
        remap[self.root] = MARKED;
        while let Some(n) = stack.pop() {
            for &edge in &self.nodes[n].linked_edges {
                let c = self.nodes[n].edges[edge as usize]
                    .child
                    .expect("linked child");
                if remap[c] == UNSEEN {
                    remap[c] = MARKED;
                    stack.push(c);
                }
            }
        }
        let mut kept = 0;
        for id in &mut remap {
            if *id == MARKED {
                *id = kept;
                kept += 1;
            }
        }
        if let Some(change) = &mut self.root_change {
            change.mark_ms = start.elapsed().as_secs_f64() * 1000.0;
            change.workspace_bytes =
                (remap.capacity() + stack.capacity()) * std::mem::size_of::<usize>();
        }
        let start = Instant::now();
        let mut nodes = Vec::with_capacity(kept);
        // Removing in descending old-ID order leaves every earlier slot in
        // place. Only live nodes move; dead payloads stay in the old allocation.
        for old in (0..remap.len()).rev() {
            if remap[old] != UNSEEN {
                nodes.push(self.nodes.swap_remove(old));
            }
        }
        nodes.reverse();
        if let Some(change) = &mut self.root_change {
            change.compact_ms = start.elapsed().as_secs_f64() * 1000.0;
            change.workspace_bytes += nodes.capacity() * std::mem::size_of::<Node>();
        }
        let start = Instant::now();
        for n in &mut nodes {
            // Every connected child of a retained node is retained. Edge order
            // and the sparse edge index therefore stay exactly as they were.
            for &i in &n.linked_edges {
                let e = &mut n.edges[i as usize];
                e.child = e.child.map(|c| remap[c]);
            }
            n.parents.clear();
        }
        // Rebinding an edge can leave historical reverse links in parents.
        // Rebuild from forward edges, as the original collector did, rather
        // than merely filtering old parent IDs (which would preserve ghosts).
        for parent in 0..nodes.len() {
            let count = nodes[parent].linked_edges.len();
            for i in 0..count {
                let i = nodes[parent].linked_edges[i] as usize;
                if let Some(child) = nodes[parent].edges[i].child {
                    nodes[child].parents.insert(parent);
                }
            }
        }
        if let Some(change) = &mut self.root_change {
            change.relink_ms = start.elapsed().as_secs_f64() * 1000.0;
        }
        let start = Instant::now();
        self.root = remap[self.root];
        let old_index = std::mem::replace(
            &mut self.index,
            nodes.iter().enumerate().map(|(i, n)| (n.key, i)).collect(),
        );
        let old_nodes = std::mem::replace(&mut self.nodes, nodes);
        if let Some(change) = &mut self.root_change {
            change.index_ms = start.elapsed().as_secs_f64() * 1000.0;
            change.workspace_bytes += self.index.capacity() * 64;
        }
        self.retire_graph(old_nodes, old_index);
    }
    pub fn snapshot(&self, limit: usize, pv_len: usize) -> SearchSnapshot {
        let _timer = Span::new(Stage::Snapshot);
        let n = &self.nodes[self.root];
        let mut edges: Vec<_> = n.edges.iter().collect();
        edges.sort_by(|a, b| {
            self.edge_weight(b)
                .total_cmp(&self.edge_weight(a))
                .then(b.visits.cmp(&a.visits))
                .then(b.prior.total_cmp(&a.prior))
        });
        let candidates = edges
            .into_iter()
            .take(limit)
            .map(|e| {
                let mv = Move {
                    color: n.color,
                    point: e.point,
                };
                let mut pv = vec![mv];
                let mut child = e.child;
                let mut seen = HashSet::from([self.root]);
                while pv.len() < pv_len {
                    let Some(c) = child else {
                        break;
                    };
                    if !seen.insert(c) {
                        break;
                    }
                    let cn = &self.nodes[c];
                    let Some(best) = cn.edges.iter().filter(|e| e.visits > 0).max_by(|a, b| {
                        self.edge_weight(a)
                            .total_cmp(&self.edge_weight(b))
                            .then(a.visits.cmp(&b.visits))
                            .then(a.prior.total_cmp(&b.prior))
                    }) else {
                        break;
                    };
                    pv.push(Move {
                        color: cn.color,
                        point: best.point,
                    });
                    child = best.child;
                }
                if pv_len == 0 {
                    pv.clear();
                }
                Candidate {
                    mv,
                    prior: e.prior,
                    visits: e.visits,
                    weight: self.edge_weight(e),
                    in_flight: e.in_flight,
                    stats: e.child.map_or(n.stats, |c| {
                        if self.nodes[c].stats.visits > 0 {
                            self.nodes[c].stats
                        } else {
                            n.stats
                        }
                    }),
                    pv,
                }
            })
            .collect();
        SearchSnapshot {
            generation: self.generation,
            version: self.version,
            root: n.stats,
            candidates,
            ownership: n.ownership.clone(),
            nodes: self.nodes.len(),
            memory_bytes: self.memory_bytes(),
            reclamation: self.reclaimer.snapshot(),
            root_change: self.root_change.clone(),
            in_flight: self.pending.len(),
            evaluations_completed: self.evaluations_completed,
            transposition_hits: self.transposition_hits,
            catch_up_visits: self.catch_up_visits,
            terminal: self.position.terminal(),
        }
    }
    /// Existing graph continuation only; does not start work or consume visits.
    pub fn variation(&self, moves: &[Move], pv_len: usize) -> Vec<Move> {
        let mut node = self.root;
        for mv in moves {
            let n = &self.nodes[node];
            if n.color != mv.color {
                return Vec::new();
            }
            let Some(child) = n
                .edges
                .iter()
                .find(|e| e.point == mv.point)
                .and_then(|e| e.child)
            else {
                return Vec::new();
            };
            node = child;
        }
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        while result.len() < pv_len && seen.insert(node) {
            let n = &self.nodes[node];
            let Some(e) = n
                .edges
                .iter()
                .filter(|e| e.visits > 0)
                .max_by(|a, b| self.edge_weight(a).total_cmp(&self.edge_weight(b)))
            else {
                break;
            };
            result.push(Move {
                color: n.color,
                point: e.point,
            });
            let Some(c) = e.child else {
                break;
            };
            node = c;
        }
        result
    }
    fn edge_weight(&self, e: &Edge) -> f64 {
        e.child
            .map_or(0.0, |c| self.nodes[c].stats.child_weight(e.visits))
    }
    fn node_key(p: &Position, force_non_terminal: bool) -> Key {
        if !force_non_terminal {
            return p.graph_key();
        }
        let mut h = Sha256::new();
        h.update(b"go-force-friendly-pass-v1");
        h.update(p.graph_key());
        h.finalize().into()
    }
}
