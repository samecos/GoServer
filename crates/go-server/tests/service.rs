//! Network and lifecycle tests use a deliberately synthetic evaluator fixture.
//! These prove orchestration, not neural-network fidelity or playing strength.
use futures_util::{SinkExt, StreamExt};
use go_protocol::{
    INPUT_PROFILE, PROTOCOL_VERSION,
    v1::{
        self as w, worker_service_client::WorkerServiceClient,
        worker_service_server::WorkerServiceServer,
    },
};
use go_server::{
    app::App,
    session::{SessionConfig, Sessions},
    worker::WorkerPool,
    worker_schedule::{Scheduler, SchedulingConfig},
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const MODEL: &str = "1111111111111111111111111111111111111111111111111111111111111111";
struct Harness {
    http: String,
    grpc: String,
    sessions: Sessions,
    pool: WorkerPool,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Harness {
    async fn new() -> Self {
        let mut config = SessionConfig {
            publish: Duration::from_millis(20),
            retention: Duration::from_secs(2),
            ..Default::default()
        };
        config.search.max_in_flight = 4;
        Self::configured(config).await
    }
    async fn configured(config: SessionConfig) -> Self {
        Self::configured_with_lease(config, Duration::from_millis(150)).await
    }
    async fn configured_with_lease(config: SessionConfig, lease: Duration) -> Self {
        Self::scheduled(config, lease, SchedulingConfig::default()).await
    }
    async fn scheduled(
        config: SessionConfig,
        lease: Duration,
        scheduling: SchedulingConfig,
    ) -> Self {
        let pool = WorkerPool::with_scheduling(Some(MODEL.into()), lease, scheduling).unwrap();
        let sessions = Sessions::new(config, pool.clone());
        let hl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = format!("ws://{}/ws", hl.local_addr().unwrap());
        let gl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc = format!("http://{}", gl.local_addr().unwrap());
        let app = App::new(sessions.clone(), pool.clone());
        let ps = pool.clone();
        let maintenance = pool.clone();
        let tasks = vec![
            tokio::spawn(async move {
                axum::serve(hl, app.router()).await.unwrap();
            }),
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(WorkerServiceServer::new(ps))
                    .serve_with_incoming(TcpListenerStream::new(gl))
                    .await
                    .unwrap();
            }),
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    maintenance.maintenance();
                }
            }),
        ];
        Self {
            http,
            grpc,
            sessions,
            pool,
            tasks,
        }
    }
    async fn worker(
        &self,
        id: &str,
    ) -> (
        mpsc::Sender<w::WorkerMessage>,
        tonic::Streaming<w::ServerMessage>,
    ) {
        self.worker_with_hello(hello(id)).await
    }
    async fn worker_with_hello(
        &self,
        hello: w::WorkerHello,
    ) -> (
        mpsc::Sender<w::WorkerMessage>,
        tonic::Streaming<w::ServerMessage>,
    ) {
        let channel = tonic::transport::Endpoint::from_shared(self.grpc.clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = WorkerServiceClient::new(channel);
        let (tx, rx) = mpsc::channel(32);
        tx.send(w::WorkerMessage {
            payload: Some(w::worker_message::Payload::Hello(hello)),
        })
        .await
        .unwrap();
        let mut stream = client
            .connect(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            stream.message().await.unwrap().unwrap().payload,
            Some(w::server_message::Payload::Welcome(_))
        ));
        (tx, stream)
    }
}

