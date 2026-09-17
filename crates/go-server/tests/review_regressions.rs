//! Independent review regressions. These use isolated actors and no inference workers.
use futures_util::{SinkExt, StreamExt};
use go_protocol::{
    INPUT_PROFILE, PROTOCOL_VERSION,
    v1::{
        self as wire, worker_service_client::WorkerServiceClient,
        worker_service_server::WorkerServiceServer,
    },
};
use go_server::{
    app::App,
    session::{SessionConfig, Sessions},
    worker::WorkerPool,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::test]
async fn reattach_same_connection_must_not_be_deleted_by_older_detach() {
    let pool = WorkerPool::new(None, Duration::from_secs(1));
    let sessions = Sessions::new(SessionConfig::default(), pool);
    let handle = sessions.open(None).unwrap();
    let connection = "same-socket-switching-back".to_owned();
    handle.attach(connection.clone()).await.unwrap();
    // Let the idle actor enter recv_timeout. Detach must precede reattach even
    // though the two operations currently use different ingress mechanisms.
    tokio::time::sleep(Duration::from_millis(5)).await;
    handle.detach(connection.clone());
    handle.attach(connection.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let result = handle
        .request(connection, json!({"type":"snapshot"}))
        .unwrap()
        .await
        .unwrap();
    sessions.shutdown();
    assert!(
        result.is_ok(),
        "a newer successful attach must remain valid: {result:?}"
    );
}

#[tokio::test]
async fn later_detach_must_not_leave_an_older_queued_attach_alive() {
    let sessions = Sessions::new(
        SessionConfig {
            retention: Duration::from_millis(50),
            ..Default::default()
        },
        WorkerPool::new(None, Duration::from_secs(1)),
    );
    let handle = sessions.open(None).unwrap();
    let connection = "disconnected-before-attach-completes".to_owned();
    let mut attaching = Box::pin(handle.attach(connection.clone()));
    let first = futures_util::poll!(attaching.as_mut());
    handle.detach(connection);
    if first.is_pending() {
        let _ = tokio::time::timeout(Duration::from_secs(1), attaching)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        sessions.open(Some(&handle.id)).is_err(),
        "an old attach must not keep a disconnected session alive"
    );
    sessions.shutdown();
}

struct Harness {
    sessions: Sessions,
    http: String,
    grpc: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Harness {
    async fn new() -> Self {
        let pool = WorkerPool::new(None, Duration::from_secs(5));
        let config = SessionConfig {
            publish: Duration::from_millis(10),
            ..Default::default()
        };
        let sessions = Sessions::new(config, pool.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = format!("ws://{}/ws", listener.local_addr().unwrap());
        let app = App::new(sessions.clone(), pool.clone());
        let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc = format!("http://{}", grpc_listener.local_addr().unwrap());
        let tasks = vec![
            tokio::spawn(async move {
                axum::serve(listener, app.router()).await.unwrap();
            }),
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(WorkerServiceServer::new(pool))
                    .serve_with_incoming(TcpListenerStream::new(grpc_listener))
                    .await
                    .unwrap();
            }),
        ];
        Self {
            sessions,
            http,
            grpc,
            tasks,
        }
    }
    async fn idle_worker(
        &self,
    ) -> (
        mpsc::Sender<wire::WorkerMessage>,
        tonic::Streaming<wire::ServerMessage>,
    ) {
        let channel = tonic::transport::Endpoint::from_shared(self.grpc.clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = WorkerServiceClient::new(channel);
        let (tx, rx) = mpsc::channel(8);
        tx.send(wire::WorkerMessage {
            payload: Some(wire::worker_message::Payload::Hello(wire::WorkerHello {
                worker_id: "subscription-review-fixture".into(),
                instance_id: "test".into(),
                protocol_version: PROTOCOL_VERSION,
                model_sha256: "1".repeat(64),
                model_version: 15,
                input_profile: INPUT_PROFILE.into(),
                max_in_flight: 2,
                max_board_size: 19,
                supports_friendly_pass_search: true,
                ..Default::default()
            })),
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
            Some(wire::server_message::Payload::Welcome(_))
        ));
        (tx, stream)
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.sessions.shutdown();
        for task in &self.tasks {
            task.abort();
        }
    }
}
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn request(socket: &mut Socket, value: Value) -> Value {
    let id = value["id"].clone();
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let reply: Value = serde_json::from_str(&text).unwrap();
                if reply["type"] == "response" && reply["id"] == id {
                    return reply;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn websocket_switching_a_b_a_preserves_the_current_subscription() {
    let harness = Harness::new().await;
    let (mut socket, _) = connect_async(&harness.http).await.unwrap();
    let a =
        request(&mut socket, json!({"id":"a","type":"open"})).await["data"]["sessionId"].clone();
    let b =
        request(&mut socket, json!({"id":"b","type":"open"})).await["data"]["sessionId"].clone();
    for turn in 0..20 {
        for session in [&a, &b, &a] {
            assert_eq!(
                request(
                    &mut socket,
                    json!({"id":format!("open-{turn}-{session}"),"type":"open","sessionId":session})
                )
                .await["ok"],
                true
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        let snapshot = request(
            &mut socket,
            json!({"id":format!("state-{turn}"),"type":"snapshot"}),
        )
        .await;
        assert_eq!(snapshot["ok"], true, "{snapshot}");
        assert_eq!(snapshot["data"]["sessionId"], a);
    }
}

#[tokio::test]
async fn old_subscription_genmove_is_cancelled_while_new_subscription_remains_usable() {
    let harness = Harness::new().await;
    let (_worker, mut incoming) = harness.idle_worker().await;
    let handle = harness.sessions.open(None).unwrap();
    let connection = "same-socket".to_owned();
    handle.attach(connection.clone()).await.unwrap();
    let old = handle
        .request(
            connection.clone(),
            json!({"type":"genmove","id":"old","maxTimeMs":10000}),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                incoming.message().await.unwrap().unwrap().payload,
                Some(wire::server_message::Payload::Evaluate(_))
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    handle.detach(connection.clone());
    handle.attach(connection.clone()).await.unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(1), old)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(cancelled.code, "CANCELLED");
    let mut new = handle
        .request(
            connection.clone(),
            json!({"type":"genmove","id":"new","maxTimeMs":10000}),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        matches!(
            new.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "new subscription's genmove must stay active"
    );
    let snapshot = handle
        .request(connection.clone(), json!({"type":"snapshot"}))
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["position"], 0);
    assert_eq!(snapshot["analysis"]["enabled"], true);
    handle.detach(connection);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), new)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        "CANCELLED"
    );
}

#[test]
fn configuration_reports_the_actual_search_and_session_limits() {
    let mut config = SessionConfig {
        max_sessions: 11,
        max_moves: 999,
        publish: Duration::from_millis(75),
        retention: Duration::from_secs(17),
        ..Default::default()
    };
    config.search.max_in_flight = 7;
    config.search.max_nodes = 1234;
    config.search.simd = go_core::SearchSimd::Scalar;
    let sessions = Sessions::new(config, WorkerPool::new(None, Duration::from_secs(1)));
    let actual = sessions.configuration();
    assert_eq!(actual["search"]["max_in_flight"], 7);
    assert_eq!(actual["search"]["max_nodes"], 1234);
    assert_eq!(actual["search"]["simd"], "scalar");
    assert_eq!(actual["searchSimdSelected"], "scalar");
    assert_eq!(actual["publishMs"], 75);
    assert_eq!(actual["retentionSecs"], 17.0);
    assert_eq!(actual["maxSessions"], 11);
    assert_eq!(actual["maxMoves"], 999);
}

/// Opt in when the sibling frontend and Node.js are installed. The fixture uses
/// ephemeral ports and an idle synthetic worker; it does not touch the live service.
#[tokio::test]
#[ignore = "requires sibling FrontEnd checkout and Node.js"]
async fn production_frontend_passive_attach_preserves_another_connections_genmove() {
    let harness = Harness::new().await;
    let (_worker, _incoming) = harness.idle_worker().await;
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .join("FrontEnd/scripts/session_resume_live.mjs");
    assert!(
        script.is_file(),
        "frontend integration script is missing: {}",
        script.display()
    );
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("node")
            .arg(script)
            .arg(&harness.http)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["genmoveSurvivedPassiveAttach"], true);
    assert_eq!(report["observerRequests"], json!(["open"]));
}
