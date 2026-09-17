use crate::worker::{Outcome, WorkerPool};
use go_core::{
    Color, Completion, Evaluation, EvaluationRequest, Move, Position, Search, SearchConfig,
    SearchStep,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}
impl ApiError {
    pub fn new(code: &str, message: impl ToString) -> Self {
        Self {
            code: code.into(),
            message: message.to_string(),
        }
    }
    pub fn invalid(message: impl ToString) -> Self {
        Self::new("INVALID_REQUEST", message)
    }
}
pub type ApiResult = Result<Value, ApiError>;
type Reply = oneshot::Sender<ApiResult>;

#[derive(Clone)]
pub struct SessionConfig {
    pub search: SearchConfig,
    pub max_sessions: usize,
    pub retention: Duration,
    pub publish: Duration,
    pub max_moves: usize,
}
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            search: SearchConfig::default(),
            max_sessions: 4,
            retention: Duration::from_secs(120),
            publish: Duration::from_millis(300),
            max_moves: 2000,
        }
    }
}

pub struct SessionHandle {
    pub id: String,
    tx: mpsc::SyncSender<Command>,
    control: Arc<Mutex<Control>>,
    pub snapshot: watch::Receiver<Value>,
}
impl SessionHandle {
    pub fn request(
        &self,
        connection: String,
        request: Value,
    ) -> Result<oneshot::Receiver<ApiResult>, ApiError> {
        let (tx, rx) = oneshot::channel();
        let control = self.control.lock().unwrap();
        let epoch = control
            .subscriptions
            .get(&connection)
            .copied()
            .ok_or_else(|| {
                ApiError::new(
                    "SESSION_NOT_FOUND",
                    "connection is not attached to this session",
                )
            })?;
        self.tx
            .try_send(Command::Request {
                connection,
                epoch,
                request,
                reply: tx,
            })
            .map_err(|e| ApiError::new("CAPACITY", e))?;
        Ok(rx)
    }
    pub async fn attach(&self, connection: String) -> ApiResult {
        let (tx, rx) = oneshot::channel();
        {
            let mut control = self.control.lock().unwrap();
            let epoch = control.next_epoch();
            self.tx
                .try_send(Command::Attach(connection.clone(), epoch, tx))
                .map_err(|e| ApiError::new("CAPACITY", e))?;
            // Commit only after successful enqueue. Holding this short lock also
            // prevents the actor from observing the command before its identity.
            control.subscriptions.insert(connection, epoch);
        }
        rx.await
            .map_err(|_| ApiError::new("SESSION_NOT_FOUND", "session closed"))?
    }
    pub fn detach(&self, connection: String) {
        let mut control = self.control.lock().unwrap();
        let epoch = control.next_epoch();
        control.subscriptions.remove(&connection);
        control.detached.insert(connection, epoch);
    }
    pub fn shutdown(&self) {
        self.control.lock().unwrap().shutdown = true;
    }
}
// Control operations cannot be starved by a full request queue, and never block
// a Tokio thread waiting for the CPU search actor. Detach identities coalesce.
#[derive(Default)]
struct Control {
    detached: HashMap<String, u64>,
    subscriptions: HashMap<String, u64>,
    epoch: u64,
    shutdown: bool,
}
impl Control {
    fn next_epoch(&mut self) -> u64 {
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("subscription identity exhausted");
        self.epoch
    }
}
#[derive(Clone)]
pub struct Sessions {
    handles: Arc<Mutex<HashMap<String, Arc<SessionHandle>>>>,
    config: SessionConfig,
    pool: WorkerPool,
}
impl Sessions {
    pub fn new(config: SessionConfig, pool: WorkerPool) -> Self {
        Self {
            handles: Arc::new(Mutex::new(HashMap::new())),
            config,
            pool,
        }
    }
    pub fn open(&self, id: Option<&str>) -> Result<Arc<SessionHandle>, ApiError> {
        let mut map = self.handles.lock().unwrap();
        map.retain(|_, h| h.snapshot.has_changed().is_ok());
        if let Some(id) = id {
            return map
                .get(id)
                .cloned()
                .ok_or_else(|| ApiError::new("SESSION_NOT_FOUND", "session expired or unknown"));
        }
        if map.len() >= self.config.max_sessions {
            return Err(ApiError::new(
                "CAPACITY",
                "maximum retained sessions reached",
            ));
        }
        let id = Uuid::new_v4().to_string();
        let position = Position::new(19, 7.5).map_err(ApiError::invalid)?;
        let search =
            Search::new(position, self.config.search.clone()).map_err(ApiError::invalid)?;
        let (tx, rx) = mpsc::sync_channel(256);
        let control = Arc::new(Mutex::new(Control::default()));
        let (results_tx, results_rx) = mpsc::channel();
        let (snapshot_tx, snapshot_rx) = watch::channel(Value::Null);
        let mut actor = Actor {
            id: id.clone(),
            config: self.config.clone(),
            pool: self.pool.clone(),
            commands: rx,
            control: control.clone(),
            results_tx,
            results_rx,
            snapshots: snapshot_tx,
            search,
            line: vec![],
            cursor: 0,
            komi: 7.5,
            subscribers: HashMap::new(),
            unsubscribed_since: Some(Instant::now()),
            enabled: false,
            status: "idle".into(),
            reason: None,
            generation: 1,
            version: 0,
            pending: None,
            unassigned: None,
            budget: None,
            retry_after: Instant::now(),
            last_publish: Instant::now(),
            last_rate: Instant::now(),
            last_visits: 0,
            rate: 0.0,
        };
        actor.publish();
        let handle = Arc::new(SessionHandle {
            id: id.clone(),
            tx,
            control,
            snapshot: snapshot_rx,
        });
        thread::Builder::new()
            .name(format!("search-{}", &id[..8]))
            .spawn(move || actor.run())
            .map_err(ApiError::invalid)?;
        map.insert(id, handle.clone());
        Ok(handle)
    }
    pub fn shutdown(&self) {
        for h in self.handles.lock().unwrap().values() {
            h.shutdown();
        }
    }
    pub fn count(&self) -> usize {
        self.handles.lock().unwrap().len()
    }
    pub fn configuration(&self) -> Value {
        json!({
            "search": self.config.search,
            "searchSimdSelected": self.config.search.simd.resolve().ok(),
            "publishMs": self.config.publish.as_millis(),
            "retentionSecs": self.config.retention.as_secs_f64(),
            "maxSessions": self.config.max_sessions,
            "maxMoves": self.config.max_moves,
            "workerScheduling": self.pool.scheduling(),
        })
    }
}

