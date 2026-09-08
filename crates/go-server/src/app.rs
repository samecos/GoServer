use crate::{
    session::{ApiError, ApiResult, SessionHandle, Sessions},
    worker::WorkerPool,
};
use axum::{
    Json, Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use uuid::Uuid;

#[derive(Clone)]
pub struct App {
    pub sessions: Sessions,
    pub workers: WorkerPool,
    connections: Arc<Semaphore>,
}
impl App {
    pub fn new(sessions: Sessions, workers: WorkerPool) -> Self {
        Self {
            sessions,
            workers,
            connections: Arc::new(Semaphore::new(64)),
        }
    }
    pub fn router(self) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/api/workers", get(workers))
            .route("/ws", get(upgrade))
            .with_state(self)
    }
}
async fn health(State(app): State<App>) -> Json<Value> {
    Json(
        json!({"status":"ok","protocolVersion":1,"sessions":app.sessions.count(),"workers":app.workers.views().len(),"modelSha256":app.workers.model(),"configuration":app.sessions.configuration()}),
    )
}
async fn workers(State(app): State<App>) -> Json<Value> {
    Json(json!({"workers":app.workers.views()}))
}
async fn upgrade(State(app): State<App>, ws: WebSocketUpgrade) -> impl IntoResponse {
    let permit = app.connections.clone().try_acquire_owned();
    ws.max_message_size(1024 * 1024)
        .on_upgrade(move |socket| async move {
            if let Ok(_permit) = permit {
                serve_socket(socket, app).await;
            }
        })
}
fn response(id: &str, result: ApiResult) -> Value {
    match result {
        Ok(data) => json!({"type":"response","id":id,"ok":true,"data":data}),
        Err(error) => json!({"type":"response","id":id,"ok":false,"error":error}),
    }
}

async fn deliver_response(
    out: mpsc::Sender<Value>,
    pending: Arc<Mutex<HashSet<String>>>,
    id: String,
    receiver: oneshot::Receiver<ApiResult>,
) {
    let result = receiver
        .await
        .unwrap_or_else(|_| Err(ApiError::new("SESSION_NOT_FOUND", "session closed")));
    // An admitted request retains its slot while its full response waits for
    // the bounded writer queue. Slow readers cannot grow untracked send tasks.
    let _ = out.send(response(&id, result)).await;
    pending.lock().unwrap().remove(&id);
}

