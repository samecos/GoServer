//! Only detached, exclusively owned graphs cross this thread boundary. Node IDs
//! in a batch are inert integers: the worker never dereferences graph links.
use super::{Key, Node};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) struct RetiredGraph {
    pub nodes: Vec<Node>,
    pub index: HashMap<Key, usize>,
    pub bytes: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReclamationSnapshot {
    pub pending_nodes: usize,
    pub pending_bytes: usize,
    pub pending_batches: usize,
    pub peak_pending_bytes: usize,
    pub reclaimed_nodes: u64,
    pub completed_batches: u64,
    pub drop_ms: f64,
    pub backpressure_ms: f64,
    pub synchronous_batches: u64,
}

#[derive(Default)]
struct State {
    queue: VecDeque<RetiredGraph>,
    stats: ReclamationSnapshot,
    stopping: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    drained: Condvar,
    bytes: AtomicUsize,
    urgent: AtomicBool,
    #[cfg(test)]
    drop_gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

#[derive(Default)]
pub(super) struct Reclaimer {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Reclaimer {
    pub fn bytes(&self) -> usize {
        if self.worker.is_none() {
            return 0;
        }
        self.shared.bytes.load(Ordering::Acquire)
    }

    pub fn expedite(&self) {
        self.shared.urgent.store(true, Ordering::Release);
    }

    pub fn snapshot(&self) -> ReclamationSnapshot {
        self.shared.state.lock().unwrap().stats.clone()
    }

    fn start(&mut self) -> std::io::Result<()> {
        if self.worker.is_none() {
            let shared = Arc::clone(&self.shared);
            self.worker = Some(
                thread::Builder::new()
                    .name("go-graph-reclaim".into())
                    .spawn(move || {
                        loop {
                            let batch = {
                                let mut state = shared.state.lock().unwrap();
                                loop {
                                    if let Some(batch) = state.queue.pop_front() {
                                        break batch;
                                    }
                                    if state.stopping {
                                        return;
                                    }
                                    state = shared.ready.wait(state).unwrap();
                                }
                            };
                            let count = batch.nodes.len();
                            let bytes = batch.bytes;
                            #[cfg(test)]
                            if let Some((entered, release)) =
                                shared.drop_gate.lock().unwrap().take()
                            {
                                entered.send(()).unwrap();
                                release.recv().unwrap();
                            }
                            let start = Instant::now();
                            // Release the payload in bounded chunks and yield between
                            // chunks; do not hold the queue mutex during destruction.
                            let mut nodes = batch.nodes.into_iter();
                            loop {
                                for _ in 0..256 {
                                    let Some(node) = nodes.next() else { break };
                                    drop(node);
                                }
                                if nodes.len() == 0 {
                                    break;
                                }
                                if shared.urgent.load(Ordering::Acquire) {
                                    thread::yield_now();
                                } else {
                                    // yield_now alone continuously hammers the
                                    // allocator while a new search is starting.
                                    thread::sleep(Duration::from_millis(1));
                                }
                            }
                            drop(nodes);
                            drop(batch.index);
                            let mut state = shared.state.lock().unwrap();
                            state.stats.pending_nodes -= count;
                            state.stats.pending_bytes -= bytes;
                            state.stats.pending_batches -= 1;
                            state.stats.reclaimed_nodes += count as u64;
                            state.stats.completed_batches += 1;
                            state.stats.drop_ms += start.elapsed().as_secs_f64() * 1000.0;
                            shared
                                .bytes
                                .store(state.stats.pending_bytes, Ordering::Release);
                            if state.stats.pending_batches == 0 {
                                shared.urgent.store(false, Ordering::Release);
                            }
                            shared.drained.notify_all();
                        }
                    })?,
            );
        }
        Ok(())
    }

    pub fn retire(
        &mut self,
        batch: RetiredGraph,
        enabled: bool,
        max_nodes: usize,
        max_bytes: usize,
    ) {
        // Small trees do not justify starting/waking another thread. An
        // oversized batch or failed thread creation still releases ownership.
        if !enabled
            || batch.nodes.len() < 1024
            || batch.nodes.len() > max_nodes
            || batch.bytes > max_bytes
            || self.start().is_err()
        {
            let count = batch.nodes.len();
            let start = Instant::now();
            drop(batch);
            let mut state = self.shared.state.lock().unwrap();
            state.stats.reclaimed_nodes += count as u64;
            state.stats.completed_batches += 1;
            state.stats.synchronous_batches += 1;
            state.stats.drop_ms += start.elapsed().as_secs_f64() * 1000.0;
            return;
        }
        let start = Instant::now();
        let mut state = self.shared.state.lock().unwrap();
        // Include both the running batch and queued batches. Repeated root
        // changes cannot accumulate an unbounded queue or allocate more nodes.
        while state.stats.pending_batches >= 2
            || state.stats.pending_bytes.saturating_add(batch.bytes) > max_bytes
            || state.stats.pending_nodes.saturating_add(batch.nodes.len()) > max_nodes
        {
            self.expedite();
            state = self.shared.drained.wait(state).unwrap();
        }
        state.stats.backpressure_ms += start.elapsed().as_secs_f64() * 1000.0;
        state.stats.pending_nodes += batch.nodes.len();
        state.stats.pending_bytes += batch.bytes;
        state.stats.pending_batches += 1;
        state.stats.peak_pending_bytes = state
            .stats
            .peak_pending_bytes
            .max(state.stats.pending_bytes);
        self.shared
            .bytes
            .store(state.stats.pending_bytes, Ordering::Release);
        state.queue.push_back(batch);
        self.shared.ready.notify_one();
    }

    /// Used only at explicit memory-pressure/safe points and shutdown, never
    /// on the ordinary root-change path. Search allocation pauses meanwhile.
    pub fn drain(&self) {
        let start = Instant::now();
        let mut state = self.shared.state.lock().unwrap();
        while state.stats.pending_batches != 0 {
            self.expedite();
            state = self.shared.drained.wait(state).unwrap();
        }
        state.stats.backpressure_ms += start.elapsed().as_secs_f64() * 1000.0;
    }
}

impl Drop for Reclaimer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.shared.state.lock().unwrap().stopping = true;
            self.expedite();
            self.shared.ready.notify_one();
            worker.join().expect("graph reclamation worker panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;
    use std::sync::mpsc;
    use std::time::Duration;

    fn batch() -> RetiredGraph {
        let p = Position::new(19, 7.5).unwrap();
        RetiredGraph {
            nodes: (0..1024).map(|_| Node::new(&p)).collect(),
            index: HashMap::new(),
            bytes: 1_000_000,
        }
    }

    #[test]
    fn byte_backpressure_counts_running_batch_and_drop_joins() {
        let mut reclaimer = Reclaimer::default();
        let shared = Arc::clone(&reclaimer.shared);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *shared.drop_gate.lock().unwrap() = Some((entered_tx, release_rx));
        reclaimer.retire(batch(), true, 4096, 1_000_000);
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(reclaimer.bytes(), 1_000_000);
        assert_eq!(reclaimer.snapshot().pending_nodes, 1024);
        let (done_tx, done_rx) = mpsc::channel();
        let producer = thread::spawn(move || {
            reclaimer.retire(batch(), true, 4096, 1_000_000);
            // Drop must drain the second queued/running batch before returning.
            drop(reclaimer);
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        producer.join().unwrap();
        let state = shared.state.lock().unwrap();
        assert_eq!(state.stats.pending_bytes, 0);
        assert_eq!(state.stats.pending_nodes, 0);
        assert_eq!(state.stats.pending_batches, 0);
        assert_eq!(state.stats.reclaimed_nodes, 2048);
        assert_eq!(state.stats.peak_pending_bytes, 1_000_000);
        assert!(state.stats.backpressure_ms > 0.0);
        assert!(state.queue.is_empty());
    }

    #[test]
    fn temporary_retired_memory_waits_then_resumes_instead_of_latching_limit() {
        use crate::{Search, SearchConfig, SearchStep};
        let mut search =
            Search::new(Position::new(19, 7.5).unwrap(), SearchConfig::default()).unwrap();
        let active = search.memory_bytes();
        search.config.max_memory_bytes = active + 1_000_000;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *search.reclaimer.shared.drop_gate.lock().unwrap() = Some((entered_tx, release_rx));
        search.reclaimer.retire(batch(), true, 4096, 1_000_000);
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(search.memory_bytes(), active + 1_000_000);
        let step = search.next_evaluation().unwrap();
        // Release the test gate even if the assertion below detects a failure.
        release_tx.send(()).unwrap();
        search.reclaimer.drain();
        assert!(matches!(step, SearchStep::Waiting));
        assert_eq!(search.memory_bytes(), active);
        assert!(matches!(
            search.next_evaluation().unwrap(),
            SearchStep::Evaluate(_)
        ));
    }

    #[test]
    fn queue_bound_includes_the_batch_currently_being_destroyed() {
        let mut reclaimer = Reclaimer::default();
        let shared = Arc::clone(&reclaimer.shared);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *shared.drop_gate.lock().unwrap() = Some((entered_tx, release_rx));
        reclaimer.retire(batch(), true, 10_000, 10_000_000);
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        reclaimer.retire(batch(), true, 10_000, 10_000_000);
        assert_eq!(reclaimer.snapshot().pending_batches, 2);
        let (done_tx, done_rx) = mpsc::channel();
        let producer = thread::spawn(move || {
            reclaimer.retire(batch(), true, 10_000, 10_000_000);
            reclaimer.drain();
            done_tx.send(reclaimer.snapshot()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        release_tx.send(()).unwrap();
        let stats = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        producer.join().unwrap();
        assert_eq!(stats.reclaimed_nodes, 3072);
        assert_eq!(stats.pending_bytes, 0);
        assert!(stats.peak_pending_bytes <= 2_000_000);
    }
}
