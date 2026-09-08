use go_core::{EvalToken, EvaluationRequest};
use go_protocol::{
    INPUT_PROFILE, PROTOCOL_VERSION,
    v1::{self as wire, worker_service_server::WorkerService},
};
use serde::Serialize;
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex, mpsc as sync_mpsc},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

pub type Outcome = (EvalToken, Result<wire::NnOutput, EvalFailure>);
#[derive(Debug)]
pub struct EvalFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl EvalFailure {
    fn retry(message: impl Into<String>) -> Self {
        Self {
            code: "WORKER_UNAVAILABLE".into(),
            message: message.into(),
            retryable: true,
        }
    }
}

#[derive(Clone)]
pub struct WorkerPool {
    inner: Arc<Mutex<PoolState>>,
    lease: Duration,
}
struct PoolState {
    workers: HashMap<String, Worker>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
    model: Option<String>,
    cooldowns: HashMap<String, Instant>,
}
struct Worker {
    hello: wire::WorkerHello,
    connection: String,
    tx: mpsc::Sender<Result<wire::ServerMessage, Status>>,
    last_seen: Instant,
    in_flight: usize,
    retiring: usize,
    completed: u64,
    assigned_requests: u64,
    retired_results: u64,
    failures: u64,
    consecutive_failures: u32,
    latency_ms: f64,
    worker_elapsed_ms: Option<f64>,
    queue_ms: Option<f64>,
    context_ms: Option<f64>,
    evaluator_ms: Option<f64>,
    heartbeat: wire::WorkerHeartbeat,
    heartbeat_sequence: u64,
    last_heartbeat: Option<Instant>,
}
struct Pending {
    session: String,
    worker: String,
    connection: String,
    token: EvalToken,
    hash: Vec<u8>,
    sent: Instant,
    reply: Option<sync_mpsc::Sender<Outcome>>,
    cancelled: Option<Instant>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerView {
    pub id: String,
    pub connected: bool,
    pub connection_id: String,
    pub instance_id: String,
    pub model: String,
    pub capacity: u32,
    pub in_flight: usize,
    pub retiring_in_flight: usize,
    pub completed: u64,
    pub assigned_requests: u64,
    pub retired_results: u64,
    pub failures: u64,
    pub latency_ms: f64,
    pub worker_elapsed_ms: Option<f64>,
    pub queue_ms: Option<f64>,
    pub context_ms: Option<f64>,
    pub evaluator_ms: Option<f64>,
    pub nn_rows: u64,
    pub nn_batches: u64,
    pub heartbeat_sequence: u64,
    pub heartbeat_in_flight: u32,
    pub heartbeat_age_ms: Option<u64>,
    pub backend_info: String,
    pub model_version: u32,
    pub input_profile: String,
    pub engine_commit: String,
    pub supports_shortterm_error: bool,
}

impl WorkerPool {
    pub fn new(model: Option<String>, lease: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolState {
                workers: HashMap::new(),
                pending: HashMap::new(),
                next_id: 1,
                model,
                cooldowns: HashMap::new(),
            })),
            lease,
        }
    }
    pub fn model(&self) -> Option<String> {
        self.inner.lock().unwrap().model.clone()
    }
    pub fn available(&self) -> bool {
        !self.inner.lock().unwrap().workers.is_empty()
    }
    pub fn views(&self) -> Vec<WorkerView> {
        let st = self.inner.lock().unwrap();
        let mut views: Vec<_> = st
            .workers
            .iter()
            .map(|(id, w)| WorkerView {
                id: id.clone(),
                connected: true,
                connection_id: w.connection.clone(),
                instance_id: w.hello.instance_id.clone(),
                model: w.hello.model_sha256.clone(),
                capacity: w.hello.max_in_flight,
                in_flight: w.in_flight,
                retiring_in_flight: w.retiring,
                completed: w.completed,
                assigned_requests: w.assigned_requests,
                retired_results: w.retired_results,
                failures: w.failures,
                latency_ms: w.latency_ms,
                worker_elapsed_ms: w.worker_elapsed_ms,
                queue_ms: w.queue_ms,
                context_ms: w.context_ms,
                evaluator_ms: w.evaluator_ms,
                nn_rows: w.heartbeat.nn_rows,
                nn_batches: w.heartbeat.nn_batches,
                heartbeat_sequence: w.heartbeat_sequence,
                heartbeat_in_flight: w.heartbeat.in_flight,
                heartbeat_age_ms: w.last_heartbeat.map(|t| t.elapsed().as_millis() as u64),
                backend_info: w.hello.backend_info.clone(),
                model_version: w.hello.model_version,
                input_profile: w.hello.input_profile.clone(),
                engine_commit: w.hello.engine_commit.clone(),
                supports_shortterm_error: w.hello.supports_shortterm_error,
            })
            .collect();
        views.sort_by(|a, b| a.id.cmp(&b.id));
        views
    }
    fn register(
        &self,
        hello: wire::WorkerHello,
        tx: mpsc::Sender<Result<wire::ServerMessage, Status>>,
    ) -> Result<String, Status> {
        if hello.protocol_version != PROTOCOL_VERSION
            || hello.input_profile != INPUT_PROFILE
            || !hello.supports_friendly_pass_search
        {
            return Err(Status::failed_precondition(
                "unsupported evaluator protocol/profile",
            ));
        }
        if hello.worker_id.is_empty()
            || hello.worker_id.len() > 128
            || hello.instance_id.is_empty()
            || hello.model_sha256.len() != 64
            || !hello.model_sha256.bytes().all(|x| x.is_ascii_hexdigit())
            || hello.max_in_flight == 0
            || hello.max_in_flight > 4096
            || hello.max_board_size < 19
        {
            return Err(Status::invalid_argument(
                "invalid worker identity, model hash, or capacity",
            ));
        }
        let mut st = self.inner.lock().unwrap();
        if st
            .cooldowns
            .get(&hello.worker_id)
            .is_some_and(|until| *until > Instant::now())
        {
            return Err(Status::unavailable(
                "worker cooling down after repeated evaluator failures",
            ));
        }
        let model = hello.model_sha256.to_ascii_lowercase();
        if st.model.as_ref().is_some_and(|x| x != &model) {
            return Err(Status::failed_precondition(
                "model SHA256 does not match server search profile",
            ));
        }
        if let Some(old) = st.workers.get(&hello.worker_id) {
            let old_connection = old.connection.clone();
            Self::remove_worker(
                &mut st,
                &hello.worker_id,
                &old_connection,
                "worker reconnected",
            );
        }
        st.model = Some(model);
        let connection = Uuid::new_v4().to_string();
        st.workers.insert(
            hello.worker_id.clone(),
            Worker {
                hello,
                connection: connection.clone(),
                tx,
                last_seen: Instant::now(),
                in_flight: 0,
                retiring: 0,
                completed: 0,
                assigned_requests: 0,
                retired_results: 0,
                failures: 0,
                consecutive_failures: 0,
                latency_ms: 0.0,
                worker_elapsed_ms: None,
                queue_ms: None,
                context_ms: None,
                evaluator_ms: None,
                heartbeat: Default::default(),
                heartbeat_sequence: 0,
                last_heartbeat: None,
            },
        );
        Ok(connection)
    }
    // Result mailboxes are bounded by each actor's maximum outstanding evaluations:
    // only the actor can create new work, and every assigned task emits at most one outcome.
    pub fn dispatch(
        &self,
        session: &str,
        request: &EvaluationRequest,
        reply: sync_mpsc::Sender<Outcome>,
    ) -> bool {
        let mut st = self.inner.lock().unwrap();
        let Some(id) = st
            .workers
            .iter()
            .filter(|(_, w)| w.in_flight < (w.hello.max_in_flight as usize) && !w.tx.is_closed())
            .min_by(|(_, a), (_, b)| {
                let a_load = (a.in_flight as f64 + 1.0) / (a.hello.max_in_flight as f64)
                    * a.latency_ms.max(1.0);
                let b_load = (b.in_flight as f64 + 1.0) / (b.hello.max_in_flight as f64)
                    * b.latency_ms.max(1.0);
                a_load.total_cmp(&b_load)
            })
            .map(|(id, _)| id.clone())
        else {
            return false;
        };
        let task_id = st.next_id;
        st.next_id = st.next_id.checked_add(1).expect("task identity exhausted");
        let model = st.model.clone().unwrap();
        let p = &request.position;
        let msg = wire::EvalRequest {
            task_id,
            generation: request.token.generation,
            session_id: session.into(),
            input_hash: request.input_hash.to_vec(),
            model_sha256: model,
            position: Some(wire::Position {
                board_size: p.board_size() as u32,
                komi: p.komi(),
                rules: "chinese".into(),
                moves: p
                    .moves()
                    .iter()
                    .map(|m| wire::Move {
                        color: m.color.stone() as i32,
                        vertex: m.point.map(|x| x as i32).unwrap_or(-1),
                    })
                    .collect(),
                initial_stones: vec![],
                initial_player: wire::Color::Black as i32,
                next_player: p.to_move().stone() as i32,
            }),
            parameters: Some(wire::EvalParameters {
                symmetry: 0,
                policy_temperature: 1.0,
                policy_optimism: 0.0,
                draw_equivalent_wins_for_white: 0.5,
                playout_doubling_advantage: 0.0,
                include_ownership: false,
                max_history: 1000,
                conservative_pass: false,
                enable_passing_hacks: false,
                always_compute_pass_alive: false,
                exclude_territory_adjacent_to_atari: false,
                avoid_mytdagger_hack: false,
                skip_cache: false,
                allow_terminal_search_history: request.allow_terminal_search_history,
                force_non_terminal: request.force_non_terminal,
            }),
            lease_ms: self.lease.as_millis() as u64,
        };
        let w = st.workers.get_mut(&id).unwrap();
        if w.tx
            .try_send(Ok(wire::ServerMessage {
                payload: Some(wire::server_message::Payload::Evaluate(msg)),
            }))
            .is_err()
        {
            return false;
        }
        w.in_flight += 1;
        w.assigned_requests += 1;
        let connection = w.connection.clone();
        st.pending.insert(
            task_id,
            Pending {
                session: session.into(),
                worker: id,
                connection,
                token: request.token,
                hash: request.input_hash.to_vec(),
                sent: Instant::now(),
                reply: Some(reply),
                cancelled: None,
            },
        );
        true
    }
    fn receive(&self, id: &str, connection: &str, msg: wire::WorkerMessage) {
        let mut st = self.inner.lock().unwrap();
        let Some(w) = st
            .workers
            .get_mut(id)
            .filter(|w| w.connection == connection)
        else {
            return;
        };
        w.last_seen = Instant::now();
        match msg.payload {
            Some(wire::worker_message::Payload::Heartbeat(heartbeat)) => {
                w.heartbeat = heartbeat;
                w.heartbeat_sequence += 1;
                w.last_heartbeat = Some(Instant::now());
            }
            Some(wire::worker_message::Payload::Result(mut result)) => {
                let Some(p) = st.pending.get(&result.task_id) else {
                    return;
                };
                if p.worker != id || p.connection != connection {
                    return;
                }
                if result.generation != p.token.generation
                    || result.session_id != p.session
                    || result.input_hash != p.hash
                    || st.model.as_ref() != Some(&result.model_sha256.to_ascii_lowercase())
                {
                    Self::remove_worker(&mut st, id, connection, "worker result identity mismatch");
                    return;
                }
                let p = st.pending.remove(&result.task_id).unwrap();
                let w = st.workers.get_mut(id).unwrap();
                w.in_flight = w.in_flight.saturating_sub(1);
                // A cancelled lease no longer owns a graph node, but retains physical
                // Worker capacity until its result/ack arrives. Never apply its value.
                let Some(reply) = p.reply else {
                    w.retiring = w.retiring.saturating_sub(1);
                    w.retired_results += 1;
                    return;
                };
                if result.error_code.is_empty()
                    && result.output.as_ref().is_some_and(|output| {
                        output.has_shortterm_error != w.hello.supports_shortterm_error
                    })
                {
                    result.error_code = "INVALID_OUTPUT".into();
                    result.error_message =
                        "short-term error capability does not match WorkerHello".into();
                }
                let ms = p.sent.elapsed().as_secs_f64() * 1000.0;
                w.latency_ms = if w.latency_ms == 0.0 {
                    ms
                } else {
                    0.8 * w.latency_ms + 0.2 * ms
                };
                let mut trip_circuit = false;
                let result = if !result.error_code.is_empty() {
                    w.failures += 1;
                    if !matches!(
                        result.error_code.as_str(),
                        "INVALID_CONTEXT"
                            | "CANCELLED"
                            | "LEASE_EXPIRED"
                            | "CAPACITY_EXCEEDED"
                            | "DRAINING"
                    ) {
                        w.consecutive_failures += 1;
                        trip_circuit = w.consecutive_failures >= 3;
                    }
                    Err(EvalFailure {
                        retryable: result.error_code != "INVALID_CONTEXT",
                        code: result.error_code,
                        message: result.error_message,
                    })
                } else if let Some(output) = result.output {
                    w.worker_elapsed_ms = ewma(w.worker_elapsed_ms, Some(result.elapsed_us));
                    w.queue_ms = ewma(w.queue_ms, result.queue_us);
                    w.context_ms = ewma(w.context_ms, result.context_us);
                    w.evaluator_ms = ewma(w.evaluator_ms, result.evaluator_us);
                    w.completed += 1;
                    w.consecutive_failures = 0;
                    Ok(output)
                } else {
                    w.failures += 1;
                    w.consecutive_failures += 1;
                    trip_circuit = w.consecutive_failures >= 3;
                    Err(EvalFailure::retry("empty evaluator result"))
                };
                if trip_circuit {
                    st.cooldowns
                        .insert(id.into(), Instant::now() + Duration::from_secs(10));
                    tracing::warn!(worker=%id,"worker cooling down for 10 seconds after repeated evaluator failures");
                    Self::remove_worker(
                        &mut st,
                        id,
                        connection,
                        "worker repeatedly failed evaluations",
                    );
                }
                let _ = reply.send((p.token, result));
            }
            _ => Self::remove_worker(&mut st, id, connection, "unexpected worker message"),
        }
    }
    fn remove_worker(st: &mut PoolState, id: &str, connection: &str, reason: &str) {
        if !st
            .workers
            .get(id)
            .is_some_and(|w| w.connection == connection)
        {
            return;
        }
        if let Some(w) = st.workers.remove(id) {
            let _ = w.tx.try_send(Err(Status::unavailable(reason.to_owned())));
        }
        let tasks: Vec<_> = st
            .pending
            .iter()
            .filter(|(_, p)| p.worker == id && p.connection == connection)
            .map(|(k, _)| *k)
            .collect();
        for task in tasks {
            if let Some(p) = st.pending.remove(&task)
                && let Some(reply) = p.reply
            {
                let _ = reply.send((p.token, Err(EvalFailure::retry(reason))));
            }
        }
    }
    fn disconnect(&self, id: &str, connection: &str, reason: &str) {
        Self::remove_worker(&mut self.inner.lock().unwrap(), id, connection, reason);
    }
    pub fn cancel_session(&self, session: &str) {
        let mut st = self.inner.lock().unwrap();
        let keys: Vec<_> = st
            .pending
            .iter()
            .filter(|(_, p)| p.session == session && p.reply.is_some())
            .map(|(k, _)| *k)
            .collect();
        for task in keys {
            Self::cancel_task(&mut st, task, None);
        }
    }
    fn cancel_task(st: &mut PoolState, task: u64, error: Option<&str>) {
        let Some(p) = st.pending.get_mut(&task) else {
            return;
        };
        let Some(reply) = p.reply.take() else {
            return;
        };
        p.cancelled = Some(Instant::now());
        if let Some(reason) = error {
            let _ = reply.send((p.token, Err(EvalFailure::retry(reason))));
        }
        if let Some(w) = st
            .workers
            .get_mut(&p.worker)
            .filter(|w| w.connection == p.connection)
        {
            w.retiring += 1;
            if error.is_some() {
                w.failures += 1;
            }
            let _ = w.tx.try_send(Ok(wire::ServerMessage {
                payload: Some(wire::server_message::Payload::Cancel(wire::Cancel {
                    task_id: task,
                    generation: p.token.generation,
                    session_id: p.session.clone(),
                })),
            }));
        }
    }
    pub fn maintenance(&self) {
        let mut st = self.inner.lock().unwrap();
        st.cooldowns.retain(|_, until| *until > Instant::now());
        let dead: Vec<_> = st
            .workers
            .iter()
            .filter(|(_, w)| w.tx.is_closed() || w.last_seen.elapsed() > Duration::from_secs(15))
            .map(|(id, w)| (id.clone(), w.connection.clone()))
            .collect();
        for (id, connection) in dead {
            Self::remove_worker(
                &mut st,
                &id,
                &connection,
                "worker disconnected or heartbeat expired",
            );
        }
        let expired: Vec<_> = st
            .pending
            .iter()
            .filter(|(_, p)| p.reply.is_some() && p.sent.elapsed() > self.lease)
            .map(|(id, _)| *id)
            .collect();
        for task in expired {
            Self::cancel_task(&mut st, task, Some("evaluation lease expired"));
        }
        // A responsive heartbeat cannot keep permanently stuck cancelled work alive.
        // Drop that stream to recover the remaining assignments without overbooking it.
        let stuck: Vec<_> = st
            .pending
            .values()
            .filter(|p| {
                p.cancelled
                    .is_some_and(|t| t.elapsed() > self.lease.max(Duration::from_secs(1)))
            })
            .map(|p| (p.worker.clone(), p.connection.clone()))
            .collect();
        for (id, connection) in stuck {
            Self::remove_worker(
                &mut st,
                &id,
                &connection,
                "cancelled evaluation did not finish within grace period",
            );
        }
    }
}

