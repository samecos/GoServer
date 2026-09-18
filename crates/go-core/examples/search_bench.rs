//! Deterministic CPU benchmark, not a neural evaluator or strength benchmark.
//! Run release builds with/without `search-profiling`; compare traceSha256 and
//! final statistics before interpreting speed. Completion order is fixed FIFO.
use go_core::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, time::Instant};

fn fixture(name: &str) -> Position {
    if let Ok(path) = std::env::var("SEARCH_BENCH_POSITIONS") {
        let entries: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let entry = entries
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == name)
            .unwrap();
        assert_eq!(entry["boardSize"], 19);
        assert_eq!(entry["rules"], "chinese");
        let mut p = Position::new(19, entry["komi"].as_f64().unwrap()).unwrap();
        for mv in entry["moves"].as_array().unwrap() {
            let vertex = mv[1].as_str().unwrap();
            let point = if vertex.eq_ignore_ascii_case("pass") {
                None
            } else {
                let x = "ABCDEFGHJKLMNOPQRST".find(&vertex[..1]).unwrap();
                let y = 19 - vertex[1..].parse::<usize>().unwrap();
                Some((y * 19 + x) as u16)
            };
            let color = if mv[0] == "B" {
                Color::Black
            } else {
                Color::White
            };
            p.play(Move { color, point }).unwrap();
        }
        return p;
    }
    let points = match name {
        "opening" => vec![Some(288), Some(72), Some(60), Some(97)],
        "fight" => vec![
            Some(288),
            Some(287),
            Some(269),
            Some(268),
            Some(289),
            Some(270),
            Some(308),
            Some(307),
            Some(306),
            Some(249),
            Some(291),
            Some(309),
        ],
        "pass" => vec![
            Some(288),
            Some(72),
            Some(60),
            Some(300),
            Some(174),
            Some(186),
            None,
        ],
        _ => panic!("unknown fixture"),
    };
    let mut p = Position::new(19, 7.5).unwrap();
    for point in points {
        p.play(Move {
            color: p.to_move(),
            point,
        })
        .unwrap();
    }
    p
}

fn synthetic(p: &Position, transpositions: bool) -> Evaluation {
    let hash = p.input_hash();
    let seed = u64::from_le_bytes(hash[..8].try_into().unwrap());
    let policy = (0..362)
        .map(|i| {
            let offset = if transpositions {
                if p.to_move() == Color::Black {
                    17
                } else {
                    149
                }
            } else {
                seed % 361
            };
            let rank = (i as u64 * 137 + offset) % 361;
            if i == 361 {
                0.00001
            } else {
                1.0 / (1.0 + rank as f64).powi(2)
            }
        })
        .collect();
    let q = (hash[8] as f64 / 255.0 - 0.5) * 0.6;
    let score = q * 12.0;
    Evaluation {
        policy,
        white_win_prob: (1.0 + q) * 0.5,
        white_loss_prob: (1.0 - q) * 0.5,
        white_no_result_prob: 0.0,
        white_score_mean: score,
        white_score_mean_sq: score * score + 16.0,
        white_lead: score,
        has_shortterm_error: true,
        shortterm_winloss_error: 0.1,
        shortterm_score_error: 1.0,
        ownership: vec![],
    }
}

