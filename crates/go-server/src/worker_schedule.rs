//! Experimental completion-time scheduling; physical capacity is never a speed weight.
use clap::ValueEnum;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "camelCase")]
pub enum Scheduler {
    #[default]
    Legacy,
    Completion,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulingConfig {
    pub scheduler: Scheduler,
    /// None uses Hello capacity in legacy, min(64, capacity) in completion.
    pub target_inflight: Option<usize>,
    pub worker_targets: BTreeMap<String, usize>,
}

impl SchedulingConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .target_inflight
            .is_some_and(|n| !(1..=4096).contains(&n))
            || self
                .worker_targets
                .iter()
                .any(|(id, n)| id.is_empty() || id.len() > 128 || !(1..=4096).contains(n))
        {
            return Err("worker target must be 1..=4096 with a nonempty worker id (max 128 bytes)");
        }
        Ok(())
    }
    pub fn target(&self, id: &str, capacity: usize) -> usize {
        self.worker_targets
            .get(id)
            .copied()
            .or(self.target_inflight)
            .unwrap_or(if self.scheduler == Scheduler::Completion {
                64
            } else {
                capacity
            })
            .min(capacity)
    }
}

#[derive(Default)]
pub(crate) struct CompletionModel {
    // Powers-of-two occupancy bins store observed total RPC time and occupancy.
    bins: [Option<(f64, f64)>; 13],
    pub samples: u64,
    pub throughput_rps: Option<f64>,
    interval: Option<Instant>,
    last: Option<Instant>,
    supplied_secs: f64,
    completions: u64,
}

impl CompletionModel {
    pub fn limit(&self, target: usize) -> usize {
        if self.samples < 4 {
            target.min(4)
        } else {
            target
        }
    }

    pub fn predict_ms(&self, occupancy: usize) -> f64 {
        let index = bin(occupancy);
        // Prefer the closest observed load. Scale total response time once; do not
        // add a second queue estimate on top of an already queued RPC measurement.
        self.bins
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|v| (i, v)))
            .min_by_key(|(i, _)| i.abs_diff(index))
            .map_or(occupancy as f64, |(_, (ms, load))| {
                ms * (occupancy as f64 / load).max(0.25)
            })
            .max(0.001)
    }

    pub fn observe(&mut self, occupancy: usize, ms: f64) {
        let slot = &mut self.bins[bin(occupancy)];
        *slot = Some(slot.map_or((ms, occupancy as f64), |(old_ms, old_load)| {
            (
                0.8 * old_ms + 0.2 * ms,
                0.8 * old_load + 0.2 * occupancy as f64,
            )
        }));
        self.samples += 1;
    }

    /// Report throughput only from >=1s intervals supplied >=75% of the target
    /// for >=80% of the interval. An idle interval clears the estimate, not to zero.
    pub fn traffic(
        &mut self,
        now: Instant,
        previous_inflight: usize,
        target: usize,
        completed: bool,
    ) {
        let start = *self.interval.get_or_insert(now);
        if let Some(last) = self.last.replace(now)
            && previous_inflight >= (target * 3).div_ceil(4)
        {
            self.supplied_secs += now.duration_since(last).as_secs_f64();
        }
        self.completions += u64::from(completed);
        let elapsed = now.duration_since(start);
        if elapsed >= Duration::from_secs(1) {
            self.throughput_rps = (self.supplied_secs >= elapsed.as_secs_f64() * 0.8)
                .then(|| self.completions as f64 / elapsed.as_secs_f64());
            self.interval = Some(now);
            self.supplied_secs = 0.0;
            self.completions = 0;
        }
    }
}

fn bin(n: usize) -> usize {
    (usize::BITS - n.max(1).leading_zeros() - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_is_not_capacity_and_prediction_does_not_double_count_queue() {
        let c = SchedulingConfig {
            scheduler: Scheduler::Completion,
            ..Default::default()
        };
        assert_eq!(c.target("cpp", 1024), 64);
        assert_eq!(c.target("rust", 64), 64);
        assert_eq!(c.target("small", 16), 16);
        let mut m = CompletionModel::default();
        m.observe(64, 40.0);
        assert_eq!(m.predict_ms(64), 40.0);
        assert_eq!(m.predict_ms(32), 20.0);
    }

    #[test]
    fn idle_time_is_not_a_low_speed_sample() {
        let mut m = CompletionModel::default();
        let start = Instant::now();
        m.traffic(start, 0, 64, false);
        m.traffic(start + Duration::from_secs(2), 0, 64, true);
        assert_eq!(m.throughput_rps, None);
        m.traffic(start + Duration::from_secs(3), 64, 64, true);
        assert_eq!(m.throughput_rps, Some(1.0));
    }

    #[test]
    fn overrides_are_validated_and_clipped_to_hard_capacity() {
        let mut c = SchedulingConfig {
            target_inflight: Some(64),
            worker_targets: BTreeMap::from([("a".into(), 32)]),
            ..Default::default()
        };
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.target("a", 1024), 32);
        assert_eq!(c.target("a", 8), 8);
        assert_eq!(c.target("b", 1024), 64);
        c.worker_targets.insert("bad".into(), 0);
        assert!(c.validate().is_err());
        c.worker_targets.clear();
        c.target_inflight = Some(4097);
        assert!(c.validate().is_err());
    }
}
