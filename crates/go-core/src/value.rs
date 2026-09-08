use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// KataGo evaluator's postprocessed outputs. All value/score quantities are
/// white-positive. Policy is row-major board area followed by pass, with negative
/// values allowed only for illegal moves (as in KataGo's policy output).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evaluation {
    pub policy: Vec<f64>,
    pub white_win_prob: f64,
    pub white_loss_prob: f64,
    pub white_no_result_prob: f64,
    pub white_score_mean: f64,
    pub white_score_mean_sq: f64,
    pub white_lead: f64,
    /// False on older models; their error fields are sentinel values, not zero
    /// uncertainty. KataGo falls back to unit weight for these models.
    #[serde(default)]
    pub has_shortterm_error: bool,
    pub shortterm_winloss_error: f64,
    pub shortterm_score_error: f64,
    /// Empty when not requested, otherwise board-area white-positive values.
    #[serde(default)]
    pub ownership: Vec<f64>,
}

impl Evaluation {
    pub(crate) fn validate(&self, area: usize) -> Result<(), &'static str> {
        if self.policy.len() != area + 1 {
            return Err("policy shape");
        }
        if self
            .policy
            .iter()
            .any(|p| !p.is_finite() || *p > 1.0001 || *p < -1.0001)
        {
            return Err("policy probabilities");
        }
        let ps = [
            self.white_win_prob,
            self.white_loss_prob,
            self.white_no_result_prob,
        ];
        if ps
            .iter()
            .any(|p| !p.is_finite() || !(0.0..=1.0001).contains(p))
            || (ps.iter().sum::<f64>() - 1.0).abs() > 0.001
        {
            return Err("outcome probabilities");
        }
        if [
            self.white_score_mean,
            self.white_score_mean_sq,
            self.white_lead,
        ]
        .iter()
        .any(|x| !x.is_finite())
        {
            return Err("nonfinite value");
        }
        if self.white_score_mean.abs() > 10000.0
            || self.white_lead.abs() > 10000.0
            || self.white_score_mean_sq > 100_000_000.0
            || self.white_score_mean_sq < self.white_score_mean.powi(2) - 0.02
        {
            return Err("score moments");
        }
        if self.has_shortterm_error
            && (!(0.0..=10.0).contains(&self.shortterm_winloss_error)
                || !(0.0..=10000.0).contains(&self.shortterm_score_error))
        {
            return Err("uncertainty outside supported range");
        }
        if !(self.ownership.is_empty() || self.ownership.len() == area)
            || self
                .ownership
                .iter()
                .any(|x| !x.is_finite() || x.abs() > 1.0001)
        {
            return Err("ownership shape or range");
        }
        Ok(())
    }
}

/// Node visits are search events; edge visits are kept separately in the graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeStats {
    pub visits: u64,
    pub win_loss_value: f64,
    pub no_result_value: f64,
    pub score_mean: f64,
    pub score_mean_sq: f64,
    pub lead: f64,
    pub utility: f64,
    pub utility_sq: f64,
    pub weight_sum: f64,
    pub weight_sq_sum: f64,
}
impl NodeStats {
    pub fn white_win_prob(&self) -> f64 {
        ((1.0 - self.no_result_value + self.win_loss_value) * 0.5).clamp(0.0, 1.0)
    }
    pub fn score_stdev(&self) -> f64 {
        (self.score_mean_sq - self.score_mean.powi(2))
            .max(0.0)
            .sqrt()
    }
    pub(crate) fn child_weight(&self, edge_visits: u64) -> f64 {
        self.weight_sum * edge_visits as f64 / self.visits.max(1) as f64
    }
    pub(crate) fn add_weighted(&mut self, c: Self, desired_weight: f64) {
        self.win_loss_value += c.win_loss_value * desired_weight;
        self.no_result_value += c.no_result_value * desired_weight;
        self.score_mean += c.score_mean * desired_weight;
        self.score_mean_sq += c.score_mean_sq * desired_weight;
        self.lead += c.lead * desired_weight;
        self.utility += c.utility * desired_weight;
        self.utility_sq += c.utility_sq * desired_weight;
        self.weight_sum += desired_weight;
        self.weight_sq_sum += (desired_weight / c.weight_sum).powi(2) * c.weight_sq_sum;
    }
    pub(crate) fn normalize(&mut self) {
        if self.weight_sum <= 0.0 {
            return;
        }
        self.win_loss_value /= self.weight_sum;
        self.no_result_value /= self.weight_sum;
        self.score_mean /= self.weight_sum;
        self.score_mean_sq /= self.weight_sum;
        self.lead /= self.weight_sum;
        self.utility /= self.weight_sum;
        self.utility_sq /= self.weight_sum;
    }
}