#[tokio::test]
async fn completion_scheduler_serves_two_sessions_with_different_hard_capacities() {
    let h = Harness::scheduled(
        SessionConfig {
            publish: Duration::from_millis(20),
            ..Default::default()
        },
        Duration::from_secs(2),
        SchedulingConfig {
            scheduler: Scheduler::Completion,
            target_inflight: Some(2),
            ..Default::default()
        },
    )
    .await;
    let mut bots = vec![];
    for (id, capacity, delay) in [("large", 1024, 4), ("small", 64, 2)] {
        let mut identity = hello(id);
        identity.max_in_flight = capacity;
        let (tx, mut incoming) = h.worker_with_hello(identity).await;
        bots.push(tokio::spawn(async move {
            while let Ok(Some(msg)) = incoming.message().await {
                if let Some(w::server_message::Payload::Evaluate(r)) = msg.payload {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    if tx.send(result(r)).await.is_err() {
                        break;
                    }
                }
            }
        }));
    }
    let (mut a, _) = connect_async(&h.http).await.unwrap();
    let (mut b, _) = connect_async(&h.http).await.unwrap();
    request(&mut a, json!({"id":"open-a","type":"open"})).await;
    request(&mut b, json!({"id":"open-b","type":"open"})).await;
    let (a_result, b_result) = tokio::join!(
        request(
            &mut a,
            json!({"id":"a","type":"genmove","maxVisits":32,"maxTimeMs":2000})
        ),
        request(
            &mut b,
            json!({"id":"b","type":"genmove","maxVisits":32,"maxTimeMs":2000})
        ),
    );
    assert_eq!(a_result["ok"], true, "{a_result}");
    assert_eq!(b_result["ok"], true, "{b_result}");
    for w in h.pool.views() {
        assert_eq!(w.target_inflight, 2);
        assert!(w.in_flight <= 2);
        assert!(w.completed > 0);
        assert!(w.metrics.actor_wait.total_count > 0);
        assert!(w.metrics.send_queue.total_count > 0);
        assert!(w.shared_session_dispatches > 0);
    }
    for bot in bots {
        bot.abort();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.sessions.shutdown();
        for t in &self.tasks {
            t.abort();
        }
    }
}
fn hello(id: &str) -> w::WorkerHello {
    w::WorkerHello {
        worker_id: id.into(),
        instance_id: "fixture-instance".into(),
        protocol_version: PROTOCOL_VERSION,
        model_sha256: MODEL.into(),
        model_version: 15,
        input_profile: INPUT_PROFILE.into(),
        max_in_flight: 2,
        max_board_size: 19,
        supports_ownership: true,
        supports_shortterm_error: false,
        supports_friendly_pass_search: true,
        engine_commit: "synthetic-test-fixture".into(),
        ..Default::default()
    }
}
fn result(r: w::EvalRequest) -> w::WorkerMessage {
    w::WorkerMessage {
        payload: Some(w::worker_message::Payload::Result(w::EvalResult {
            task_id: r.task_id,
            generation: r.generation,
            session_id: r.session_id,
            input_hash: r.input_hash,
            model_sha256: r.model_sha256,
            output: Some(w::NnOutput {
                policy: vec![1.0 / 362.0; 362],
                white_win_prob: 0.48,
                white_loss_prob: 0.52,
                white_no_result_prob: 0.0,
                white_score_mean: -1.0,
                white_score_mean_sq: 100.0,
                white_lead: -1.0,
                ..Default::default()
            }),
            ..Default::default()
        })),
    }
}
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[tokio::test]
async fn search_settings_are_atomic_session_scoped_and_restored() {
    let h = Harness::new().await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    let open = request(&mut ws, json!({"id":"open","type":"open"})).await;
    let defaults = json!({"playoutDoublingAdvantage":0.0,"playoutDoublingAdvantagePla":"root","wideRootNoise":0.0});
    assert_eq!(open["data"]["settings"]["search"], defaults);
    let sid = open["data"]["sessionId"].clone();
    let change = request(&mut ws, json!({"id":"set","type":"configure_search","search":{"playoutDoublingAdvantage":-1.5,"playoutDoublingAdvantagePla":"white","wideRootNoise":0.04}})).await;
    assert_eq!(change["ok"], true, "{change}");
    let settings = change["data"]["settings"]["search"].clone();
    let generation = change["data"]["generation"].clone();
    for patch in [
        json!({"playoutDoublingAdvantage":3.1}),
        json!({"wideRootNoise":-0.1}),
        json!({"wideRootNoise":5.1}),
        json!({"wideRootNoise":null}),
        json!({"wideRootNoise":"0.1"}),
        json!({"playoutDoublingAdvantagePla":"blue"}),
        json!({"wideRootNoize":0.1}),
        json!(false),
    ] {
        let bad = request(
            &mut ws,
            json!({"id":"bad","type":"configure_search","search":patch}),
        )
        .await;
        assert_eq!(bad["error"]["code"], "INVALID_REQUEST", "{bad}");
        let current = request(&mut ws, json!({"id":"snapshot","type":"snapshot"})).await;
        assert_eq!(current["data"]["settings"]["search"], settings);
        assert_eq!(current["data"]["generation"], generation);
    }
    let noop = request(
        &mut ws,
        json!({"id":"noop","type":"configure_search","search":settings}),
    )
    .await;
    assert_eq!(noop["data"]["generation"], generation);
    let partial = request(
        &mut ws,
        json!({"id":"partial","type":"configure_search","search":{"wideRootNoise":0.08}}),
    )
    .await;
    assert_eq!(
        partial["data"]["settings"]["search"]["playoutDoublingAdvantage"],
        -1.5
    );
    let stale = request(
        &mut ws,
        json!({"id":"stale","type":"configure_search","generation":generation,"search":defaults}),
    )
    .await;
    assert_eq!(stale["error"]["code"], "STALE_GENERATION");
    let (mut other, _) = connect_async(&h.http).await.unwrap();
    let other_open = request(&mut other, json!({"id":"other","type":"open"})).await;
    assert_eq!(other_open["data"]["settings"]["search"], defaults);
    let resumed = request(
        &mut other,
        json!({"id":"attach","type":"open","sessionId":sid}),
    )
    .await;
    assert_eq!(
        resumed["data"]["settings"]["search"],
        partial["data"]["settings"]["search"]
    );
    let reset = request(
        &mut other,
        json!({"id":"reset","type":"configure_search","search":defaults}),
    )
    .await;
    let shared = request(&mut ws, json!({"id":"shared","type":"snapshot"})).await;
    assert_eq!(shared["data"]["settings"], reset["data"]["settings"]);
}