async fn serve_socket(socket: WebSocket, app: App) {
    let connection = Uuid::new_v4().to_string();
    let (mut sink, mut incoming) = socket.split();
    let (out, mut replies) = mpsc::channel::<Value>(64);
    let (snapshot_tx, mut snapshots) = watch::channel(Value::Null);
    let mut writer = tokio::spawn(async move {
        loop {
            let value = tokio::select! {
                biased;
                v=replies.recv()=>match v{Some(v)=>v,None=>break},
                change=snapshots.changed()=>{if change.is_err(){break;}let v=snapshots.borrow_and_update().clone();if v.is_null(){continue;}v},
            };
            if !matches!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    sink.send(Message::Text(value.to_string().into()))
                )
                .await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    });
    let mut attached: Option<Arc<SessionHandle>> = None;
    let mut forward: Option<tokio::task::JoinHandle<()>> = None;
    let pending = Arc::new(Mutex::new(HashSet::<String>::new()));
    loop {
        let next = tokio::select! { _=&mut writer=>break, next=incoming.next()=>next };
        let Some(Ok(message)) = next else {
            break;
        };
        let text = match message {
            Message::Text(s) => s,
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => {
                let _ = out
                    .send(response(
                        "",
                        Err(ApiError::invalid("JSON text messages required")),
                    ))
                    .await;
                continue;
            }
        };
        let r: Value = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                let _ = out.send(response("", Err(ApiError::invalid(e)))).await;
                continue;
            }
        };
        let Some(id) = r
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128)
        else {
            let _ = out
                .send(response(
                    "",
                    Err(ApiError::invalid("nonempty string id required")),
                ))
                .await;
            continue;
        };
        let id = id.to_owned();
        if r.get("type").and_then(Value::as_str) == Some("open") {
            let desired = r.get("sessionId").and_then(Value::as_str);
            let result = match app.sessions.open(desired) {
                Ok(handle) => {
                    let snapshot = handle.attach(connection.clone()).await;
                    if snapshot.is_ok() {
                        if let Some(old) = attached.take()
                            && old.id != handle.id
                        {
                            old.detach(connection.clone());
                        }
                        if let Some(f) = forward.take() {
                            f.abort();
                        }
                        let mut rx = handle.snapshot.clone();
                        let tx = snapshot_tx.clone();
                        forward = Some(tokio::spawn(async move {
                            loop {
                                tx.send_replace(rx.borrow_and_update().clone());
                                if rx.changed().await.is_err() {
                                    break;
                                }
                            }
                        }));
                        attached = Some(handle);
                    }
                    snapshot
                }
                Err(e) => Err(e),
            };
            if out.send(response(&id, result)).await.is_err() {
                break;
            }
            continue;
        }
        let Some(session) = attached.as_ref() else {
            let _ = out
                .send(response(
                    &id,
                    Err(ApiError::new("SESSION_NOT_FOUND", "open a session first")),
                ))
                .await;
            continue;
        };
        if r.get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|v| v != session.id)
        {
            let _ = out
                .send(response(
                    &id,
                    Err(ApiError::new(
                        "SESSION_NOT_FOUND",
                        "session is not attached to this connection",
                    )),
                ))
                .await;
            continue;
        }
        let accepted = {
            let mut p = pending.lock().unwrap();
            p.len() < 32 && p.insert(id.clone())
        };
        if !accepted {
            let _ = out
                .send(response(
                    &id,
                    Err(ApiError::new(
                        "CAPACITY",
                        "duplicate id or too many pending requests",
                    )),
                ))
                .await;
            continue;
        }
        match session.request(connection.clone(), r) {
            Ok(receiver) => {
                let out = out.clone();
                let pending = pending.clone();
                tokio::spawn(deliver_response(out, pending, id, receiver));
            }
            Err(e) => {
                pending.lock().unwrap().remove(&id);
                let _ = out.send(response(&id, Err(e))).await;
            }
        }
    }
    if let Some(f) = forward {
        f.abort();
    }
    if let Some(session) = attached {
        session.detach(connection);
    }
    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn full_writer_queue_keeps_all_response_requests_admitted_until_sent() {
        let (out, mut incoming) = mpsc::channel(1);
        out.send(json!({"occupied":true})).await.unwrap();
        let pending = Arc::new(Mutex::new(
            (0..32).map(|i| i.to_string()).collect::<HashSet<_>>(),
        ));
        let mut tasks = Vec::new();
        for i in 0..32 {
            let (tx, rx) = oneshot::channel();
            tx.send(Ok(json!({"board":vec![0;361]}))).unwrap();
            tasks.push(tokio::spawn(deliver_response(
                out.clone(),
                pending.clone(),
                i.to_string(),
                rx,
            )));
        }
        tokio::task::yield_now().await;
        assert_eq!(
            pending.lock().unwrap().len(),
            32,
            "blocked response senders must retain the admission limit"
        );
        assert!(tasks.iter().all(|task| !task.is_finished()));
        incoming.recv().await.unwrap();
        for _ in 0..32 {
            incoming.recv().await.unwrap();
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disconnected_writer_releases_response_admission_slots() {
        let (out, incoming) = mpsc::channel(1);
        let pending = Arc::new(Mutex::new(HashSet::from(["request".into()])));
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(json!({}))).unwrap();
        drop(incoming);
        deliver_response(out, pending.clone(), "request".into(), rx).await;
        assert!(pending.lock().unwrap().is_empty());
    }
}
