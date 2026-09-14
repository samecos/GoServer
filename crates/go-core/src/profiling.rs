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
}

const COUNT: usize = 8;
pub const NAMES: [&str; COUNT] = [
    "selection_scores",
    "position_replay",
    "legal_moves",
    "node_recompute",
    "ancestor_refresh",
    "snapshot",
    "memory_accounting",
    "input_hash",
];

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
}

#[cfg(feature = "search-profiling")]
pub use enabled::take;
#[cfg(feature = "search-profiling")]
pub(crate) use enabled::Span;

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