fn ewma(previous: Option<f64>, micros: Option<u64>) -> Option<f64> {
    micros
        .map(|us| {
            previous.map_or(us as f64 / 1000.0, |old| {
                0.8 * old + 0.2 * us as f64 / 1000.0
            })
        })
        .or(previous)
}

#[tonic::async_trait]
impl WorkerService for WorkerPool {
    type ConnectStream =
        Pin<Box<dyn Stream<Item = Result<wire::ServerMessage, Status>> + Send + 'static>>;
    async fn connect(
        &self,
        request: Request<Streaming<wire::WorkerMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let mut stream = request.into_inner();
        let first = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .map_err(|_| Status::deadline_exceeded("worker hello timeout"))??;
        let Some(wire::WorkerMessage {
            payload: Some(wire::worker_message::Payload::Hello(hello)),
        }) = first
        else {
            return Err(Status::invalid_argument(
                "first message must be WorkerHello",
            ));
        };
        let id = hello.worker_id.clone();
        let (tx, rx) = mpsc::channel((hello.max_in_flight as usize).clamp(1, 4096) * 2 + 8);
        let connection = self.register(hello, tx.clone())?;
        tx.send(Ok(wire::ServerMessage {
            payload: Some(wire::server_message::Payload::Welcome(wire::Welcome {
                protocol_version: PROTOCOL_VERSION,
                connection_id: connection.clone(),
            })),
        }))
        .await
        .map_err(|_| Status::cancelled("connection closed"))?;
        tracing::info!(worker=%id,connection=%connection,"inference worker ready");
        let pool = self.clone();
        tokio::spawn(async move {
            loop {
                match stream.message().await {
                    Ok(Some(msg)) => pool.receive(&id, &connection, msg),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(worker=%id,error=%e,"worker stream ended");
                        break;
                    }
                }
            }
            pool.disconnect(&id, &connection, "worker stream ended");
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