#[tokio::test]
async fn configured_pda_reaches_worker_leaves_and_late_replies_cannot_restore_old_values() {
    let config = SessionConfig {
        search: go_core::SearchConfig {
            max_in_flight: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let h = Harness::configured_with_lease(config, Duration::from_secs(5)).await;
    let (tx, mut worker) = h.worker("pda").await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    request(&mut ws, json!({"id":"c","type":"configure_search","search":{"playoutDoublingAdvantage":1.5,"wideRootNoise":0.04}})).await;
    request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;
    let root = next_eval(&mut worker).await;
    assert_eq!(
        root.parameters.as_ref().unwrap().playout_doubling_advantage,
        1.5
    );
    let root_hash = root.input_hash.clone();
    tx.send(result(root)).await.unwrap();
    let leaf = next_eval(&mut worker).await;
    assert_eq!(
        leaf.position.as_ref().unwrap().next_player,
        w::Color::White as i32
    );
    assert_eq!(
        leaf.parameters.as_ref().unwrap().playout_doubling_advantage,
        -1.5
    );
    let changed = request(&mut ws, json!({"id":"switch","type":"configure_search","search":{"playoutDoublingAdvantage":-0.5,"playoutDoublingAdvantagePla":"white"}})).await;
    assert_eq!(changed["data"]["analysis"]["enabled"], true);
    assert_eq!(changed["data"]["analysis"]["visits"], 0);
    assert!(changed["data"]["analysis"]["root"].is_null());
    tx.send(result(leaf)).await.unwrap(); // canceled old generation
    let next = next_eval(&mut worker).await;
    assert!(next.position.as_ref().unwrap().moves.is_empty());
    assert_eq!(
        next.parameters.as_ref().unwrap().playout_doubling_advantage,
        0.5
    );
    assert_ne!(next.input_hash, root_hash);
    let current = request(&mut ws, json!({"id":"current","type":"snapshot"})).await;
    assert_eq!(current["data"]["analysis"]["visits"], 0);
    request(
        &mut ws,
        json!({"id":"off","type":"analyze","enabled":false}),
    )
    .await;
    tx.send(result(next)).await.unwrap();
    let played = request(
        &mut ws,
        json!({"id":"play","type":"play","color":1,"index":60}),
    )
    .await;
    assert_eq!(
        played["data"]["settings"]["search"]["playoutDoublingAdvantagePla"],
        "white"
    );
    request(
        &mut ws,
        json!({"id":"again","type":"analyze","enabled":true}),
    )
    .await;
    let white_root = next_eval(&mut worker).await;
    assert_eq!(
        white_root
            .parameters
            .as_ref()
            .unwrap()
            .playout_doubling_advantage,
        -0.5
    );
}

#[tokio::test]
async fn played_pda_subtrees_are_retained_and_wide_changes_do_not_reset_them() {
    for (pda, reference) in [(0.0, "root"), (0.5, "black"), (0.5, "white"), (0.5, "root")] {
        let mut config = SessionConfig::default();
        config.search.max_in_flight = 1;
        let h = Harness::configured_with_lease(config, Duration::from_secs(10)).await;
        let (tx, mut worker) = h.worker("reuse").await;
        let (mut ws, _) = connect_async(&h.http).await.unwrap();
        request(&mut ws, json!({"id":"open","type":"open"})).await;
        request(&mut ws, json!({"id":"settings","type":"configure_search","search":{
            "playoutDoublingAdvantage":pda,"playoutDoublingAdvantagePla":reference,"wideRootNoise":0.04}})).await;
        request(
            &mut ws,
            json!({"id":"start","type":"analyze","enabled":true}),
        )
        .await;
        for _ in 0..15 {
            let r = next_eval(&mut worker).await;
            let depth = r.position.as_ref().unwrap().moves.len();
            let mut reply = result(r);
            if let Some(w::worker_message::Payload::Result(r)) = &mut reply.payload {
                let policy = &mut r.output.as_mut().unwrap().policy;
                policy.fill(0.0);
                policy[depth] = 1.0;
            }
            tx.send(reply).await.unwrap();
        }
        let held = next_eval(&mut worker).await;
        let paused = request(
            &mut ws,
            json!({"id":"pause","type":"analyze","enabled":false}),
        )
        .await;
        let before = &paused["data"]["analysis"];
        assert_eq!(before["visits"], 15);
        assert_eq!(before["candidates"][0]["index"], 0);
        assert_eq!(before["candidates"][0]["visits"], 14);
        let wide = request(
            &mut ws,
            json!({"id":"wide","type":"configure_search","search":{"wideRootNoise":0.1}}),
        )
        .await;
        assert_eq!(wide["data"]["analysis"]["visits"], before["visits"]);
        assert_eq!(wide["data"]["analysis"]["root"], before["root"]);
        let played = request(
            &mut ws,
            json!({"id":"play","type":"play","color":1,"index":0}),
        )
        .await;
        let analysis = &played["data"]["analysis"];
        let flips = pda != 0.0 && reference == "root";
        assert_eq!(analysis["visits"], if flips { 0 } else { 14 });
        assert_eq!(
            analysis["rootChange"]["retained_visits"],
            analysis["visits"]
        );
        assert_eq!(
            analysis["rootChange"]["reuse_reason"],
            if flips {
                "pda_reference_changed"
            } else {
                "reused"
            }
        );
        tx.send(result(held)).await.unwrap();
        let stable = request(&mut ws, json!({"id":"stable","type":"snapshot"})).await;
        assert_eq!(stable["data"]["analysis"]["visits"], analysis["visits"]);
    }
}

async fn request(socket: &mut Socket, r: Value) -> Value {
    let id = r["id"].clone();
    socket
        .send(Message::Text(r.to_string().into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = socket.next().await.unwrap().unwrap();
            if let Message::Text(text) = msg {
                let v: Value = serde_json::from_str(&text).unwrap();
                if v["type"] == "response" && v["id"] == id {
                    return v;
                }
            }
        }
    })
    .await
    .expect("response timed out")
}

#[tokio::test]
async fn authoritative_moves_atomic_import_history_and_resume() {
    let h = Harness::new().await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    let open = request(&mut ws, json!({"id":"o","type":"open"})).await;
    assert_eq!(open["ok"], true);
    let sid = open["data"]["sessionId"].clone();
    assert!(open["data"]["analysis"]["root"].is_null());
    assert_eq!(open["data"]["board"].as_array().unwrap().len(), 361);
    let played = request(
        &mut ws,
        json!({"id":"p","type":"play","color":1,"index":60}),
    )
    .await;
    assert_eq!(played["data"]["board"][60], 1);
    let bad=request(&mut ws,json!({"id":"b","type":"set_position","moves":[{"color":1,"index":60},{"color":2,"index":60}]})).await;
    assert_eq!(bad["ok"], false);
    let snapshot = request(&mut ws, json!({"id":"s","type":"snapshot"})).await;
    assert_eq!(snapshot["data"]["moves"].as_array().unwrap().len(), 1);
    assert_eq!(
        request(
            &mut ws,
            json!({"id":"p2","type":"play","color":2,"index":61})
        )
        .await["ok"],
        true
    );
    let seek = request(&mut ws, json!({"id":"seek","type":"seek","position":1})).await;
    assert_eq!(seek["data"]["moves"].as_array().unwrap().len(), 2);
    assert_eq!(seek["data"]["board"][61], 0);
    let branch = request(
        &mut ws,
        json!({"id":"branch","type":"play","color":2,"index":62}),
    )
    .await;
    assert_eq!(branch["data"]["moves"][1]["index"], 62);
    let old_generation = branch["data"]["generation"].as_u64().unwrap() - 1;
    let stale = request(
        &mut ws,
        json!({"id":"v","type":"variation","index":1,"generation":old_generation}),
    )
    .await;
    assert_eq!(stale["error"]["code"], "STALE_GENERATION");
    assert_eq!(
        request(&mut ws, json!({"id":"g","type":"genmove"})).await["error"]["code"],
        "NO_WORKERS"
    );
    ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    let resume = request(&mut ws, json!({"id":"r","type":"open","sessionId":sid})).await;
    assert_eq!(resume["data"]["moves"].as_array().unwrap().len(), 2);
    assert_eq!(resume["data"]["board"][62], 2);
}

#[tokio::test]
async fn two_workers_complete_shared_analysis_and_genmove() {
    let h = Harness::new().await;
    let mut bots = vec![];
    for id in ["a", "b"] {
        let (tx, mut incoming) = h.worker(id).await;
        bots.push(tokio::spawn(async move {
            while let Ok(Some(msg)) = incoming.message().await {
                if let Some(w::server_message::Payload::Evaluate(r)) = msg.payload {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    tx.send(result(r)).await.unwrap();
                }
            }
        }));
    }
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    let generated = request(
        &mut ws,
        json!({"id":"g","type":"genmove","maxVisits":12,"maxTimeMs":2000}),
    )
    .await;
    assert_eq!(generated["ok"], true, "{generated}");
    assert_eq!(generated["data"]["moves"].as_array().unwrap().len(), 1);
    assert!(h.pool.views().iter().all(|w| w.completed > 0));
    let before = generated["data"]["position"].clone();
    let pv = request(&mut ws, json!({"id":"v","type":"variation","index":5})).await;
    assert_eq!(pv["ok"], true);
    assert_eq!(
        request(&mut ws, json!({"id":"s","type":"snapshot"})).await["data"]["position"],
        before
    );
    for b in bots {
        b.abort();
    }
}

#[tokio::test]
async fn retry_rejects_old_reply_and_cancel_does_not_play() {
    let h = Harness::new().await;
    let (tx, mut stream) = h.worker("slow").await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;
    let first = loop {
        if let Some(w::server_message::Payload::Evaluate(r)) =
            stream.message().await.unwrap().unwrap().payload
        {
            break r;
        }
    };
    let retry = loop {
        if let Some(w::server_message::Payload::Evaluate(r)) =
            stream.message().await.unwrap().unwrap().payload
        {
            break r;
        }
    };
    assert_ne!(first.task_id, retry.task_id);
    tx.send(result(first)).await.unwrap();
    let check = request(&mut ws, json!({"id":"s","type":"snapshot"})).await;
    assert!(check["data"]["analysis"]["root"].is_null());
    tx.send(result(retry)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let check = request(&mut ws, json!({"id":"s2","type":"snapshot"})).await;
    assert!(!check["data"]["analysis"]["root"].is_null());
    request(
        &mut ws,
        json!({"id":"stop","type":"analyze","enabled":false}),
    )
    .await;
    ws.send(Message::Text(
        json!({"id":"g","type":"genmove","maxTimeMs":3000})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let cancel = request(&mut ws, json!({"id":"c","type":"cancel","requestId":"g"})).await;
    assert_eq!(cancel["ok"], true);
    assert_eq!(cancel["data"]["position"], 0);
}

#[tokio::test]
async fn incompatible_worker_model_rejected_before_dispatch() {
    let h = Harness::new().await;
    let channel = tonic::transport::Endpoint::from_shared(h.grpc.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = WorkerServiceClient::new(channel);
    let (tx, rx) = mpsc::channel(2);
    let mut other = hello("wrong");
    other.model_sha256 = "2".repeat(64);
    tx.send(w::WorkerMessage {
        payload: Some(w::worker_message::Payload::Hello(other)),
    })
    .await
    .unwrap();
    let result = client.connect(ReceiverStream::new(rx)).await;
    assert!(result.is_err());
    assert!(h.pool.views().is_empty());
}

async fn next_eval(stream: &mut tonic::Streaming<w::ServerMessage>) -> w::EvalRequest {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(w::server_message::Payload::Evaluate(r)) =
                stream.message().await.unwrap().unwrap().payload
            {
                return r;
            }
        }
    })
    .await
    .expect("no evaluation arrived")
}
async fn snapshot_when(ws: &mut Socket, predicate: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let r = request(ws, json!({"id":"probe","type":"snapshot"})).await;
            let v = r["data"].clone();
            if predicate(&v) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("snapshot state did not arrive")
}

#[tokio::test]
async fn cancellation_releases_graph_ownership_before_physical_worker_capacity() {
    let h = Harness::new().await;
    let (tx, mut stream) = h.worker("physical").await;
    let mut search =
        go_core::Search::new(go_core::Position::new(19, 7.5).unwrap(), Default::default()).unwrap();
    let go_core::SearchStep::Evaluate(r) = search.next_evaluation().unwrap() else {
        panic!("root evaluation expected")
    };
    let (reply, mailbox) = std::sync::mpsc::channel();
    assert!(h.pool.dispatch("old", &r, reply.clone()));
    assert!(h.pool.dispatch("other", &r, reply.clone()));
    let first = next_eval(&mut stream).await;
    let _second = next_eval(&mut stream).await;
    h.pool.cancel_session("old");
    assert_eq!(h.pool.views()[0].in_flight, 2);
    assert!(
        !h.pool.dispatch("new", &r, reply.clone()),
        "cancel is not an immediate GPU completion"
    );
    tx.send(result(first)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while h.pool.views()[0].in_flight == 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        mailbox.try_recv().is_err(),
        "old value must not reach any graph mailbox"
    );
    assert!(h.pool.dispatch("new", &r, reply));
}

#[tokio::test]
async fn worker_failure_recovers_then_all_workers_pause_without_losing_value() {
    let h = Harness::new().await;
    let (dead_tx, mut dead_stream) = h.worker("lost").await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;
    let _first = next_eval(&mut dead_stream).await;
    let (tx, mut stream) = h.worker("survivor").await;
    let bot = tokio::spawn(async move {
        while let Ok(Some(msg)) = stream.message().await {
            if let Some(w::server_message::Payload::Evaluate(r)) = msg.payload {
                tx.send(result(r)).await.unwrap();
            }
        }
    });
    drop(dead_tx);
    drop(dead_stream);
    let valid = snapshot_when(&mut ws, |v| {
        v["analysis"]["visits"].as_u64().unwrap_or(0) > 3
    })
    .await;
    assert!(!valid["analysis"]["root"].is_null());
    bot.abort();
    let paused = snapshot_when(&mut ws, |v| v["analysis"]["status"] == "waiting_workers").await;
    assert!(
        paused["analysis"]["visits"].as_u64().unwrap()
            >= valid["analysis"]["visits"].as_u64().unwrap()
    );
    assert!(!paused["analysis"]["root"].is_null());
    let (tx, mut stream) = h.worker("recovered").await;
    let bot = tokio::spawn(async move {
        while let Ok(Some(msg)) = stream.message().await {
            if let Some(w::server_message::Payload::Evaluate(r)) = msg.payload {
                tx.send(result(r)).await.unwrap();
            }
        }
    });
    snapshot_when(&mut ws, |v| {
        v["analysis"]["visits"].as_u64().unwrap_or(0)
            > paused["analysis"]["visits"].as_u64().unwrap()
    })
    .await;
    bot.abort();
}

#[tokio::test]
async fn memory_backpressure_retains_root_and_variation_is_read_only_with_first_move() {
    let mut config = SessionConfig::default();
    config.search.max_nodes = 1;
    let h = Harness::configured(config).await;
    let (tx, mut stream) = h.worker("one-node").await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;
    let root = next_eval(&mut stream).await;
    tx.send(result(root)).await.unwrap();
    let limited = snapshot_when(&mut ws, |v| v["analysis"]["status"] == "memory_limited").await;
    assert_eq!(limited["analysis"]["visits"], 1);
    assert_eq!(limited["analysis"]["graphNodes"], 1);
    assert_eq!(
        limited["analysis"]["reason"],
        "configured search node budget reached"
    );
    assert_eq!(limited["analysis"]["limits"]["maxNodes"], 1);
    assert_eq!(limited["analysis"]["nodesPerSecond"], 0.0);
    let diagnostics = h.sessions.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["status"], "memory_limited");
    assert_eq!(diagnostics[0]["graphNodes"], 1);
    assert!(diagnostics[0].get("board").is_none());
    assert_eq!(
        limited["analysis"]["candidates"].as_array().unwrap().len(),
        362
    );
    let first = &limited["analysis"]["candidates"][0];
    assert!(first["winRateBlack"].is_null());
    let pv = request(
        &mut ws,
        json!({"id":"v","type":"variation","index":first["index"]}),
    )
    .await;
    assert_eq!(pv["data"]["available"], true);
    assert_eq!(pv["data"]["moves"][0]["index"], first["index"]);
    let after = request(&mut ws, json!({"id":"s","type":"snapshot"})).await;
    assert_eq!(after["data"]["analysis"]["visits"], 1);
    assert_eq!(after["data"]["position"], 0);
    assert_eq!(after["data"]["analysis"]["enabled"], true);
    assert_eq!(after["data"]["analysis"]["status"], "memory_limited");
    // Continuous analysis resumes on a new position while retaining the limit
    // status and the existing result for the saturated position above.
    request(
        &mut ws,
        json!({"id":"new","type":"play","color":1,"index":first["index"]}),
    )
    .await;
    let next_root = next_eval(&mut stream).await;
    tx.send(result(next_root)).await.unwrap();
    snapshot_when(&mut ws, |v| v["analysis"]["status"] == "memory_limited").await;
}

#[tokio::test]
async fn graph_limit_drains_assigned_leaves_then_preserves_a_stable_snapshot() {
    let mut config = SessionConfig::default();
    config.search.max_nodes = 3;
    config.search.max_in_flight = 2;
    let h = Harness::configured(config).await;
    let (tx, mut stream) = h.worker("three-nodes").await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;
    let root = next_eval(&mut stream).await;
    tx.send(result(root)).await.unwrap();
    let first = next_eval(&mut stream).await;
    let second = next_eval(&mut stream).await;
    tx.send(result(first)).await.unwrap();
    let draining = snapshot_when(&mut ws, |v| v["analysis"]["evaluationsCompleted"] == 2).await;
    assert_eq!(draining["analysis"]["inFlight"], 1);
    assert_ne!(draining["analysis"]["status"], "memory_limited");
    tx.send(result(second)).await.unwrap();
    let limited = snapshot_when(&mut ws, |v| v["analysis"]["status"] == "memory_limited").await;
    assert_eq!(limited["analysis"]["evaluationsCompleted"], 3);
    assert_eq!(limited["analysis"]["inFlight"], 0);
    assert_eq!(limited["analysis"]["graphNodes"], 3);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let after = request(&mut ws, json!({"id":"s","type":"snapshot"})).await;
    assert_eq!(after["data"]["analysis"]["status"], "memory_limited");
    assert_eq!(
        after["data"]["analysis"]["visits"],
        limited["analysis"]["visits"]
    );
    assert_eq!(
        after["data"]["analysis"]["root"],
        limited["analysis"]["root"]
    );
    assert_eq!(h.pool.views()[0].assigned_requests, 3);
}

#[tokio::test]
async fn depth_limit_does_not_latch_before_pending_results_drain() {
    let mut config = SessionConfig::default();
    config.search.max_depth = 1;
    config.search.max_in_flight = 2;
    let h = Harness::configured_with_lease(config, Duration::from_secs(10)).await;
    let (tx, mut stream) = h.worker("depth-limit").await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;
    let root = next_eval(&mut stream).await;
    tx.send(result(root)).await.unwrap();
    let held = next_eval(&mut stream).await;
    // Exhaust all other 19x19 root children, leaving one assigned leaf. The
    // selector now observes depth-limited branches alongside the pending leaf.
    for _ in 0..361 {
        let child = next_eval(&mut stream).await;
        tx.send(result(child)).await.unwrap();
    }
    let draining = snapshot_when(&mut ws, |v| v["analysis"]["evaluationsCompleted"] == 362).await;
    assert_eq!(draining["analysis"]["inFlight"], 1);
    assert_eq!(draining["analysis"]["status"], "analyzing");
    tx.send(result(held)).await.unwrap();
    let limited = snapshot_when(&mut ws, |v| v["analysis"]["status"] == "memory_limited").await;
    assert_eq!(limited["analysis"]["inFlight"], 0);
    assert_eq!(limited["analysis"]["evaluationsCompleted"], 363);
    assert_eq!(
        limited["analysis"]["reason"],
        "configured search depth budget reached"
    );
}

#[tokio::test]
async fn disconnected_session_expires_and_stale_genmove_never_starts() {
    let config = SessionConfig {
        retention: Duration::from_millis(60),
        ..Default::default()
    };
    let h = Harness::configured(config).await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    let opened = request(&mut ws, json!({"id":"o","type":"open"})).await;
    let sid = opened["data"]["sessionId"].clone();
    let stale = request(&mut ws, json!({"id":"g","type":"genmove","generation":0})).await;
    assert_eq!(stale["error"]["code"], "STALE_GENERATION");
    let malformed = request(
        &mut ws,
        json!({"id":"m","type":"set_position","moves":[{"color":1}]}),
    )
    .await;
    assert_eq!(malformed["error"]["code"], "INVALID_REQUEST");
    ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    assert_eq!(
        request(&mut ws, json!({"id":"r","type":"open","sessionId":sid})).await["error"]["code"],
        "SESSION_NOT_FOUND"
    );
}

fn error_result(r: w::EvalRequest, code: &str) -> w::WorkerMessage {
    let mut message = result(r);
    if let Some(w::worker_message::Payload::Result(r)) = &mut message.payload {
        r.output = None;
        r.error_code = code.into();
        r.error_message = "explicit test failure".into();
    }
    message
}

#[tokio::test]
async fn invalid_context_stops_session_without_repeating_or_blaming_worker() {
    let h = Harness::new().await;
    let (tx, mut stream) = h.worker("validator").await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;
    let r = next_eval(&mut stream).await;
    tx.send(error_result(r, "INVALID_CONTEXT")).await.unwrap();
    let failed = snapshot_when(&mut ws, |s| s["analysis"]["status"] == "error").await;
    assert_eq!(failed["analysis"]["enabled"], false);
    assert_eq!(failed["analysis"]["visits"], 0);
    assert_eq!(h.pool.views()[0].failures, 1);
    assert!(h.pool.available());
    assert!(
        tokio::time::timeout(Duration::from_millis(180), stream.message())
            .await
            .is_err(),
        "deterministic context errors must not loop"
    );
}

#[tokio::test]
async fn shortterm_capability_mismatch_retries_without_committing_output() {
    for supports_shortterm in [false, true] {
        let mut config = SessionConfig::default();
        config.search.max_nodes = 1;
        config.search.max_in_flight = 1;
        let h = Harness::configured(config).await;
        let mut capability = hello("capability-check");
        capability.max_in_flight = 1;
        capability.supports_shortterm_error = supports_shortterm;
        let (tx, mut stream) = h.worker_with_hello(capability).await;
        let (mut ws, _) = connect_async(&h.http).await.unwrap();
        request(&mut ws, json!({"id":"o","type":"open"})).await;
        request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;

        let with_shortterm = |r: w::EvalRequest, has_shortterm: bool| {
            let mut message = result(r);
            let Some(w::worker_message::Payload::Result(reply)) = &mut message.payload else {
                unreachable!()
            };
            let output = reply.output.as_mut().unwrap();
            output.has_shortterm_error = has_shortterm;
            output.shortterm_winloss_error = if has_shortterm { 0.05 } else { -1.0 };
            output.shortterm_score_error = if has_shortterm { 1.0 } else { -1.0 };
            message
        };
        let first = next_eval(&mut stream).await;
        tx.send(with_shortterm(first.clone(), !supports_shortterm))
            .await
            .unwrap();
        let retry = next_eval(&mut stream).await;
        assert_ne!(retry.task_id, first.task_id);
        assert_eq!(retry.generation, first.generation);
        assert_eq!(retry.input_hash, first.input_hash);
        let pending = request(&mut ws, json!({"id":"pending","type":"snapshot"})).await;
        assert_eq!(pending["data"]["analysis"]["visits"], 0);
        assert!(pending["data"]["analysis"]["root"].is_null());
        let worker = &h.pool.views()[0];
        assert_eq!(worker.failures, 1);
        assert_eq!(worker.completed, 0);
        assert_eq!(worker.assigned_requests, 2);

        tx.send(with_shortterm(retry, supports_shortterm))
            .await
            .unwrap();
        let completed =
            snapshot_when(&mut ws, |v| v["analysis"]["status"] == "memory_limited").await;
        assert_eq!(completed["analysis"]["visits"], 1);
        assert!(!completed["analysis"]["root"].is_null());
        let worker = &h.pool.views()[0];
        assert_eq!(worker.failures, 1);
        assert_eq!(worker.completed, 1);
    }
}

#[tokio::test]
async fn repeated_evaluator_failure_cools_worker_and_healthy_capacity_recovers() {
    let h = Harness::new().await;
    let (tx, mut stream) = h.worker("broken").await;
    let (mut ws, _) = connect_async(&h.http).await.unwrap();
    request(&mut ws, json!({"id":"o","type":"open"})).await;
    request(&mut ws, json!({"id":"a","type":"analyze","enabled":true})).await;
    for _ in 0..3 {
        let r = next_eval(&mut stream).await;
        tx.send(error_result(r, "EVALUATOR_ERROR")).await.unwrap();
    }
    snapshot_when(&mut ws, |s| s["analysis"]["status"] == "waiting_workers").await;
    assert!(!h.pool.available());
    let channel = tonic::transport::Endpoint::from_shared(h.grpc.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut retry = WorkerServiceClient::new(channel);
    let (hello_tx, hello_rx) = mpsc::channel(1);
    hello_tx
        .send(w::WorkerMessage {
            payload: Some(w::worker_message::Payload::Hello(hello("broken"))),
        })
        .await
        .unwrap();
    assert_eq!(
        retry
            .connect(ReceiverStream::new(hello_rx))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    let (tx, mut stream) = h.worker("healthy").await;
    let bot = tokio::spawn(async move {
        while let Ok(Some(msg)) = stream.message().await {
            if let Some(w::server_message::Payload::Evaluate(r)) = msg.payload {
                tx.send(result(r)).await.unwrap();
            }
        }
    });
    snapshot_when(&mut ws, |s| {
        s["analysis"]["visits"].as_u64().unwrap_or(0) > 3
    })
    .await;
    bot.abort();
}
