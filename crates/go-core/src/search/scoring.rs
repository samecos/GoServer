//! SIMD only across independent candidates. No horizontal floating reductions,
//! approximate reciprocal, FMA or changes to EdgeOrder's total_cmp tie ordering.
use super::{EdgeOrder, Node, ScoredEdge, SearchConfig, SearchSimd};

#[derive(Clone, Copy, Default)]
struct Input {
    weight: f64,
    utility: f64,
    flights: f64,
    prior: f64,
}

fn input(n: &Node, nodes: &[Node], index: usize, fpu: f64) -> Input {
    let e = &n.edges[index];
    let (weight, utility, flights) = e.child.map_or((0.0, fpu, 0), |c| {
        let c = &nodes[c];
        let w = c.stats.child_weight(e.visits);
        (w, if w > 0.0 { c.stats.utility } else { fpu }, c.in_flight)
    });
    Input {
        weight,
        utility,
        flights: flights as f64,
        prior: e.prior,
    }
}

fn scalar(x: Input, sign: f64, scale: f64, loss: f64, virtual_loss: f64) -> f64 {
    let mut weight = x.weight;
    let mut utility = x.utility;
    if x.flights > 0.0 {
        let virtual_weight = x.flights * virtual_loss;
        utility += (loss - utility) * virtual_weight / (virtual_weight + weight.max(0.25));
        weight += virtual_weight;
    }
    sign * utility + scale * x.prior / (1.0 + weight)
}

const MAX_EDGES: usize = 19 * 19 + 1;

pub(super) struct Prepared {
    weights: [f64; MAX_EDGES],
    utilities: [f64; MAX_EDGES],
    flights: [f64; MAX_EDGES],
    priors: [f64; MAX_EDGES],
    all_priors: [f64; MAX_EDGES],
    indices: [usize; MAX_EDGES],
    all_len: usize,
    len: usize,
}

impl Default for Prepared {
    fn default() -> Self {
        Self {
            weights: [0.0; MAX_EDGES],
            utilities: [0.0; MAX_EDGES],
            flights: [0.0; MAX_EDGES],
            priors: [0.0; MAX_EDGES],
            all_priors: [0.0; MAX_EDGES],
            indices: [0; MAX_EDGES],
            all_len: 0,
            len: 0,
        }
    }
}

impl Prepared {
    fn gather(&mut self, n: &Node, nodes: &[Node]) {
        assert!(n.edges.len() <= MAX_EDGES);
        self.all_len = n.edges.len();
        self.len = 0;
        for i in 0..self.all_len {
            self.all_priors[i] = n.edges[i].prior;
            if n.edges[i].child.is_none() {
                continue;
            }
            let x = input(n, nodes, i, 0.0);
            let j = self.len;
            self.weights[j] = x.weight;
            self.utilities[j] = x.utility;
            self.flights[j] = x.flights;
            self.priors[j] = x.prior;
            self.indices[j] = i;
            self.len += 1;
        }
    }

    fn apply_fpu(&mut self, fpu: f64) {
        for (u, &w) in self.utilities[..self.len]
            .iter_mut()
            .zip(&self.weights[..self.len])
        {
            *u = if w > 0.0 { *u } else { fpu };
        }
    }

    fn at(&self, i: usize) -> Input {
        Input {
            weight: self.weights[i],
            utility: self.utilities[i],
            flights: self.flights[i],
            prior: self.priors[i],
        }
    }
}

pub(super) fn prepared_scores(
    mode: SearchSimd,
    n: &Node,
    nodes: &[Node],
    config: &SearchConfig,
    inputs: &mut Prepared,
) -> EdgeOrder {
    inputs.gather(n, nodes);
    // Child weights are nonnegative. Omitted childless edges contribute +0;
    // preserve the +0 result of a nonempty all-zero sum (empty sum is -0).
    // All nonzero terms retain their original order.
    let mut total: f64 = inputs.weights[..inputs.len].iter().copied().sum();
    if inputs.all_len > 0 {
        total += 0.0;
    }
    let mass: f64 = inputs.priors[..inputs.len].iter().copied().sum();
    let fpu = n.stats.utility - n.color.white_sign() * config.fpu_reduction_max * mass.sqrt();
    let scale = (config.cpuct_exploration
        + config.cpuct_exploration_log
            * ((total + config.cpuct_exploration_base) / config.cpuct_exploration_base).ln())
        * (total + 0.01).sqrt();
    inputs.apply_fpu(fpu);
    run(mode, n, config, inputs, fpu, scale)
}

