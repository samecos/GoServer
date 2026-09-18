//! Real-NN FIFO capture and CPU-only replay. Never attaches to live sessions.
//! capture FILE FIXTURES ID COUNT WINDOW; replay FILE
use go_core::*;
use go_protocol::v1::{worker_service_server::WorkerServiceServer, NnOutput};
use go_server::{
    worker::WorkerPool,
    worker_schedule::{Scheduler, SchedulingConfig},
};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    time::{Duration, Instant},
};

const MODEL: &str = "1881600caab9e9d85a3dd6a019e9b8e7d2c237b5f984e13ed49a8645be3077c6";
#[derive(Serialize, Deserialize)]
struct Header {
    fixture: Value,
    config: SearchConfig,
    count: usize,
    window: usize,
    model: String,
}
fn position(f: &Value) -> Position {
    assert_eq!(f["boardSize"], 19);
    assert_eq!(f["rules"], "chinese");
    let mut p = Position::new(19, f["komi"].as_f64().unwrap()).unwrap();
    for m in f["moves"].as_array().unwrap() {
        let vertex = m[1].as_str().unwrap();
        let point = if vertex.eq_ignore_ascii_case("pass") {
            None
        } else {
            Some(
                ((19 - vertex[1..].parse::<usize>().unwrap()) * 19
                    + "ABCDEFGHJKLMNOPQRST".find(&vertex[..1]).unwrap()) as u16,
            )
        };
        p.play(Move {
            color: if m[0] == "B" {
                Color::Black
            } else {
                Color::White
            },
            point,
        })
        .unwrap();
    }
    p
}
fn evaluation(v: NnOutput) -> Evaluation {
    Evaluation {
        policy: v.policy.into_iter().map(f64::from).collect(),
        white_win_prob: v.white_win_prob,
        white_loss_prob: v.white_loss_prob,
        white_no_result_prob: v.white_no_result_prob,
        white_score_mean: v.white_score_mean,
        white_score_mean_sq: v.white_score_mean_sq,
        white_lead: v.white_lead,
        has_shortterm_error: v.has_shortterm_error,
        shortterm_winloss_error: v.shortterm_winloss_error,
        shortterm_score_error: v.shortterm_score_error,
        ownership: v.ownership.into_iter().map(f64::from).collect(),
    }
}
fn read_blob(r: &mut impl Read) -> Vec<u8> {
    let mut len = [0; 4];
    r.read_exact(&mut len).unwrap();
    let n = u32::from_le_bytes(len) as usize;
    assert!(n <= 1024 * 1024);
    let mut b = vec![0; n];
    r.read_exact(&mut b).unwrap();
    b
}
fn write_blob(w: &mut impl Write, b: &[u8]) {
    w.write_all(&(b.len() as u32).to_le_bytes()).unwrap();
    w.write_all(b).unwrap();
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let capture = args[0] == "capture";
    assert!(capture || args[0] == "replay");
    let mut writer = None;
    let mut records = VecDeque::new();
    let h: Header = if capture {
        let fixtures: Value = serde_json::from_slice(&std::fs::read(&args[2]).unwrap()).unwrap();
        let fixture = fixtures
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == args[3])
            .unwrap()
            .clone();
        let count = args[4].parse().unwrap();
        let window = args[5].parse().unwrap();
        let h = Header {
            fixture,
            config: SearchConfig {
                max_in_flight: window,
                ..Default::default()
            },
            count,
            window,
            model: MODEL.into(),
        };
        let mut w = BufWriter::new(File::create_new(&args[1]).unwrap());
        w.write_all(b"GONNREP1").unwrap();
        write_blob(&mut w, &serde_json::to_vec(&h).unwrap());
        writer = Some(w);
        h
    } else {
        let mut r = BufReader::new(File::open(&args[1]).unwrap());
        let mut magic = [0; 8];
        r.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, b"GONNREP1");
        let h: Header = serde_json::from_slice(&read_blob(&mut r)).unwrap();
        assert_eq!(h.model, MODEL);
        for _ in 0..h.count {
            let mut hash = [0; 32];
            r.read_exact(&mut hash).unwrap();
            records.push_back((
                hash,
                evaluation(NnOutput::decode(read_blob(&mut r).as_slice()).unwrap()),
            ));
        }
        let mut trailing = [0; 1];
        assert_eq!(r.read(&mut trailing).unwrap(), 0);
        h
    };
    assert!(h.count > 0 && h.window > 0);
    let pool = WorkerPool::with_scheduling(
        Some(MODEL.into()),
        Duration::from_secs(60),
        SchedulingConfig {
            scheduler: Scheduler::Completion,
            target_inflight: Some(h.window),
            ..Default::default()
        },
    )
    .unwrap();
    let grpc = if capture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:50062")
            .await
            .unwrap();
        let p = pool.clone();
        Some(tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(WorkerServiceServer::new(p))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        }))
    } else {
        None
    };
    if capture {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !pool.available() {
            assert!(Instant::now() < deadline, "worker startup timeout");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let mut search = Search::new(position(&h.fixture), h.config.clone()).unwrap();
    #[cfg(feature = "search-hotspots")]
    profiling::hotspots::configure_paths(&std::env::var("SEARCH_HOTSPOT_KEYS").unwrap_or_default());
    let (tx, rx) = std::sync::mpsc::channel();
    let mut ready = HashMap::new();
    let mut pending = VecDeque::new();
    let (mut issued, mut completed, mut advanced) = (0, 0, 0u64);
    let (mut next_ns, mut complete_ns) = (0u128, 0u128);
    profiling::take();
    #[cfg(feature = "search-profiling")]
    profiling::take_activity();
    let start = Instant::now();
    while completed < h.count {
        while issued < h.count && pending.len() < h.window {
            let t = Instant::now();
            let step = search.next_evaluation().unwrap();
            next_ns += t.elapsed().as_nanos();
            match step {
                SearchStep::Evaluate(request) => {
                    if capture {
                        let deadline = Instant::now() + Duration::from_secs(60);
                        while !pool.dispatch("fifo-capture", &request, tx.clone()) {
                            assert!(Instant::now() < deadline, "dispatch timeout");
                            if let Ok(outcome) = rx.recv_timeout(Duration::from_millis(1)) {
                                let (token, v) = outcome.into_parts();
                                assert!(ready.insert(token, v.unwrap()).is_none());
                            }
                        }
                    }
                    pending.push_back(request);
                    issued += 1;
                }
                SearchStep::Advanced => advanced += 1,
                SearchStep::Waiting => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        let request = pending.pop_front().expect("no progress");
        let value = if capture {
            while !ready.contains_key(&request.token) {
                let outcome = rx.recv_timeout(Duration::from_secs(60)).unwrap();
                let (token, v) = outcome.into_parts();
                assert!(ready.insert(token, v.unwrap()).is_none());
            }
            let v = ready.remove(&request.token).unwrap();
            let w = writer.as_mut().unwrap();
            w.write_all(&request.input_hash).unwrap();
            write_blob(w, &v.encode_to_vec());
            evaluation(v)
        } else {
            let (hash, value) = records.pop_front().unwrap();
            assert_eq!(
                hash, request.input_hash,
                "replay diverged at completion {completed}"
            );
            value
        };
        let t = Instant::now();
        assert_eq!(
            search.complete(request.token, value).unwrap(),
            Completion::Applied
        );
        complete_ns += t.elapsed().as_nanos();
        completed += 1;
        if completed % 10000 == 0 || completed == h.count {
            let stages: serde_json::Map<_, _> = profiling::NAMES
                .into_iter()
                .zip(profiling::take())
                .map(|(n, s)| (n.into(), serde_json::to_value(s).unwrap()))
                .collect();
            #[cfg(feature = "search-profiling")]
            let activity = serde_json::to_value(profiling::take_activity()).unwrap();
            #[cfg(not(feature = "search-profiling"))]
            let activity = Value::Null;
            // Snapshot cost is deliberately outside search CPU timings.
            let snapshot = search.snapshot(12, 20);
            #[cfg(feature = "search-hotspots")]
            let hotspots = serde_json::to_value(profiling::hotspots::take()).unwrap();
            #[cfg(not(feature = "search-hotspots"))]
            let hotspots = Value::Null;
            println!(
                "{}",
                json!({"mode":args[0],"completed":completed,"advanced":advanced,"nextNs":next_ns,"completeNs":complete_ns,"elapsed":start.elapsed().as_secs_f64(),"stages":stages,"activity":activity,"snapshot":snapshot,"hotspots":hotspots})
            );
            next_ns = 0;
            complete_ns = 0;
        }
    }
    if let Some(mut w) = writer {
        w.flush().unwrap();
    }
    assert!(pending.is_empty() && ready.is_empty() && records.is_empty());
    if let Some(task) = grpc {
        task.abort();
    }
}
