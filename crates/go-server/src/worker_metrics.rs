//! Bounded per-connection observations. No per-request logging or wall-clock subtraction.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{LockResult, Mutex, MutexGuard},
    time::{Duration, Instant},
};

const WINDOW: usize = 2048;
const SNAPSHOT_CACHE_TTL: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct Samples {
    values: VecDeque<f64>,
    count: u64,
}

struct SampleSnapshot {
    values: Vec<f64>,
    count: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Distribution {
    pub total_count: u64,
    pub window_count: usize,
    pub mean_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

impl Samples {
    pub fn record(&mut self, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if self.values.len() == WINDOW {
            self.values.pop_front();
        }
        self.values.push_back(ms);
        self.count += 1;
    }

    #[cfg(test)]
    fn view(&self) -> Distribution {
        self.snapshot().into_view()
    }

    fn snapshot(&self) -> SampleSnapshot {
        SampleSnapshot {
            values: self.values.iter().copied().collect(),
            count: self.count,
        }
    }
}

impl SampleSnapshot {
    fn into_view(self) -> Distribution {
        let mut sorted = self.values;
        sorted.sort_unstable_by(f64::total_cmp);
        // Nearest-rank quantiles of the most recent WINDOW observations.
        let quantile = |percent: usize| {
            sorted
                .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
                .copied()
        };
        Distribution {
            total_count: self.count,
            window_count: sorted.len(),
            mean_ms: (!sorted.is_empty()).then(|| sorted.iter().sum::<f64>() / sorted.len() as f64),
            p50_ms: quantile(50),
            p95_ms: quantile(95),
            p99_ms: quantile(99),
            max_ms: sorted.last().copied(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Metrics {
    pub request_bytes_enqueued: u64,
    pub request_bytes_streamed: u64,
    pub result_bytes_received: u64,
    pub result_messages_received: u64,
    pub rpc: Samples,
    pub worker_elapsed: Samples,
    pub worker_queue: Samples,
    pub context: Samples,
    pub evaluator: Samples,
    pub send_queue: Samples,
    pub actor_wait: Samples,
    pub retired_rpc: Samples,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsView {
    /// Age of the distribution sample snapshot; byte/message counters stay fresh.
    pub distributions_age_ms: f64,
    /// Serialized outer protobuf envelope plus 5-byte gRPC header, not TCP traffic.
    pub request_bytes_enqueued: u64,
    pub request_bytes_streamed: u64,
    pub result_bytes_received: u64,
    pub result_messages_received: u64,
    pub rpc: Distribution,
    pub worker_elapsed: Distribution,
    pub worker_queue: Distribution,
    pub context: Distribution,
    pub evaluator: Distribution,
    /// Server enqueue to tonic polling its response stream; not wire send time.
    pub send_queue: Distribution,
    /// Valid reply mailbox enqueue to actor dequeue; not graph update CPU time.
    pub actor_wait: Distribution,
    pub retired_rpc: Distribution,
}

struct CachedView {
    sampled: Instant,
    view: MetricsView,
}

/// Recording never waits for percentile sorting.
/// Readers copy the bounded rings under `raw`, then sort without that lock.
#[derive(Default)]
pub(crate) struct MetricsStore {
    raw: Mutex<Metrics>,
    snapshot_cache: Mutex<Option<CachedView>>,
}

impl MetricsStore {
    pub fn lock(&self) -> LockResult<MutexGuard<'_, Metrics>> {
        self.raw.lock()
    }

    pub fn view(&self) -> MetricsView {
        let sampled = Instant::now();
        // Each distribution is internally consistent. Read one bounded ring at
        // a time to keep its sorting buffer hot and release writers between
        // distributions; this is not an atomic snapshot across all stages.
        let mut view = MetricsView {
            rpc: self.distribution(|m| &m.rpc),
            worker_elapsed: self.distribution(|m| &m.worker_elapsed),
            worker_queue: self.distribution(|m| &m.worker_queue),
            context: self.distribution(|m| &m.context),
            evaluator: self.distribution(|m| &m.evaluator),
            send_queue: self.distribution(|m| &m.send_queue),
            actor_wait: self.distribution(|m| &m.actor_wait),
            retired_rpc: self.distribution(|m| &m.retired_rpc),
            ..Default::default()
        };
        self.refresh_counters(&mut view);
        view.distributions_age_ms = sampled.elapsed().as_secs_f64() * 1000.0;
        view
    }

    pub fn snapshot_view(&self) -> MetricsView {
        // Only readers take this lock. Coalesce concurrent game publications,
        // while keeping dispatch/result recording independent of sorting.
        let mut cache = self.snapshot_cache.lock().unwrap();
        if cache
            .as_ref()
            .is_none_or(|cached| cached.sampled.elapsed() >= SNAPSHOT_CACHE_TTL)
        {
            let sampled = Instant::now();
            *cache = Some(CachedView {
                sampled,
                view: self.view(),
            });
        }
        let cached = cache.as_ref().unwrap();
        let mut view = cached.view.clone();
        // Admission/completion boundary counters must not be cached.
        self.refresh_counters(&mut view);
        view.distributions_age_ms = cached.sampled.elapsed().as_secs_f64() * 1000.0;
        view
    }

    fn refresh_counters(&self, view: &mut MetricsView) {
        let raw = self.raw.lock().unwrap();
        view.request_bytes_enqueued = raw.request_bytes_enqueued;
        view.request_bytes_streamed = raw.request_bytes_streamed;
        view.result_bytes_received = raw.result_bytes_received;
        view.result_messages_received = raw.result_messages_received;
    }

    fn distribution(&self, select: impl FnOnce(&Metrics) -> &Samples) -> Distribution {
        let snapshot = select(&self.raw.lock().unwrap()).snapshot();
        snapshot.into_view()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_are_bounded_recent_samples_and_missing_is_not_zero() {
        let mut s = Samples::default();
        assert_eq!(s.view().p95_ms, None);
        for i in 1..=100 {
            s.record(i as f64);
        }
        assert_eq!(s.view().p50_ms, Some(50.0));
        assert_eq!(s.view().p95_ms, Some(95.0));
        s.record(f64::NAN);
        for _ in 0..WINDOW {
            s.record(0.0);
        }
        let v = s.view();
        assert_eq!(v.total_count, 100 + WINDOW as u64);
        assert_eq!(v.window_count, WINDOW);
        assert_eq!(v.p99_ms, Some(0.0));
    }

    #[test]
    fn ring_quantiles_match_chronological_window_through_wraps() {
        let mut samples = Samples::default();
        let mut history = std::collections::VecDeque::new();
        for i in 0..WINDOW * 3 + 37 {
            let value = ((i * 137) % 2048) as f64 / 100.0;
            samples.record(value);
            history.push_back(value);
            if history.len() > WINDOW {
                history.pop_front();
            }
            if i % 97 == 0 || i + 1 == WINDOW * 3 + 37 {
                let view = samples.view();
                let mut reference: Vec<_> = history.iter().copied().collect();
                reference.sort_unstable_by(f64::total_cmp);
                assert_eq!(view.total_count, (i + 1) as u64);
                assert_eq!(view.window_count, reference.len());
                assert_eq!(
                    view.mean_ms,
                    Some(reference.iter().sum::<f64>() / reference.len() as f64)
                );
                for (percent, actual) in [
                    (50, view.p50_ms),
                    (95, view.p95_ms),
                    (99, view.p99_ms),
                    (100, view.max_ms),
                ] {
                    assert_eq!(
                        actual,
                        Some(reference[(reference.len() * percent).div_ceil(100) - 1])
                    );
                }
            }
        }
    }

    #[test]
    fn cached_distributions_have_age_but_counters_and_explicit_reads_are_fresh() {
        let store = MetricsStore::default();
        store.lock().unwrap().rpc.record(2.0);
        assert_eq!(store.snapshot_view().rpc.total_count, 1);
        // Keep this cache fresh even when a debugger/loaded test runner pauses.
        store
            .snapshot_cache
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .sampled = Instant::now() + Duration::from_secs(3600);
        {
            let mut raw = store.lock().unwrap();
            raw.rpc.record(4.0);
            raw.result_messages_received = 2;
            raw.result_bytes_received = 3400;
        }
        let cached = store.snapshot_view();
        assert_eq!(cached.rpc.total_count, 1);
        assert_eq!(cached.result_messages_received, 2);
        assert_eq!(cached.result_bytes_received, 3400);
        assert!(cached.distributions_age_ms >= 0.0);
        assert_eq!(store.view().rpc.total_count, 2);
        store
            .snapshot_cache
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .sampled = Instant::now() - SNAPSHOT_CACHE_TTL;
        let refreshed = store.snapshot_view();
        assert_eq!(refreshed.rpc.total_count, 2);
        assert_eq!(refreshed.rpc.p50_ms, Some(2.0));
        assert_eq!(refreshed.rpc.p95_ms, Some(4.0));
    }
}