enum Command {
    Attach(String, u64, Reply),
    Detach(String, u64),
    Request {
        connection: String,
        epoch: u64,
        request: Value,
        reply: Reply,
    },
    Shutdown,
}
struct Budget {
    start_visits: u64,
    max_visits: Option<u64>,
    deadline: Option<Instant>,
}
struct Genmove {
    connection: String,
    epoch: u64,
    request_id: String,
    reply: Reply,
    previous_enabled: bool,
}
struct Actor {
    id: String,
    config: SessionConfig,
    pool: WorkerPool,
    commands: mpsc::Receiver<Command>,
    results_tx: mpsc::Sender<Outcome>,
    results_rx: mpsc::Receiver<Outcome>,
    control: Arc<Mutex<Control>>,
    snapshots: watch::Sender<Value>,
    search: Search,
    line: Vec<Move>,
    cursor: usize,
    komi: f64,
    subscribers: HashMap<String, u64>,
    unsubscribed_since: Option<Instant>,
    enabled: bool,
    status: String,
    reason: Option<String>,
    generation: u64,
    version: u64,
    pending: Option<Genmove>,
    unassigned: Option<EvaluationRequest>,
    budget: Option<Budget>,
    retry_after: Instant,
    last_publish: Instant,
    last_rate: Instant,
    last_visits: u64,
    rate: f64,
}
#[derive(Deserialize)]
struct JsonMove {
    color: u8,
    index: Option<u16>,
}
impl TryFrom<JsonMove> for Move {
    type Error = ApiError;
    fn try_from(m: JsonMove) -> Result<Self, ApiError> {
        Ok(Self {
            color: color(m.color)?,
            point: m.index,
        })
    }
}
fn color(c: u8) -> Result<Color, ApiError> {
    match c {
        1 => Ok(Color::Black),
        2 => Ok(Color::White),
        _ => Err(ApiError::invalid("color must be 1 (black) or 2 (white)")),
    }
}
fn move_json(m: Move) -> Value {
    json!({"color":m.color.stone(),"index":m.point})
}
fn budget(request: &Value, visits: u64, genmove: bool) -> Result<Budget, ApiError> {
    let max_visits = match request.get("maxVisits") {
        Some(v) => Some(
            v.as_u64()
                .filter(|x| *x > 0)
                .ok_or_else(|| ApiError::invalid("maxVisits must be a positive integer"))?,
        ),
        None => {
            if genmove {
                Some(256)
            } else {
                None
            }
        }
    };
    let duration = match request.get("maxTimeMs") {
        Some(v) => Some(
            v.as_u64()
                .filter(|x| *x > 0)
                .ok_or_else(|| ApiError::invalid("maxTimeMs must be a positive integer"))?,
        ),
        None => {
            if genmove {
                Some(3000)
            } else {
                None
            }
        }
    };
    let deadline = duration
        .map(|ms| {
            Instant::now()
                .checked_add(Duration::from_millis(ms))
                .ok_or_else(|| ApiError::invalid("maxTimeMs is too large"))
        })
        .transpose()?;
    Ok(Budget {
        start_visits: visits,
        max_visits,
        deadline,
    })
}