#[cfg(test)]
fn vector_scores(
    mode: SearchSimd,
    n: &Node,
    nodes: &[Node],
    config: &SearchConfig,
    fpu: f64,
    scale: f64,
) -> EdgeOrder {
    let mut inputs = Prepared::default();
    inputs.gather(n, nodes);
    inputs.apply_fpu(fpu);
    run(mode, n, config, &mut inputs, fpu, scale)
}

fn run(
    mode: SearchSimd,
    n: &Node,
    config: &SearchConfig,
    inputs: &mut Prepared,
    fpu: f64,
    scale: f64,
) -> EdgeOrder {
    let sign = n.color.white_sign();
    let loss = -sign * (1.0 + config.static_score_utility_factor);
    #[cfg(target_arch = "x86_64")]
    let done = {
        // SAFETY: Search::new resolves/validates this immutable mode using
        // is_x86_feature_detected!, including OS vector-state support.
        unsafe {
            match mode {
                SearchSimd::Avx512 => {
                    avx512::scores(
                        inputs,
                        sign,
                        scale,
                        loss,
                        config.virtual_losses_per_path,
                        fpu,
                    );
                    true
                }
                _ => false,
            }
        }
    };
    #[cfg(not(target_arch = "x86_64"))]
    let done = false;
    let _ = mode;
    if !done {
        for p in &mut inputs.all_priors[..inputs.all_len] {
            *p = sign * fpu + scale * *p;
        }
        for i in 0..inputs.len {
            inputs.priors[i] = scalar(
                inputs.at(i),
                sign,
                scale,
                loss,
                config.virtual_losses_per_path,
            );
        }
    }
    for i in 0..inputs.len {
        inputs.all_priors[inputs.indices[i]] = inputs.priors[i];
    }
    // The ascending index iteration makes equal scores keep the lower index.
    // total_cmp retains the existing signed-zero/NaN ordering as well.
    let mut best: Option<(usize, f64)> = None;
    let result: Vec<_> = inputs.all_priors[..inputs.all_len]
        .iter()
        .enumerate()
        .map(|(i, &score)| {
            if best.is_none_or(|(_, value)| score.total_cmp(&value).is_gt()) {
                best = Some((i, score));
            }
            ScoredEdge { index: i, score }
        })
        .collect();
    EdgeOrder {
        first: best.map(|(i, _)| i),
        remaining: result,
        heap: None,
    }
}

#[cfg(target_arch = "x86_64")]
mod avx512 {
    use super::*;
    use std::arch::x86_64::*;
    #[target_feature(enable = "avx512f")]
    unsafe fn blend512(a: __m512d, b: __m512d, mask: u8) -> __m512d {
        _mm512_mask_blend_pd(mask, a, b)
    }
    #[target_feature(enable = "avx512f")]
    unsafe fn any512(mask: u8) -> bool {
        mask != 0
    }

    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn scores(
        inputs: &mut Prepared,
        sign: f64,
        scale: f64,
        loss: f64,
        virtual_loss: f64,
        fpu: f64,
    ) {
        // Every childless edge has denominator exactly 1. Score all
        // priors cheaply, then overwrite the sparse child candidates.
        let mut base_i = 0;
        while base_i + 8 <= inputs.all_len {
            unsafe {
                let p = inputs.all_priors.as_mut_ptr().add(base_i);
                _mm512_storeu_pd(
                    p,
                    _mm512_add_pd(
                        _mm512_set1_pd(sign * fpu),
                        _mm512_mul_pd(_mm512_set1_pd(scale), _mm512_loadu_pd(p)),
                    ),
                );
            }
            base_i += 8;
        }
        for p in &mut inputs.all_priors[base_i..inputs.all_len] {
            *p = sign * fpu + scale * *p;
        }
        let mut i = 0;
        while i + 8 <= inputs.len {
            // SAFETY: the loop bound guarantees full initialized chunks.
            unsafe {
                calculate_ptr(
                    inputs.weights.as_ptr().add(i),
                    inputs.utilities.as_ptr().add(i),
                    inputs.flights.as_ptr().add(i),
                    inputs.priors.as_mut_ptr().add(i),
                    sign,
                    scale,
                    loss,
                    virtual_loss,
                );
            }
            i += 8;
        }
        for i in i..inputs.len {
            inputs.priors[i] = scalar(inputs.at(i), sign, scale, loss, virtual_loss);
        }
    }