/// Matches KataGo's 2000-cell [-50,50] Student-t(df=3) distribution table.
pub(crate) fn value_weight_cdf(z: f64) -> f64 {
    fn exact(z: f64) -> f64 {
        let x = z / 3f64.sqrt();
        0.5 + (x.atan() + x / (1.0 + x * x)) / std::f64::consts::PI
    }
    static TABLE: OnceLock<[f64; 2000]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        std::array::from_fn(|i| {
            if i == 0 {
                0.0
            } else if i >= 1999 {
                1.0
            } else {
                exact(-50.0 + i as f64 * 100.0 / 1999.0)
            }
        })
    });
    let t = ((z + 50.0) * (1999.0 / 100.0)).clamp(0.0, 1999.0);
    let i = t.floor() as usize;
    table[i] + (table[(i + 1).min(1999)] - table[i]) * (t - i as f64)
}

/// Same numeric integral and bilinear interpolation as KataGo nninputs.cpp
/// ScoreValue::expectedWhiteScoreValue (default COMPILE_MAX_BOARD_LEN=19).
/// Each integral cell is computed once on first use, with its original operation
/// order. The fixed, process-wide storage bounds memory without eagerly running
/// all 354,482 integrals before the first search. Independent cells initialize
/// concurrently; subsequent reads do not acquire a global mutex.
pub(crate) fn expected_score_value(mean: f64, stdev: f64, board_size: u8) -> f64 {
    const RADIUS: i32 = 19 * 19 + 60;
    static CELLS: [OnceLock<f64>; (RADIUS * 2 * RADIUS) as usize] =
        [const { OnceLock::new() }; (RADIUS * 2 * RADIUS) as usize];
    let factor = 19.0 / (2.0 * board_size as f64);
    let mean = mean * factor;
    let stdev = stdev * factor;
    let rounded = mean.round();
    let floored = stdev.floor();
    let m0 = (rounded as i32 + RADIUS).clamp(0, RADIUS * 2 - 1);
    let m1 = (rounded as i32 + RADIUS + 1).clamp(0, RADIUS * 2 - 1);
    let s0 = (floored as i32).clamp(0, RADIUS - 1);
    let s1 = (floored as i32 + 1).clamp(0, RADIUS - 1);
    fn cell(m: i32, s: i32) -> f64 {
        *CELLS[(m * RADIUS + s) as usize].get_or_init(|| {
            let mean = (m - RADIUS) as f64 - 0.5;
            let mut weights = 0.0;
            let mut sum = 0.0;
            for i in -50..=50 {
                let z = i as f64 / 10.0;
                let w = (-0.5 * z * z).exp();
                weights += w;
                sum += w * ((mean + s as f64 * z) / 19.0).atan() * std::f64::consts::FRAC_2_PI;
            }
            sum / weights
        })
    }
    let lm = mean - rounded + 0.5;
    let ls = stdev - floored;
    let a = cell(m0, s0) * (1.0 - ls) + cell(m0, s1) * ls;
    let b = cell(m1, s0) * (1.0 - ls) + cell(m1, s1) * ls;
    a * (1.0 - lm) + b * lm
}

#[cfg(test)]
mod tests {
    use super::*;

    // Frozen pre-cache formulas. Keep these independent from production helpers
    // so the tests detect changes to rounding, boundary handling and operation order.
    fn uncached_cdf(z: f64) -> f64 {
        fn exact(z: f64) -> f64 {
            let x = z / 3f64.sqrt();
            0.5 + (x.atan() + x / (1.0 + x * x)) / std::f64::consts::PI
        }
        let t = ((z + 50.0) * (1999.0 / 100.0)).clamp(0.0, 1999.0);
        let i = t.floor() as usize;
        let cell = |i: usize| {
            if i == 0 {
                0.0
            } else if i >= 1999 {
                1.0
            } else {
                exact(-50.0 + i as f64 * 100.0 / 1999.0)
            }
        };
        cell(i) + (cell((i + 1).min(1999)) - cell(i)) * (t - i as f64)
    }

