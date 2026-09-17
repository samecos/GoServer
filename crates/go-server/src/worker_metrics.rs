//! Bounded per-connection observations. No per-request logging or wall-clock subtraction.
use serde::Serialize;
use std::collections::VecDeque;

const WINDOW: usize = 2048;

#[derive(Default)]
pub(crate) struct Samples {
    values: VecDeque<f64>,
    count: u64,
}

#[derive(Clone, Debug, Serialize)]
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

    pub fn view(&self) -> Distribution {
        let mut sorted: Vec<_> = self.values.iter().copied().collect();
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsView {
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

impl Metrics {
    pub fn view(&self) -> MetricsView {
        MetricsView {
            request_bytes_enqueued: self.request_bytes_enqueued,
            request_bytes_streamed: self.request_bytes_streamed,
            result_bytes_received: self.result_bytes_received,
            result_messages_received: self.result_messages_received,
            rpc: self.rpc.view(),
            worker_elapsed: self.worker_elapsed.view(),
            worker_queue: self.worker_queue.view(),
            context: self.context.view(),
            evaluator: self.evaluator.view(),
            send_queue: self.send_queue.view(),
            actor_wait: self.actor_wait.view(),
            retired_rpc: self.retired_rpc.view(),
        }
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
}
