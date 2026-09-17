use super::*;
use crate::worker_metrics::Metrics;
use go_core::{Position, Search, SearchStep};

fn pool(target: usize) -> WorkerPool {
    WorkerPool::with_scheduling(
        None,
        Duration::from_secs(10),
        SchedulingConfig {
            scheduler: Scheduler::Completion,
            target_inflight: Some(target),
            ..Default::default()
        },
    )
    .unwrap()
}
fn register(pool: &WorkerPool, id: &str, capacity: u32) -> (String, mpsc::Receiver<Outbound>) {
    let (tx, rx) = mpsc::channel(128);
    let connection = pool
        .register(
            wire::WorkerHello {
                worker_id: id.into(),
                instance_id: "fixture".into(),
                protocol_version: PROTOCOL_VERSION,
                model_sha256: "1".repeat(64),
                input_profile: INPUT_PROFILE.into(),
                max_in_flight: capacity,
                max_board_size: 19,
                supports_friendly_pass_search: true,
                ..Default::default()
            },
            tx,
        )
        .unwrap();
    (connection, rx)
}
fn request() -> EvaluationRequest {
    let mut s = Search::new(Position::new(19, 7.5).unwrap(), Default::default()).unwrap();
    let SearchStep::Evaluate(r) = s.next_evaluation().unwrap() else {
        panic!()
    };
    r
}
fn reply(message: Outbound) -> wire::WorkerMessage {
    let wire::server_message::Payload::Evaluate(r) = message.message.unwrap().payload.unwrap()
    else {
        panic!()
    };
    wire::WorkerMessage {
        payload: Some(wire::worker_message::Payload::Result(wire::EvalResult {
            task_id: r.task_id,
            generation: r.generation,
            session_id: r.session_id,
            input_hash: r.input_hash,
            model_sha256: r.model_sha256,
            output: Some(Default::default()),
            context_us: Some(24),
            evaluator_us: Some(1000),
            elapsed_us: 1024,
            ..Default::default()
        })),
    }
}

#[test]
fn capacity_does_not_bias_startup_and_all_workers_are_probed() {
    let p = pool(64);
    let (_, _a) = register(&p, "cpp", 1024);
    let (_, _b) = register(&p, "rust", 64);
    let (tx, _rx) = sync_mpsc::channel();
    let r = request();
    for _ in 0..8 {
        assert!(p.dispatch("one", &r, tx.clone()));
    }
    assert!(!p.dispatch("one", &r, tx));
    for w in p.views() {
        assert_eq!(w.in_flight, 4);
        assert_eq!(w.target_inflight, 64);
    }
}

#[test]
fn measured_speed_selects_worker_without_capacity_weight() {
    let p = pool(64);
    let (_, _a) = register(&p, "slow-big", 1024);
    let (_, _b) = register(&p, "fast-small", 64);
    {
        let mut s = p.inner.lock().unwrap();
        for (name, ms) in [("slow-big", 80.0), ("fast-small", 20.0)] {
            let w = s.workers.get_mut(name).unwrap();
            for _ in 0..4 {
                w.completion.observe(64, ms);
            }
        }
    }
    let (tx, _rx) = sync_mpsc::channel();
    for _ in 0..64 {
        assert!(p.dispatch("one", &request(), tx.clone()));
    }
    let views = p.views();
    assert!(views[0].assigned_requests > views[1].assigned_requests * 2);
}

#[test]
fn waiting_sessions_get_next_credit_and_cancel_keeps_physical_credit() {
    let p = pool(1);
    let (connection, mut rx) = register(&p, "w", 1024);
    let (tx, mailbox) = sync_mpsc::channel();
    let r = request();
    assert!(p.dispatch("a", &r, tx.clone()));
    let original = reply(rx.try_recv().unwrap());
    assert!(!p.dispatch("b", &r, tx.clone()));
    p.cancel_session("a");
    assert_eq!(p.views()[0].retiring_in_flight, 1);
    assert!(!p.dispatch("b", &r, tx.clone()));
    p.receive("w", &connection, original.clone());
    assert!(mailbox.try_recv().is_err());
    assert!(!p.dispatch("a", &r, tx.clone()));
    assert!(p.dispatch("b", &r, tx.clone()));
    p.receive("w", &connection, original); // Duplicate cannot release b's credit.
    assert_eq!(p.views()[0].in_flight, 1);
    assert_eq!(p.views()[0].retired_results, 1);
    p.cancel_session("a"); // Remove a blocked waiter, even with no assignments.
    assert!(!p.dispatch("b", &r, tx));
}