    fn uncached_score_value(mean: f64, stdev: f64, board_size: u8) -> f64 {
        const RADIUS: i32 = 19 * 19 + 60;
        let factor = 19.0 / (2.0 * board_size as f64);
        let mean = mean * factor;
        let stdev = stdev * factor;
        let rounded = mean.round();
        let floored = stdev.floor();
        let m0 = (rounded as i32 + RADIUS).clamp(0, RADIUS * 2 - 1);
        let m1 = (rounded as i32 + RADIUS + 1).clamp(0, RADIUS * 2 - 1);
        let s0 = (floored as i32).clamp(0, RADIUS - 1);
        let s1 = (floored as i32 + 1).clamp(0, RADIUS - 1);
        fn cell(m: i32, s: i32) -> f64 {
            let mean = (m - RADIUS) as f64 - 0.5;
            let mut weights = 0.0;
            let mut sum = 0.0;
            for i in -50..=50 {
                let z = i as f64 / 10.0;
                let w = (-0.5 * z * z).exp();
                weights += w;
                sum += w * ((mean + s as f64 * z) / 19.0).atan() * std::f64::consts::FRAC_2_PI;
            }
            sum / weights
        }
        let lm = mean - rounded + 0.5;
        let ls = stdev - floored;
        let a = cell(m0, s0) * (1.0 - ls) + cell(m0, s1) * ls;
        let b = cell(m1, s0) * (1.0 - ls) + cell(m1, s1) * ls;
        a * (1.0 - lm) + b * lm
    }

    fn random(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (*seed >> 11) as f64 / (1u64 << 53) as f64
    }

    fn check_score(mean: f64, stdev: f64, size: u8) {
        assert_eq!(
            expected_score_value(mean, stdev, size).to_bits(),
            uncached_score_value(mean, stdev, size).to_bits(),
            "size={size}, mean={mean:?}, stdev={stdev:?}"
        );
    }

    #[test]
    fn cached_numeric_tables_match_original_formulas_bitwise() {
        let mut count = 0;
        for i in 0..2000 {
            let z = -50.0 + i as f64 * 100.0 / 1999.0;
            for dz in [-1e-12, 0.0, 1e-12] {
                assert_eq!(
                    value_weight_cdf(z + dz).to_bits(),
                    uncached_cdf(z + dz).to_bits()
                );
                count += 1;
            }
        }
        for z in [-10000.0, -50.0, -0.0, 0.0, 50.0, 10000.0] {
            assert_eq!(value_weight_cdf(z).to_bits(), uncached_cdf(z).to_bits());
            count += 1;
        }
        let mut score_count = 0;
        for size in 2..=19 {
            for mean in [
                -10000.0, -842.0, -841.0, -3.0, -1.0, -0.0, 0.0, 1.0, 3.0, 841.0, 842.0, 10000.0,
            ] {
                for dm in [-1e-10, 0.0, 1e-10] {
                    for stdev in [
                        0.0,
                        1e-10,
                        0.1,
                        1.0 - 1e-10,
                        1.0,
                        1.0 + 1e-10,
                        2.0,
                        15.5,
                        440.0,
                        441.0,
                        10000.0,
                    ] {
                        check_score(mean + dm, stdev, size);
                        score_count += 1;
                    }
                }
            }
        }
        let mut seed = 0x8b79_9e34_08f0_6413;
        for i in 0..8192 {
            let z = random(&mut seed) * 200.0 - 100.0;
            assert_eq!(value_weight_cdf(z).to_bits(), uncached_cdf(z).to_bits());
            count += 1;
            let mean = random(&mut seed) * 2000.0 - 1000.0;
            let stdev = random(&mut seed) * 1000.0;
            check_score(mean, stdev, 2 + (i % 18) as u8);
            score_count += 1;
        }
        eprintln!("Bitwise equal: {count} CDF inputs and {score_count} score inputs");
    }