fn run(name: &str, window: usize, evaluations: usize) -> serde_json::Value {
    let transpositions = std::env::var("SEARCH_BENCH_TRANSPOSITIONS").is_ok_and(|v| v == "1");
    let mut search = Search::new(
        fixture(name),
        SearchConfig {
            simd: std::env::var("SEARCH_BENCH_SIMD")
                .unwrap_or_else(|_| "auto".into())
                .parse()
                .unwrap(),
            use_uncertainty: std::env::var("SEARCH_BENCH_UNCERTAINTY").is_ok_and(|v| v == "1"),
            max_nodes: evaluations + 1000,
            max_memory_bytes: 4 * 1024 * 1024 * 1024,
            max_in_flight: window,
            ..Default::default()
        },
    )
    .unwrap();
    let mut pending = VecDeque::new();
    let mut trace = Sha256::new();
    let (mut issued, mut completed, mut advanced) = (0usize, 0usize, 0usize);
    let (mut next_ns, mut complete_ns, mut eval_ns, mut snapshot_ns) = (0u128, 0u128, 0u128, 0u128);
    profiling::take();
    #[cfg(feature = "search-profiling")]
    profiling::take_activity();
    let started = Instant::now();
    while completed < evaluations {
        while issued < evaluations && pending.len() < window {
            let t = Instant::now();
            let step = search.next_evaluation().unwrap();
            next_ns += t.elapsed().as_nanos();
            match step {
                SearchStep::Evaluate(request) => {
                    trace.update(request.input_hash);
                    pending.push_back(request);
                    issued += 1;
                }
                SearchStep::Advanced => {
                    advanced += 1;
                }
                SearchStep::Waiting => break,
                other => panic!("unexpected search state: {other:?}"),
            }
        }
        let request = pending.pop_front().expect("search made no progress");
        let t = Instant::now();
        let evaluation = synthetic(&request.position, transpositions);
        eval_ns += t.elapsed().as_nanos();
        let t = Instant::now();
        assert_eq!(
            search.complete(request.token, evaluation).unwrap(),
            Completion::Applied
        );
        complete_ns += t.elapsed().as_nanos();
        completed += 1;
        if completed % 500 == 0 {
            let t = Instant::now();
            std::hint::black_box(search.snapshot(362, 30));
            snapshot_ns += t.elapsed().as_nanos();
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let stats = profiling::take();
    let snapshot = search.snapshot(362, 30);
    let stage_samples: serde_json::Map<_, _> = profiling::NAMES
        .into_iter()
        .zip(stats)
        .map(|(name, sample)| (name.into(), serde_json::to_value(sample).unwrap()))
        .collect();
    let result = json!({"fixture":name,"window":window,"evaluations":completed,"advanced":advanced,"simdBackend":search.simd_backend(),
        "transpositionPolicy":transpositions,
        "useUncertainty":search.config().use_uncertainty,
        "seconds":elapsed,"evaluationsPerSecond":completed as f64/elapsed,
        "nextNs":next_ns,"completeNs":complete_ns,"syntheticEvaluatorNs":eval_ns,"snapshotNs":snapshot_ns,
        "profilingEnabled":cfg!(feature="search-profiling"),"inclusiveStages":stage_samples,
        "traceSha256":format!("{:x}",trace.finalize()),"snapshot":snapshot});
    #[cfg(feature = "search-profiling")]
    let result = {
        let mut result = result;
        result["activity"] = serde_json::to_value(profiling::take_activity()).unwrap();
        result
    };
    result
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let evaluations = args.first().map_or(4000, |v| v.parse::<usize>().unwrap());
    let repeats = args.get(1).map_or(1, |v| v.parse::<usize>().unwrap());
    assert!(evaluations > 0 && repeats > 0);
    let windows: Vec<usize> = std::env::var("SEARCH_BENCH_WINDOWS")
        .unwrap_or_else(|_| "1,32".into())
        .split(',')
        .map(|v| v.parse::<usize>().expect("positive search window"))
        .collect();
    assert!(!windows.is_empty() && windows.iter().all(|&v| v > 0));
    let fixtures: Vec<String> = if let Ok(path) = std::env::var("SEARCH_BENCH_POSITIONS") {
        let entries: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        entries
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_string())
            .collect()
    } else {
        ["opening", "fight", "pass"]
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    // Initialize the shared numerical tables outside measured samples.
    std::hint::black_box(run(&fixtures[0], 1, 32));
    for repeat in 0..repeats {
        for fixture in &fixtures {
            for &window in &windows {
                let mut result = run(fixture, window, evaluations);
                result["repeat"] = json!(repeat);
                println!("{result}");
            }
        }
    }
}
