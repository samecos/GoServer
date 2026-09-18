//! Opt-in, thread-local inclusive timers. Nested stage times must not be summed.
//! Without the feature the scopes compile to no-ops.

#[derive(Clone, Copy)]
pub(crate) enum Stage {
    Selection,
    Replay,
    LegalMoves,
    Recompute,
    Ancestors,
    Snapshot,
    Memory,
    InputHash,
    NextEvaluation,
    CompleteEvaluation,
    PositionClone,
    BoardPlay,
    GraphLookup,
    GraphKey,
}

const COUNT: usize = 14;
pub const NAMES: [&str; COUNT] = [
    "selection_scores",
    "position_replay",
    "legal_moves",
    "node_recompute",
    "ancestor_refresh",
    "snapshot",
    "memory_accounting",
    "input_hash",
    "next_evaluation",
    "complete_evaluation",
    "position_clone",
    "board_play",
    "graph_index_lookup",
    "graph_key",
];

/// Window counters, independent of inclusive timing scopes. Depth is measured
/// from the current search root to a newly issued NN leaf, not PV length.
#[cfg(feature = "search-profiling")]
#[derive(Clone, Copy, Default, Debug, serde::Serialize)]
pub struct Activity {
    pub evaluate: u64,
    pub advanced: u64,
    pub waiting: u64,
    pub memory_limited: u64,
    pub depth_limited: u64,
    pub terminal: u64,
    pub errors: u64,
    pub leaf_count: u64,
    pub leaf_depth_sum: u64,
    pub leaf_depth_max: usize,
    pub ancestor_dirty_nodes: u64,
    pub ancestor_dirty_max: usize,
    pub ancestor_edges_scanned: u64,
}

#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct Sample {
    pub calls: u64,
    pub nanos: u64,
}

#[cfg(feature = "search-profiling")]
mod enabled {
    use super::*;
    use std::{cell::Cell, time::Instant};
    thread_local! {
        static TIMES: Cell<[Sample; COUNT]> = const { Cell::new([Sample { calls: 0, nanos: 0 }; COUNT]) };
        static ACTIVITY: Cell<Activity> = Cell::new(Activity::default());
    }
    pub struct Span(Stage, Instant);
    impl Span {
        pub fn new(stage: Stage) -> Self {
            Self(stage, Instant::now())
        }
    }
    impl Drop for Span {
        fn drop(&mut self) {
            let nanos = self.1.elapsed().as_nanos() as u64;
            TIMES.with(|times| {
                let mut samples = times.get();
                let sample = &mut samples[self.0 as usize];
                sample.calls += 1;
                sample.nanos += nanos;
                times.set(samples);
            });
        }
    }
    pub fn take() -> [Sample; COUNT] {
        TIMES.with(|times| times.replace([Sample::default(); COUNT]))
    }
    pub fn take_activity() -> Activity {
        ACTIVITY.with(|activity| activity.replace(Activity::default()))
    }
    #[cfg(feature = "search-hotspots")]
    pub(crate) fn peek_activity() -> Activity {
        ACTIVITY.with(Cell::get)
    }
    pub(crate) fn record_leaf(depth: usize) {
        #[cfg(feature = "search-hotspots")]
        super::hotspots::leaf();
        ACTIVITY.with(|activity| {
            let mut a = activity.get();
            a.leaf_count += 1;
            a.leaf_depth_sum += depth as u64;
            a.leaf_depth_max = a.leaf_depth_max.max(depth);
            activity.set(a);
        });
    }
    pub(crate) fn record_ancestor_dirty(nodes: usize) {
        ACTIVITY.with(|activity| {
            let mut a = activity.get();
            a.ancestor_dirty_nodes += nodes as u64;
            a.ancestor_dirty_max = a.ancestor_dirty_max.max(nodes);
            activity.set(a);
        });
    }
    pub(crate) fn record_ancestor_edges(edges: usize) {
        ACTIVITY.with(|activity| {
            let mut a = activity.get();
            a.ancestor_edges_scanned += edges as u64;
            activity.set(a);
        });
    }
    pub(crate) fn record_step(result: &Result<crate::SearchStep, crate::SearchError>) {
        ACTIVITY.with(|activity| {
            let mut a = activity.get();
            match result {
                Ok(crate::SearchStep::Evaluate(_)) => a.evaluate += 1,
                Ok(crate::SearchStep::Advanced) => a.advanced += 1,
                Ok(crate::SearchStep::Waiting) => a.waiting += 1,
                Ok(crate::SearchStep::MemoryLimited) => a.memory_limited += 1,
                Ok(crate::SearchStep::DepthLimited) => a.depth_limited += 1,
                Ok(crate::SearchStep::Terminal) => a.terminal += 1,
                Err(_) => a.errors += 1,
            }
            activity.set(a);
        });
    }
}

#[cfg(feature = "search-hotspots")]
pub(crate) use enabled::peek_activity;
#[cfg(feature = "search-profiling")]
pub use enabled::take;
#[cfg(feature = "search-profiling")]
pub use enabled::take_activity;
#[cfg(feature = "search-hotspots")]
pub mod hotspots;
#[cfg(feature = "search-profiling")]
pub(crate) use enabled::Span;
#[cfg(feature = "search-profiling")]
pub(crate) use enabled::{record_ancestor_dirty, record_ancestor_edges, record_leaf, record_step};

#[cfg(not(feature = "search-profiling"))]
pub(crate) struct Span;
#[cfg(not(feature = "search-profiling"))]
impl Span {
    #[inline(always)]
    pub fn new(_: Stage) -> Self {
        Self
    }
}
#[cfg(not(feature = "search-profiling"))]
impl Drop for Span {
    fn drop(&mut self) {}
}
#[cfg(not(feature = "search-profiling"))]
pub fn take() -> [Sample; COUNT] {
    [Sample::default(); COUNT]
}