    #[cfg(test)]
    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn calculate(
        xs: [Input; 8],
        sign: f64,
        scale: f64,
        loss: f64,
        virtual_loss: f64,
    ) -> [f64; 8] {
        // SAFETY: every load/store addresses a full, initialized lane
        // array; loadu/storeu impose no extra alignment requirement.
        let weights = xs.map(|x| x.weight);
        let utilities = xs.map(|x| x.utility);
        let flights = xs.map(|x| x.flights);
        let mut priors = xs.map(|x| x.prior);
        unsafe {
            calculate_ptr(
                weights.as_ptr(),
                utilities.as_ptr(),
                flights.as_ptr(),
                priors.as_mut_ptr(),
                sign,
                scale,
                loss,
                virtual_loss,
            );
        }
        priors
    }

    #[target_feature(enable = "avx512f")]
    #[allow(clippy::too_many_arguments)]
    unsafe fn calculate_ptr(
        weights: *const f64,
        utilities: *const f64,
        flights: *const f64,
        priors: *mut f64,
        sign: f64,
        scale: f64,
        loss: f64,
        virtual_loss: f64,
    ) {
        let (w, u, f, p) = unsafe {
            (
                _mm512_loadu_pd(weights),
                _mm512_loadu_pd(utilities),
                _mm512_loadu_pd(flights),
                _mm512_loadu_pd(priors),
            )
        };
        let mask = _mm512_cmp_pd_mask::<_CMP_GT_OQ>(f, _mm512_set1_pd(0.0));
        let (utility, weight) = if unsafe { any512(mask) } {
            let v = _mm512_mul_pd(f, _mm512_set1_pd(virtual_loss));
            let adjusted_u = _mm512_add_pd(
                u,
                _mm512_div_pd(
                    _mm512_mul_pd(_mm512_sub_pd(_mm512_set1_pd(loss), u), v),
                    _mm512_add_pd(v, _mm512_max_pd(w, _mm512_set1_pd(0.25))),
                ),
            );
            (
                blend512(u, adjusted_u, mask),
                blend512(w, _mm512_add_pd(w, v), mask),
            )
        } else {
            (u, w)
        };
        let scores = _mm512_add_pd(
            _mm512_mul_pd(_mm512_set1_pd(sign), utility),
            _mm512_div_pd(
                _mm512_mul_pd(_mm512_set1_pd(scale), p),
                _mm512_add_pd(_mm512_set1_pd(1.0), weight),
            ),
        );
        unsafe {
            _mm512_storeu_pd(priors, scores);
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use crate::{Color, Position};

    #[test]
    #[ignore = "explicit scoring-kernel ablation; excludes search, network and GPU"]
    fn benchmark_prepared_scoring() {
        let mode = std::env::var("SCORING_BENCH_MODE").unwrap_or_else(|_| "avx512".into());
        let kernel = match mode.as_str() {
            "prepared_scalar" => SearchSimd::Scalar,
            "avx512" => SearchSimd::Avx512.resolve().unwrap(),
            _ => panic!("expected prepared_scalar or avx512"),
        };
        let iterations = 100_000;
        for children in [0, 16, 128, 362] {
            let position = Position::new(19, 7.5).unwrap();
            let config = SearchConfig::default();
            let mut nodes: Vec<_> = (0..33).map(|_| Node::new(&position)).collect();
            for (i, node) in nodes.iter_mut().enumerate().skip(1) {
                node.stats.visits = (i + 1) as u64;
                node.stats.weight_sum = (i * 7) as f64;
                node.stats.utility = (i % 5) as f64 / 5.0 - 0.5;
                node.in_flight = (i % 3) as u32;
            }
            nodes[0].edges = (0..362)
                .map(|i| super::super::Edge {
                    point: Some(i as u16),
                    prior: ((i * 137) % 362) as f64 / 362.0,
                    child: (i < children).then_some(i % 32 + 1),
                    visits: (i % 3) as u64,
                    in_flight: 0,
                })
                .collect();
            let mut scratch = Prepared::default();
            for _ in 0..100 {
                std::hint::black_box(prepared_scores(
                    kernel,
                    &nodes[0],
                    &nodes,
                    &config,
                    &mut scratch,
                ));
            }
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(prepared_scores(
                    kernel,
                    std::hint::black_box(&nodes[0]),
                    &nodes,
                    &config,
                    &mut scratch,
                ));
            }
            println!(
                "{}",
                serde_json::json!({"mode":mode,"children":children,"iterations":iterations,"seconds":start.elapsed().as_secs_f64(),"scope":"same sparse preparation and fused ordering; explicit AVX512 versus scalar/autovectorized arithmetic"})
            );
        }
    }

    #[test]
    fn sparse_preparation_and_fused_best_match_original_search_scores_bitwise() {
        let position = Position::new(19, 7.5).unwrap();
        for case in 0..16 {
            let config = SearchConfig {
                simd: SearchSimd::Scalar,
                cpuct_exploration_log: [0.0, 0.7][case % 2],
                cpuct_exploration_base: [0.000001, 500.0][case % 2],
                virtual_losses_per_path: [0.0, 3.0, 1000.0][case % 3],
                fpu_reduction_max: [0.0, 0.2][case % 2],
                ..Default::default()
            };
            let mut search = crate::Search::new(position.clone(), config).unwrap();
            search.selection_scratch = Some(Box::default());
            search.nodes.extend((0..31).map(|_| Node::new(&position)));
            search.nodes[0].color = if case % 2 == 0 {
                Color::Black
            } else {
                Color::White
            };
            for (i, child) in search.nodes.iter_mut().enumerate().skip(1) {
                child.stats.visits = [0, 1, 199, u64::MAX][i % 4];
                child.stats.weight_sum = [0.0, f64::from_bits(1), 1e-100, 0.5, 3.0, 1e100][i % 6];
                child.stats.utility = (i % 11) as f64 / 7.0 - 0.5;
                child.in_flight = if case % 3 == 0 { 0 } else { (i % 5) as u32 };
            }
            // Reuse the same scratch through shrinking and growing edge sets.
            for count in [362, 0, 1, 361, 9, 4, 8, 19] {
                search.nodes[0].edges = (0..count)
                    .map(|i| super::super::Edge {
                        point: Some(i as u16),
                        prior: (i % 17) as f64 / 19.0,
                        child: (i % (case + 1) != 0).then_some(i % 31 + 1),
                        visits: [0, 1, 123, u64::MAX][i % 4],
                        in_flight: 0,
                    })
                    .collect();
                search.simd = SearchSimd::Scalar;
                let expected = search.ordered_edges(0);
                for mode in [SearchSimd::Scalar, SearchSimd::Avx512] {
                    if mode.resolve().is_err() {
                        continue;
                    }
                    search.simd = mode;
                    let actual = search.ordered_edges(0);
                    assert_eq!(
                        actual
                            .remaining
                            .iter()
                            .map(|e| e.score.to_bits())
                            .collect::<Vec<_>>(),
                        expected
                            .remaining
                            .iter()
                            .map(|e| e.score.to_bits())
                            .collect::<Vec<_>>()
                    );
                    assert_eq!(
                        actual.collect::<Vec<_>>(),
                        EdgeOrder::new(expected.remaining.clone()).collect::<Vec<_>>()
                    );
                }
            }
        }
    }

    #[test]
    fn fused_best_keeps_total_order_for_zeros_infinities_and_nan_payloads() {
        let position = Position::new(19, 7.5).unwrap();
        let node = Node::new(&position);
        let mut inputs = Prepared {
            all_len: MAX_EDGES,
            ..Default::default()
        };
        for (i, p) in inputs.all_priors.iter_mut().enumerate() {
            *p = [
                -0.0,
                0.0,
                f64::NEG_INFINITY,
                f64::INFINITY,
                f64::from_bits(0x7ff8000000000042),
                f64::from_bits(0xfff8000000000043),
                0.42,
            ][i % 7];
        }
        let actual = run(
            SearchSimd::Scalar,
            &node,
            &SearchConfig::default(),
            &mut inputs,
            0.0,
            1.0,
        );
        let expected = EdgeOrder::new(actual.remaining.clone());
        assert_eq!(actual.collect::<Vec<_>>(), expected.collect::<Vec<_>>());
    }

    #[test]
    fn vector_arithmetic_matches_scalar_bits_including_signed_zero_and_subnormals() {
        let mut seed = 0x8192babc712339u64;
        let mut draw = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let weights = [
            0.0,
            -0.0,
            f64::from_bits(1),
            0.25,
            0.25000000000000006,
            1.0,
            1e-200,
            1e200,
        ];
        for i in 0..4096 {
            let xs: [Input; 8] = std::array::from_fn(|_| Input {
                weight: weights[(draw() % 8) as usize],
                utility: [0.0, -0.0, -0.987, 0.999, 1e-300][(draw() % 5) as usize],
                flights: [0.0, 1.0, 128.0, u32::MAX as f64][(draw() % 4) as usize],
                prior: [0.0, -0.0, 1e-300, 0.01, 0.9][(draw() % 5) as usize],
            });
            let sign = if i % 2 == 0 { -1.0 } else { 1.0 };
            let scale = [0.0, 0.5, 9.0, 1e-100][i % 4];
            let virtual_loss = [0.0, 3.0, 1000.0][i % 3];
            let loss = -sign * 1.3;
            let expected = xs.map(|x| scalar(x, sign, scale, loss, virtual_loss).to_bits());
            if std::is_x86_feature_detected!("avx512f") {
                // SAFETY: checked above.
                let actual = unsafe { avx512::calculate(xs, sign, scale, loss, virtual_loss) };
                assert_eq!(actual.map(f64::to_bits), expected, "AVX512 case {i}");
            }
        }
    }

    #[test]
    fn tails_shared_children_and_complete_tie_order_match_scalar() {
        let position = Position::new(19, 7.5).unwrap();
        let config = SearchConfig::default();
        for count in [0, 1, 3, 4, 7, 8, 9, 19, 361, 362] {
            for color in [Color::Black, Color::White] {
                let mut nodes: Vec<_> = (0..17).map(|_| Node::new(&position)).collect();
                nodes[0].color = color;
                for (i, node) in nodes.iter_mut().enumerate().skip(1) {
                    node.stats.visits = (i + 1) as u64;
                    node.stats.weight_sum = (i * 7) as f64;
                    node.stats.utility = (i % 5) as f64 / 5.0 - 0.5;
                    node.in_flight = (i % 3) as u32;
                }
                nodes[0].edges = (0..count)
                    .map(|i| super::super::Edge {
                        point: Some(i as u16),
                        prior: if i % 4 == 0 { 0.0 } else { 0.01 },
                        child: (i % 4 != 0).then_some(i % 16 + 1),
                        visits: (i % 3) as u64,
                        in_flight: 0,
                    })
                    .collect();
                let expected =
                    vector_scores(SearchSimd::Scalar, &nodes[0], &nodes, &config, 0.1, 0.9);
                for mode in [SearchSimd::Scalar, SearchSimd::Avx512] {
                    if mode.resolve().is_err() {
                        continue;
                    }
                    let actual = vector_scores(mode, &nodes[0], &nodes, &config, 0.1, 0.9);
                    assert_eq!(
                        actual
                            .remaining
                            .iter()
                            .map(|e| e.score.to_bits())
                            .collect::<Vec<_>>(),
                        expected
                            .remaining
                            .iter()
                            .map(|e| e.score.to_bits())
                            .collect::<Vec<_>>()
                    );
                    assert_eq!(
                        actual.collect::<Vec<_>>(),
                        super::super::EdgeOrder::new(expected.remaining.clone())
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}
