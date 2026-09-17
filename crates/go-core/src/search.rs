use crate::position::Key;
use crate::profiling::{Span, Stage};
use crate::value::{expected_score_value, value_weight_cdf};
use crate::{Color, Evaluation, Move, NodeStats, Position, SearchSimd, Terminal};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use thiserror::Error;

mod scoring;

/// Deterministic KataGo-compatible parameter profile. Optional neural uncertainty
/// weighting uses the evaluator's short-term error outputs. Dynamic score utility,
/// noise pruning, subtree bias, eval-cache and human-SL features are disabled.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub simd: SearchSimd,
    pub max_nodes: usize,
    pub max_memory_bytes: usize,
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
        s.nodes[parent].edges.push(Edge {
            point: Some(point),
            prior: 0.5,
            child: Some(child),
            visits,
            in_flight: 0,
        });
        s.nodes[child].parents.insert(parent);
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
        s.refresh_ancestors(c);
        let expected_c = (0.39 + 199.0 * 0.51) / 200.0;
        let after = s.nodes[a].stats;
        assert!((after.utility - (0.39 + 30.0 * expected_c) / 31.0).abs() < 1e-12);
        assert!(after.utility > before.utility);
        assert_eq!(after.visits, before.visits);
        assert_eq!(s.nodes[a].edges[0].visits, 30);
        assert_eq!(s.nodes[b].edges[0].visits, 70);
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
            max_nodes: 1_000_000,
            max_memory_bytes: 32 * 1024 * 1024 * 1024,
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
    pub in_flight: usize,
    pub evaluations_completed: u64,
    pub transposition_hits: u64,
    pub catch_up_visits: u64,
    pub terminal: Option<Terminal>,
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
            parents: HashSet::new(),
            in_flight: 0,
            ownership: Vec::new(),
            force_non_terminal: false,
        }
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

    // Charge all potential edge slots (including unallocated slots), reverse
    // links, ownership and table overhead. Pending replay histories have their
    // own budget. This deliberately overestimates logical live storage.
    fn node_charge(&self) -> usize {
        512 + self.position.board().len() * (std::mem::size_of::<Edge>() + 64)
    }
    fn position_charge(&self, p: &Position) -> usize {
        1024 + p.board().len() + p.moves().len() * 128
    }
    pub fn memory_bytes(&self) -> usize {
        let _timer = Span::new(Stage::Memory);
        self.nodes.len().saturating_mul(self.node_charge())
            + self.position_charge(&self.position)
            + self.pending_memory_bytes
    }

    /// No further NN work can fit until the root changes. Pending replay storage
    /// is transient, so it must drain before declaring this a permanent limit.
    /// Already allocated, unevaluated nodes must still be allowed to initialize.
    pub fn graph_budget_exhausted(&self) -> bool {
        if !self.pending.is_empty() {
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
            let mut child_position = position.clone();
            // GraphHash's bounded history can intentionally share representatives;
            // actual path legality/terminal status is always checked, never copied.
            let can_force_friendly =
                m.point.is_none() && position.friendly_pass_would_force_non_terminal();
            if child_position.play_search(m).is_err() {
                continue;
            }
            let force_non_terminal = can_force_friendly && child_position.terminal().is_some();
            let key = Self::node_key(&child_position, force_non_terminal);
            drop(replay_timer);
            let child = match self.nodes[node].edges[edge_idx].child {
                Some(c) if self.nodes[c].key == key => c,
                _ => {
                    let child = if let Some(c) = self.index.get(&key).copied() {
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
                    self.nodes[node].edges[edge_idx].child = Some(child);
                    self.nodes[child].parents.insert(node);
                    child
                }
            };
            if matches!(self.nodes[child].state, State::Evaluating(_)) {
                continue;
            }
            path.push((node, edge_idx));
            // KataGo maybeCatchUpEdgeVisits with leak probability 0: one real
            // parent-edge event can reuse child information without a new playout.
            if self.nodes[node].edges[edge_idx].visits < self.nodes[child].stats.visits {
                self.catch_up_visits += 1;
                self.commit_path(path);
                self.refresh_ancestors(node);
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
                .edges
                .iter()
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
        for n in dirty.iter().copied().collect::<Vec<_>>() {
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
        if visited.contains(&node) || !active.insert(node) {
            return;
        }
        let children: Vec<_> = self.nodes[node]
            .edges
            .iter()
            .filter_map(|e| e.child)
            .filter(|c| dirty.contains(c))
            .collect();
        for c in children {
            self.refresh_recursive(c, dirty, visited, active);
        }
        active.remove(&node);
        visited.insert(node);
        self.recompute(node);
    }

    /// Switching roots invalidates every old task before reclaiming anything.
    /// Reuses reachable nodes only when board size/komi/context match. The caller
    /// must create a fresh Search for a different model/evaluation profile.
    pub fn set_root(&mut self, position: Position) -> Result<Vec<EvalToken>, SearchError> {
        let minimum_charge = 512
            + position.board().len() * (std::mem::size_of::<Edge>() + 64)
            + self.position_charge(&position);
        if minimum_charge > self.config.max_memory_bytes {
            return Err(SearchError::RootBudget);
        }
        let canceled = self.cancel_all();
        self.generation += 1;
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
            self.nodes = vec![Node::new(&self.position)];
            self.root = 0;
            self.index = HashMap::from([(self.position.graph_key(), 0)]);
        }
        self.version += 1;
        if self.memory_bytes() > self.config.max_memory_bytes {
            // A longer root replay can leave insufficient budget for the old
            // subtree. Keep the valid root NN value and safely reclaim children.
            let mut root = self.nodes.swap_remove(self.root);
            root.edges.clear();
            root.parents.clear();
            if let Some(raw) = root.raw {
                root.stats = raw;
            }
            self.nodes = vec![root];
            self.root = 0;
            self.index = HashMap::from([(self.position.graph_key(), 0)]);
        }
        Ok(canceled)
    }
    fn collect_unreachable(&mut self) {
        let mut reachable = HashSet::new();
        let mut stack = vec![self.root];
        while let Some(n) = stack.pop() {
            if reachable.insert(n) {
                stack.extend(self.nodes[n].edges.iter().filter_map(|e| e.child));
            }
        }
        let mut remap = HashMap::new();
        let mut nodes = Vec::with_capacity(reachable.len());
        for (old, n) in std::mem::take(&mut self.nodes).into_iter().enumerate() {
            if reachable.contains(&old) {
                remap.insert(old, nodes.len());
                nodes.push(n);
            }
        }
        for n in &mut nodes {
            for e in &mut n.edges {
                e.child = e.child.and_then(|c| remap.get(&c).copied());
            }
            n.parents.clear();
        }
        for n in 0..nodes.len() {
            let children: Vec<_> = nodes[n].edges.iter().filter_map(|e| e.child).collect();
            for c in children {
                nodes[c].parents.insert(n);
            }
        }
        self.root = remap[&self.root];
        self.index = nodes.iter().enumerate().map(|(i, n)| (n.key, i)).collect();
        self.nodes = nodes;
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