    #[test]
    fn numeric_cells_are_consistent_across_concurrent_searches() {
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for i in 0..256 {
                        // Threads share some cells and initialize others separately.
                        let mean = 320.125 + (i % 17) as f64;
                        let stdev = 610.75 + (i % 11) as f64;
                        check_score(mean, stdev, 19);
                        check_score(mean + worker as f64 * 30.0, stdev, 19);
                        let z = -31.375 + i as f64 * 0.125;
                        assert_eq!(value_weight_cdf(z).to_bits(), uncached_cdf(z).to_bits());
                    }
                });
            }
        });
    }

    #[test]
    #[ignore = "explicit release microbenchmark; does not measure search or GPU performance"]
    fn benchmark_numeric_tables() {
        use std::hint::black_box;
        use std::time::Instant;
        if cfg!(debug_assertions) {
            panic!("run this microbenchmark with --release");
        }
        let mut seed = 0x3109_a2bd_4f11_0584;
        let inputs: Vec<_> = (0..4096)
            .map(|_| {
                (
                    random(&mut seed) * 120.0 - 60.0,
                    1.0 + random(&mut seed) * 30.0,
                )
            })
            .collect();
        let cdf_inputs: Vec<_> = (0..4096).map(|_| random(&mut seed) * 12.0 - 6.0).collect();
        let cold = Instant::now();
        for &(mean, stdev) in &inputs {
            black_box(expected_score_value(
                black_box(mean),
                black_box(stdev),
                black_box(19),
            ));
        }
        for &z in &cdf_inputs {
            black_box(value_weight_cdf(black_box(z)));
        }
        let cold_ms = cold.elapsed().as_secs_f64() * 1000.0;
        let score_iterations = 50_000;
        let cdf_iterations = 500_000;
        let score_time = |f: fn(f64, f64, u8) -> f64| {
            let start = Instant::now();
            let mut sum = 0.0;
            for i in 0..score_iterations {
                let (mean, stdev) = inputs[i % inputs.len()];
                sum += black_box(f(black_box(mean), black_box(stdev), black_box(19)));
            }
            (start.elapsed().as_secs_f64() * 1000.0, black_box(sum))
        };
        let cdf_time = |f: fn(f64) -> f64| {
            let start = Instant::now();
            let mut sum = 0.0;
            for i in 0..cdf_iterations {
                sum += black_box(f(black_box(cdf_inputs[i % cdf_inputs.len()])));
            }
            (start.elapsed().as_secs_f64() * 1000.0, black_box(sum))
        };
        let mut rounds = Vec::new();
        for round in 0..3 {
            let (uncached_score, cached_score, uncached_cdf, cached_cdf) = if round % 2 == 0 {
                (
                    score_time(uncached_score_value),
                    score_time(expected_score_value),
                    cdf_time(uncached_cdf),
                    cdf_time(value_weight_cdf),
                )
            } else {
                let cached_score = score_time(expected_score_value);
                let uncached_score = score_time(uncached_score_value);
                let cached_cdf = cdf_time(value_weight_cdf);
                let uncached_cdf = cdf_time(uncached_cdf);
                (uncached_score, cached_score, uncached_cdf, cached_cdf)
            };
            assert_eq!(uncached_score.1.to_bits(), cached_score.1.to_bits());
            assert_eq!(uncached_cdf.1.to_bits(), cached_cdf.1.to_bits());
            rounds.push(serde_json::json!({
                "round": round + 1,
                "uncachedScoreMs": uncached_score.0,
                "cachedScoreMs": cached_score.0,
                "uncachedCdfMs": uncached_cdf.0,
                "cachedCdfMs": cached_cdf.0,
            }));
        }
        eprintln!(
            "{}",
            serde_json::json!({
                "benchmark": "numeric-table-functions-only",
                "inputCount": inputs.len(),
                "coldCachePopulateMs": cold_ms,
                "scoreIterationsPerRound": score_iterations,
                "cdfIterationsPerRound": cdf_iterations,
                "scoreCacheStorageBytes": 2 * (19 * 19 + 60) * (19 * 19 + 60) * std::mem::size_of::<OnceLock<f64>>(),
                "rounds": rounds,
            })
        );
    }
}