impl Actor {
    fn run(&mut self) {
        loop {
            let (shutdown, detached) = {
                let mut c = self.control.lock().unwrap();
                (
                    std::mem::take(&mut c.shutdown),
                    std::mem::take(&mut c.detached),
                )
            };
            if shutdown {
                self.command(Command::Shutdown);
                self.stop_tasks();
                return;
            }
            for (connection, epoch) in detached {
                self.command(Command::Detach(connection, epoch));
            }
            while let Ok(outcome) = self.results_rx.try_recv() {
                self.complete_evaluation(outcome);
            }
            for _ in 0..32 {
                match self.commands.try_recv() {
                    Ok(c) => {
                        if !self.command(c) {
                            self.stop_tasks();
                            return;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.stop_tasks();
                        return;
                    }
                }
            }
            if self
                .unsubscribed_since
                .is_some_and(|t| t.elapsed() > self.config.retention)
            {
                self.stop_tasks();
                return;
            }
            let search_ready = self.pump();
            if self.last_publish.elapsed() >= self.config.publish {
                self.publish();
            }
            if search_ready {
                // Yield to control/results/commands before continuing CPU work.
                // Exhausting a work quantum does not imply waiting for the NN.
                continue;
            }
            if self.enabled && !self.subscribers.is_empty() && self.pool.available() {
                // NN completions should wake the active actor immediately so it
                // can refill the worker. The timeout still bounds the wait before
                // polling commands/control; idle actors wait on commands below.
                if let Ok(outcome) = self.results_rx.recv_timeout(Duration::from_millis(1)) {
                    self.complete_evaluation(outcome);
                }
                continue;
            }
            match self.commands.recv_timeout(Duration::from_millis(20)) {
                Ok(c) => {
                    if !self.command(c) {
                        self.stop_tasks();
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.stop_tasks();
                    return;
                }
            }
        }
    }
    fn complete_evaluation(&mut self, outcome: Outcome) {
        let (token, result) = outcome.into_parts();
        match result {
            Ok(v) => {
                let evaluation = Evaluation {
                    policy: v.policy.into_iter().map(f64::from).collect(),
                    white_win_prob: v.white_win_prob,
                    white_loss_prob: v.white_loss_prob,
                    white_no_result_prob: v.white_no_result_prob,
                    white_score_mean: v.white_score_mean,
                    white_score_mean_sq: v.white_score_mean_sq,
                    white_lead: v.white_lead,
                    shortterm_winloss_error: v.shortterm_winloss_error,
                    shortterm_score_error: v.shortterm_score_error,
                    has_shortterm_error: v.has_shortterm_error,
                    ownership: v.ownership.into_iter().map(f64::from).collect(),
                };
                if let Err(e) = self.search.complete(token, evaluation) {
                    self.fail_analysis("INVALID_EVALUATION", e.to_string());
                }
            }
            Err(reason) => {
                if self.search.fail(token) == Completion::Applied {
                    if reason.retryable {
                        self.reason = Some(reason.message);
                        self.retry_after = Instant::now() + Duration::from_millis(100);
                    } else {
                        self.fail_analysis(&reason.code, reason.message);
                    }
                }
            }
        }
    }
    fn command(&mut self, c: Command) -> bool {
        match c {
            Command::Shutdown => {
                self.cancel_genmove("service shutting down");
                return false;
            }
            Command::Attach(id, epoch, reply) => {
                if self.control.lock().unwrap().subscriptions.get(&id) != Some(&epoch) {
                    let _ = reply.send(Err(ApiError::new(
                        "SESSION_NOT_FOUND",
                        "subscription was superseded or disconnected",
                    )));
                    return true;
                }
                self.subscribers.insert(id, epoch);
                self.unsubscribed_since = None;
                let value = self.publish();
                let _ = reply.send(Ok(value));
            }
            Command::Detach(id, epoch) => {
                if self
                    .subscribers
                    .get(&id)
                    .is_some_and(|attached| *attached <= epoch)
                {
                    self.subscribers.remove(&id);
                }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.connection == id && p.epoch <= epoch)
                {
                    self.cancel_genmove("client disconnected");
                }
                if self.subscribers.is_empty() {
                    self.unsubscribed_since = Some(Instant::now());
                    self.stop_tasks();
                    self.status = "idle".into();
                    self.reason = Some("analysis paused while disconnected".into());
                    self.publish();
                }
            }
            Command::Request {
                connection,
                epoch,
                request,
                reply,
            } => {
                if self.subscribers.get(&connection) != Some(&epoch)
                    || self.control.lock().unwrap().subscriptions.get(&connection) != Some(&epoch)
                {
                    let _ = reply.send(Err(ApiError::new(
                        "SESSION_NOT_FOUND",
                        "connection is not attached to this session",
                    )));
                    return true;
                }
                if request
                    .get("generation")
                    .is_some_and(|g| g.as_u64() != Some(self.generation))
                {
                    let _ = reply.send(Err(ApiError::new(
                        "STALE_GENERATION",
                        "request targets an old analysis generation",
                    )));
                    return true;
                }
                let kind = request.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "genmove" {
                    match self.begin_genmove(&request) {
                        Ok(()) => {
                            self.pending = Some(Genmove {
                                connection,
                                epoch,
                                request_id: request["id"].as_str().unwrap_or("").into(),
                                reply,
                                previous_enabled: self.enabled,
                            });
                            self.enabled = true;
                            self.status = "analyzing".into();
                            self.reason = None;
                            self.generation += 1;
                            self.publish();
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                } else {
                    let result = self.execute(&connection, &request);
                    let _ = reply.send(result);
                }
            }
        }
        true
    }
    fn begin_genmove(&mut self, r: &Value) -> Result<(), ApiError> {
        if self.pending.is_some() {
            return Err(ApiError::new("BUSY", "genmove already in progress"));
        }
        if self.search.position().terminal().is_some() {
            return Err(ApiError::new("GAME_OVER", "game has ended"));
        }
        if self.cursor >= self.config.max_moves {
            return Err(ApiError::new(
                "CAPACITY",
                "game exceeds configured move history limit",
            ));
        }
        if let Some(c) = r.get("color") {
            let c = c
                .as_u64()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(|| ApiError::invalid("invalid color"))?;
            if color(c)? != self.search.position().to_move() {
                return Err(ApiError::new("ILLEGAL_MOVE", "wrong player to move"));
            }
        }
        if !self.pool.available() && self.search.root_visits() == 0 {
            return Err(ApiError::new(
                "NO_WORKERS",
                "no compatible inference worker is connected",
            ));
        }
        self.budget = Some(budget(r, self.search.root_visits(), true)?);
        Ok(())
    }
    fn execute(&mut self, connection: &str, r: &Value) -> ApiResult {
        if let Some(generation) = r.get("generation")
            && generation.as_u64() != Some(self.generation)
        {
            return Err(ApiError::new(
                "STALE_GENERATION",
                "request targets an old analysis generation",
            ));
        }
        match r.get("type").and_then(Value::as_str).unwrap_or("") {
            "snapshot" => Ok(self.publish()),
            "analyze" => {
                let enabled = r
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| ApiError::invalid("enabled must be boolean"))?;
                let budget = if enabled {
                    Some(budget(r, self.search.root_visits(), false)?)
                } else {
                    None
                };
                self.cancel_genmove("analysis command replaced genmove");
                self.stop_tasks();
                self.enabled = enabled;
                self.budget = budget;
                self.generation += 1;
                self.status = if enabled { "analyzing" } else { "idle" }.into();
                self.reason = None;
                Ok(self.publish())
            }
            "cancel" => {
                let target = r
                    .get("requestId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::invalid("requestId is required"))?;
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.connection == connection && p.request_id == target)
                {
                    self.cancel_genmove("request cancelled");
                    self.generation += 1;
                }
                Ok(self.publish())
            }
            "variation" => {
                let point = parse_point(r)?;
                let first = Move {
                    color: self.search.position().to_move(),
                    point,
                };
                // Candidate PVs include their first move. Search::variation instead
                // returns only the continuation after its supplied prefix.
                let pv = self
                    .search
                    .snapshot(362, 30)
                    .candidates
                    .into_iter()
                    .find(|c| c.mv == first)
                    .map(|c| c.pv)
                    .unwrap_or_default();
                Ok(
                    json!({"available":!pv.is_empty(),"moves":pv.into_iter().map(move_json).collect::<Vec<_>>(),"generation":self.generation,"version":self.version}),
                )
            }
            kind @ ("play" | "undo" | "seek" | "new_game" | "configure" | "set_position") => {
                let mut line = self.line.clone();
                let mut cursor = self.cursor;
                let mut komi = self.komi;
                if let Some(k) = r.get("komi") {
                    komi = k
                        .as_f64()
                        .ok_or_else(|| ApiError::invalid("komi must be numeric"))?;
                }
                if let Some(rules) = r.get("rules")
                    && rules.as_str() != Some("chinese")
                {
                    return Err(ApiError::new(
                        "UNSUPPORTED",
                        "first release supports Chinese rules only",
                    ));
                }
                if let Some(size) = r.get("boardSize")
                    && size.as_u64() != Some(19)
                {
                    return Err(ApiError::new(
                        "UNSUPPORTED",
                        "first release supports 19x19 only",
                    ));
                }
                match kind {
                    "play" => {
                        let c = r
                            .get("color")
                            .and_then(Value::as_u64)
                            .and_then(|c| u8::try_from(c).ok())
                            .ok_or_else(|| ApiError::invalid("color is required"))?;
                        line.truncate(cursor);
                        line.push(Move {
                            color: color(c)?,
                            point: parse_point(r)?,
                        });
                        cursor = line.len();
                    }
                    "undo" => {
                        if cursor == 0 {
                            return Err(ApiError::new("ILLEGAL_MOVE", "no move to undo"));
                        }
                        cursor -= 1;
                        line.truncate(cursor);
                    }
                    "seek" => {
                        cursor = r
                            .get("position")
                            .and_then(Value::as_u64)
                            .and_then(|x| usize::try_from(x).ok())
                            .ok_or_else(|| ApiError::invalid("position must be an integer"))?;
                    }
                    "new_game" => {
                        line.clear();
                        cursor = 0;
                    }
                    "set_position" => {
                        if r.get("moves")
                            .and_then(Value::as_array)
                            .is_some_and(|moves| moves.iter().any(|m| m.get("index").is_none()))
                        {
                            return Err(ApiError::invalid(
                                "every move requires an index, with null for pass",
                            ));
                        }
                        let moves: Vec<JsonMove> = serde_json::from_value(
                            r.get("moves")
                                .cloned()
                                .ok_or_else(|| ApiError::invalid("moves are required"))?,
                        )
                        .map_err(ApiError::invalid)?;
                        line = moves
                            .into_iter()
                            .map(Move::try_from)
                            .collect::<Result<Vec<_>, _>>()?;
                        cursor = match r.get("position") {
                            Some(p) => p
                                .as_u64()
                                .and_then(|x| usize::try_from(x).ok())
                                .ok_or_else(|| ApiError::invalid("position must be an integer"))?,
                            None => line.len(),
                        };
                    }
                    _ => {}
                }
                if line.len() > self.config.max_moves {
                    return Err(ApiError::new(
                        "CAPACITY",
                        "game exceeds configured move history limit",
                    ));
                }
                if cursor > line.len() {
                    return Err(ApiError::invalid("position exceeds move history"));
                }
                // Validate the whole imported mainline before modifying any authoritative state.
                Position::replay(19, komi, &line).map_err(|e| ApiError::new("ILLEGAL_MOVE", e))?;
                let position = Position::replay(19, komi, &line[..cursor])
                    .map_err(|e| ApiError::new("ILLEGAL_MOVE", e))?;
                self.cancel_genmove("position changed");
                self.stop_tasks();
                // Preserve monotonic core task identity across changes of komi.
                self.search.set_root(position).map_err(ApiError::invalid)?;
                self.line = line;
                self.cursor = cursor;
                self.komi = komi;
                self.budget = None;
                self.generation += 1;
                self.last_visits = self.search.root_visits();
                self.last_rate = Instant::now();
                self.rate = 0.0;
                self.status = if self.enabled { "analyzing" } else { "idle" }.into();
                self.reason = None;
                Ok(self.publish())
            }
            _ => Err(ApiError::new("UNSUPPORTED", "unknown command")),
        }
    }
    fn stop_tasks(&mut self) {
        self.pool.cancel_session(&self.id);
        self.search.cancel_all();
        self.unassigned = None;
    }
    fn cancel_genmove(&mut self, reason: &str) {
        if let Some(p) = self.pending.take() {
            self.stop_tasks();
            self.enabled = p.previous_enabled;
            self.budget = None;
            self.status = if self.enabled { "analyzing" } else { "idle" }.into();
            let _ = p.reply.send(Err(ApiError::new("CANCELLED", reason)));
        }
    }
    fn fail_analysis(&mut self, code: &str, reason: String) {
        self.stop_tasks();
        self.enabled = false;
        self.status = "error".into();
        self.reason = Some(reason.clone());
        self.budget = None;
        if let Some(p) = self.pending.take() {
            let _ = p.reply.send(Err(ApiError::new(code, reason)));
        }
        self.publish();
    }
    /// True only when the CPU quantum ended without finding a reason to wait.
    fn pump(&mut self) -> bool {
        if !self.enabled || self.subscribers.is_empty() {
            return false;
        }
        // Keep the user's continuous-analysis intent for the next position, but
        // do not repeatedly scan a saturated graph. Root/analyze commands reset
        // this status and allow work again.
        if self.status == "memory_limited" {
            return false;
        }
        let expired = self.budget.as_ref().is_some_and(|b| {
            b.max_visits
                .is_some_and(|max| self.search.root_visits().saturating_sub(b.start_visits) >= max)
                || b.deadline.is_some_and(|d| Instant::now() >= d)
        });
        if expired {
            self.finish_budget();
            return false;
        }
        if self.search.position().terminal().is_some() {
            let _ = self.search.next_evaluation();
            self.enabled = false;
            self.status = "finished".into();
            self.budget = None;
            self.stop_tasks();
            self.publish();
            return false;
        }
        if !self.pool.available() {
            self.status = "waiting_workers".into();
            self.reason = Some("No compatible inference workers".into());
            return false;
        }
        if Instant::now() < self.retry_after {
            return false;
        }
        self.status = "analyzing".into();
        self.reason = None;
        let start = Instant::now();
        for _ in 0..32 {
            if let Some(request) = self.unassigned.take()
                && !self
                    .pool
                    .dispatch(&self.id, &request, self.results_tx.clone())
            {
                self.unassigned = Some(request);
                return false;
            }
            if start.elapsed() > Duration::from_millis(3) {
                return true;
            }
            match self.search.next_evaluation() {
                Ok(SearchStep::Evaluate(request)) => {
                    if !self
                        .pool
                        .dispatch(&self.id, &request, self.results_tx.clone())
                    {
                        self.unassigned = Some(request);
                        return false;
                    }
                }
                Ok(SearchStep::Advanced) => {}
                Ok(SearchStep::Waiting) => return false,
                Ok(SearchStep::MemoryLimited) => {
                    // An outstanding replay may temporarily consume space needed
                    // by a new leaf. Its completion frees that space; do not latch
                    // a permanent stop or cancel a genmove while it can recover.
                    if self.search.in_flight() == 0 {
                        self.status = "memory_limited".into();
                        self.reason = Some(
                            "search graph capacity reached; change position or increase the graph budget to continue".into(),
                        );
                        if self.pending.is_some() {
                            self.finish_budget();
                        } else {
                            self.budget = None;
                        }
                        self.publish();
                    }
                    return false;
                }
                Ok(SearchStep::DepthLimited) => {
                    // A shallower in-flight node may expose more searchable
                    // descendants when it completes, just like memory pressure.
                    if self.search.in_flight() == 0 {
                        self.status = "memory_limited".into();
                        self.reason = Some("configured search depth budget reached".into());
                        if self.pending.is_some() {
                            self.finish_budget();
                        } else {
                            self.budget = None;
                        }
                        self.publish();
                    }
                    return false;
                }
                Ok(SearchStep::Terminal) => {
                    self.finish_budget();
                    return false;
                }
                Err(e) => {
                    self.fail_analysis("SEARCH_ERROR", e.to_string());
                    return false;
                }
            }
        }
        true
    }
    fn finish_budget(&mut self) {
        self.stop_tasks();
        self.budget = None;
        if let Some(p) = self.pending.take() {
            self.enabled = p.previous_enabled;
            let choice = self.search.snapshot(1, 1).candidates.first().map(|c| c.mv);
            if self.search.root_visits() == 0 || choice.is_none() {
                self.status = "waiting_workers".into();
                self.reason = Some("no evaluated legal move available".into());
                let _ = p.reply.send(Err(ApiError::new(
                    "NO_WORKERS",
                    "no evaluated legal move available",
                )));
                self.publish();
                return;
            }
            let mv = choice.unwrap();
            let mut position = self.search.position().clone();
            if let Err(e) = position.play(mv) {
                let _ = p.reply.send(Err(ApiError::new("ILLEGAL_MOVE", e)));
                return;
            }
            if let Err(e) = self.search.set_root(position) {
                let _ = p.reply.send(Err(ApiError::new("CAPACITY", e)));
                return;
            }
            self.line.truncate(self.cursor);
            self.line.push(mv);
            self.cursor = self.line.len();
            self.generation += 1;
            self.last_visits = self.search.root_visits();
            self.last_rate = Instant::now();
            self.rate = 0.0;
            self.status = if self.enabled { "analyzing" } else { "idle" }.into();
            self.reason = None;
            let value = self.publish();
            let _ = p.reply.send(Ok(value));
        } else {
            self.enabled = false;
            self.status = "finished".into();
            self.reason = None;
            self.publish();
        }
    }
    fn publish(&mut self) -> Value {
        // Send all root candidates so clients can choose a display percentage.
        let ss = self.search.snapshot(362, 30);
        let p = self.search.position();
        let dt = self.last_rate.elapsed().as_secs_f64();
        if dt >= 0.25 {
            self.rate = ss.root.visits.saturating_sub(self.last_visits) as f64 / dt;
            self.last_visits = ss.root.visits;
            self.last_rate = Instant::now();
        }
        let root = if ss.root.visits > 0 {
            json!({"winRateBlack":((1.0-ss.root.win_loss_value)/2.0).clamp(0.0,1.0),"scoreLeadBlack":-ss.root.score_mean})
        } else {
            Value::Null
        };
        let candidates:Vec<_>=ss.candidates.iter().map(|c|json!({"index":c.mv.point,"color":c.mv.color.stone(),"prior":c.prior,"visits":c.visits,"weight":c.weight,
            "winRateBlack":if c.visits>0{Some(((1.0-c.stats.win_loss_value)/2.0).clamp(0.0,1.0))}else{None},
            "scoreLeadBlack":if c.visits>0{Some(-c.stats.score_mean)}else{None},"pv":c.pv.iter().copied().map(move_json).collect::<Vec<_>>()})).collect();
        let white_placed = p
            .moves()
            .iter()
            .filter(|m| m.color == Color::White && m.point.is_some())
            .count();
        let black_placed = p
            .moves()
            .iter()
            .filter(|m| m.color == Color::Black && m.point.is_some())
            .count();
        let captures = json!({"black":white_placed.saturating_sub(p.board().iter().filter(|x|**x==2).count()),"white":black_placed.saturating_sub(p.board().iter().filter(|x|**x==1).count())});
        self.version += 1;
        let value = json!({"type":"snapshot","sessionId":self.id,"generation":self.generation,"version":self.version,"boardSize":19,"board":p.board(),
            "moves":self.line.iter().copied().map(move_json).collect::<Vec<_>>(),"position":self.cursor,"toPlay":p.to_move().stone(),"captures":captures,
            "settings":{"komi":self.komi,"rules":"chinese"},"terminal":p.terminal(),"analysis":{"enabled":self.enabled,"status":self.status,"reason":self.reason,"root":root,"candidates":candidates,
                "visits":ss.root.visits,"nodesPerSecond":self.rate,"graphNodes":ss.nodes,"memoryBytes":ss.memory_bytes,"inFlight":ss.in_flight,
                "evaluationsCompleted":ss.evaluations_completed,"transpositionHits":ss.transposition_hits,"catchUpVisits":ss.catch_up_visits},"workers":self.pool.snapshot_views()});
        self.snapshots.send_replace(value.clone());
        self.last_publish = Instant::now();
        value
    }
}
fn parse_point(r: &Value) -> Result<Option<u16>, ApiError> {
    match r.get("index") {
        Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|x| *x < 361)
            .map(|x| Some(x as u16))
            .ok_or_else(|| ApiError::invalid("index must be 0..360 or null")),
        None => Err(ApiError::invalid("index is required")),
    }
}