#[test]
fn bytes_stages_and_old_connection_results_have_explicit_boundaries() {
    let p = pool(2);
    let (connection, mut rx) = register(&p, "w", 32);
    let (tx, mailbox) = sync_mpsc::channel();
    assert!(p.dispatch("one", &request(), tx));
    let item = rx.try_recv().unwrap();
    let expected = item.message.as_ref().unwrap().encode_to_vec().len() as u64 + 5;
    let metrics = p.inner.lock().unwrap().workers["w"].metrics.clone();
    let message = item.into_message(&metrics);
    let result = reply(Outbound::new(message));
    let result_bytes = result.encode_to_vec().len() as u64 + 5;
    p.receive("w", "obsolete-connection", result.clone());
    assert_eq!(p.views()[0].metrics.result_bytes_received, 0);
    p.receive("w", &connection, result);
    let v = p.views().remove(0);
    assert_eq!(v.metrics.request_bytes_enqueued, expected);
    assert_eq!(v.metrics.request_bytes_streamed, expected);
    assert_eq!(v.metrics.result_bytes_received, result_bytes);
    assert_eq!(v.metrics.context.p95_ms, Some(0.024));
    assert_eq!(v.metrics.worker_queue.p95_ms, None);
    assert_eq!(v.metrics.actor_wait.total_count, 0);
    let _ = mailbox.recv().unwrap().into_parts();
    assert_eq!(p.views()[0].metrics.actor_wait.total_count, 1);
}

#[tokio::test]
async fn incoming_sets_nodelay_on_the_accepted_socket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut incoming =
        tonic::transport::server::TcpIncoming::from(listener).with_nodelay(Some(true));
    let _client = tokio::net::TcpStream::connect(address).await.unwrap();
    assert!(incoming.next().await.unwrap().unwrap().nodelay().unwrap());
}

#[test]
fn reconnect_starts_a_new_snapshot_cache_and_old_observers_stay_isolated() {
    let p = pool(2);
    let (old_connection, _old_rx) = register(&p, "w", 32);
    let old_metrics = p.inner.lock().unwrap().workers["w"].metrics.clone();
    old_metrics.lock().unwrap().rpc.record(42.0);
    assert_eq!(p.snapshot_views()[0].metrics.rpc.total_count, 1);
    let (new_connection, _new_rx) = register(&p, "w", 32);
    assert_ne!(old_connection, new_connection);
    // A result already in an actor mailbox can still own the old store.
    old_metrics.lock().unwrap().actor_wait.record(7.0);
    let current = p.snapshot_views().remove(0);
    assert_eq!(current.connection_id, new_connection);
    assert_eq!(current.metrics.rpc.total_count, 0);
    assert_eq!(current.metrics.actor_wait.total_count, 0);
    assert_eq!(current.metrics.result_messages_received, 0);
}

#[test]
#[ignore = "explicit telemetry scaling benchmark; no GPU or playing-strength claim"]
fn benchmark_worker_snapshots() {
    let fresh = std::env::var("WORKER_BENCH_FRESH").is_ok_and(|v| v == "1");
    let count = std::env::var("WORKER_BENCH_COUNT").map_or(64, |v| v.parse::<usize>().unwrap());
    let p = pool(64);
    let mut receivers = Vec::new();
    let mut stores = Vec::new();
    for i in 0..count {
        let id = format!("worker-{i:04}");
        receivers.push(register(&p, &id, 64).1);
        stores.push(p.inner.lock().unwrap().workers[&id].metrics.clone());
    }
    let record = |m: &mut Metrics, value: f64| {
        m.rpc.record(value);
        m.worker_elapsed.record(value);
        m.worker_queue.record(value);
        m.context.record(value);
        m.evaluator.record(value);
        m.send_queue.record(value);
        m.actor_wait.record(value);
        m.retired_rpc.record(value);
    };
    for store in &stores {
        let mut m = store.lock().unwrap();
        for i in 0..2048 {
            record(&mut m, ((i * 137) % 2048) as f64 / 100.0);
        }
    }
    let read = || if fresh { p.views() } else { p.snapshot_views() };
    std::hint::black_box(read());
    let started = Instant::now();
    for sample in 0..100 {
        for store in &stores {
            record(&mut store.lock().unwrap(), sample as f64 / 10.0);
        }
        std::hint::black_box(read());
    }
    println!(
        "{}",
        serde_json::json!({"workers":count,"snapshots":100,"fresh":fresh,
            "seconds":started.elapsed().as_secs_f64(),
            "scope":"rapid telemetry reads with changing samples; excludes network and GPU"})
    );
}
