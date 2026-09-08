//! GTP v2 adapter over the same authoritative session actor as WebSocket.
//! KataGo analysis framing/units follow the pinned docs/GTP_Extensions.md.
use crate::session::{ApiError, ApiResult, SessionHandle, Sessions};
use serde_json::{Value, json};
use std::{io, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

const MAX_LINE: usize = 16 * 1024;
const COMMANDS: &[&str] = &[
    "protocol_version",
    "name",
    "version",
    "list_commands",
    "known_command",
    "boardsize",
    "clear_board",
    "komi",
    "get_komi",
    "play",
    "undo",
    "genmove",
    "quit",
    "kata-analyze",
    "lz-analyze",
    "stop",
    "kata-get-rules",
    "kata-set-rules",
    "final_score",
];
enum Input {
    Line(Vec<u8>),
    TooLong,
    Error(io::Error),
    End,
}
#[derive(Debug, PartialEq)]
struct Command {
    id: String,
    name: String,
    args: Vec<String>,
}
struct ParseError {
    id: String,
    message: String,
}

fn parse(line: &[u8]) -> Result<Option<Command>, ParseError> {
    let cleaned: Vec<u8> = line
        .iter()
        .copied()
        .filter(|b| *b >= 32 || *b == b'\t')
        .take_while(|b| *b != b'#')
        .collect();
    let text = std::str::from_utf8(&cleaned)
        .map_err(|_| ParseError {
            id: String::new(),
            message: "invalid UTF-8".into(),
        })?
        .trim();
    if text.is_empty() {
        return Ok(None);
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let id = text[..digits].to_owned();
    if !id.is_empty() && id.parse::<u32>().is_err() {
        return Err(ParseError {
            id: String::new(),
            message: "invalid command id".into(),
        });
    }
    let mut tokens = text[digits..].split_whitespace();
    let name = tokens.next().ok_or_else(|| ParseError {
        id: id.clone(),
        message: "empty command".into(),
    })?;
    Ok(Some(Command {
        id,
        name: name.into(),
        args: tokens.map(str::to_owned).collect(),
    }))
}
fn parse_color(s: &str) -> Result<u8, String> {
    match s.to_ascii_lowercase().as_str() {
        "b" | "black" => Ok(1),
        "w" | "white" => Ok(2),
        _ => Err("invalid color".into()),
    }
}
fn parse_vertex(s: &str) -> Result<Option<u16>, String> {
    if s.eq_ignore_ascii_case("pass") {
        return Ok(None);
    }
    let bytes = s.as_bytes();
    if !(2..=3).contains(&bytes.len()) || !bytes[0].is_ascii_alphabetic() {
        return Err("invalid vertex".into());
    }
    let letter = bytes[0].to_ascii_uppercase();
    if !(b'A'..=b'T').contains(&letter) || letter == b'I' {
        return Err("invalid vertex".into());
    }
    let row = s[1..].parse::<u16>().map_err(|_| "invalid vertex")?;
    if !(1..=19).contains(&row) {
        return Err("invalid vertex".into());
    }
    let col = (letter - b'A' - u8::from(letter > b'I')) as u16;
    Ok(Some((19 - row) * 19 + col))
}
fn vertex(point: &Value) -> Option<String> {
    if point.is_null() {
        return Some("pass".into());
    }
    let p = point.as_u64().filter(|p| *p < 361)?;
    let col = p % 19;
    let letter = (b'A' + col as u8 + u8::from(col >= 8)) as char;
    Some(format!("{letter}{}", 19 - p / 19))
}
fn rules() -> Value {
    json!({"ko":"SIMPLE","scoring":"AREA","tax":"NONE","suicide":false,"hasButton":false,"whiteHandicapBonus":"N","friendlyPassOk":true})
}
fn exact_args(c: &Command, n: usize) -> Result<(), String> {
    if c.args.len() == n {
        Ok(())
    } else {
        Err(format!("expected {n} argument(s)"))
    }
}
fn failure(e: ApiError) -> String {
    format!("{}: {}", e.code, e.message)
}
async fn call(handle: &SessionHandle, connection: &str, request: Value) -> Result<Value, String> {
    handle
        .request(connection.into(), request)
        .map_err(failure)?
        .await
        .map_err(|_| "session closed".to_owned())?
        .map_err(failure)
}
async fn reply<W: AsyncWrite + Unpin>(
    out: &mut W,
    id: &str,
    result: Result<String, String>,
) -> io::Result<()> {
    let (prefix, body) = match result {
        Ok(s) => ('=', s),
        Err(s) => ('?', s.replace(['\r', '\n'], " ")),
    };
    let text = if body.is_empty() {
        format!("{prefix}{id}\n\n")
    } else {
        format!("{prefix}{id} {body}\n\n")
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        out.write_all(text.as_bytes()).await?;
        out.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "GTP client is not reading"))?
}

#[derive(Clone, Debug)]
struct Analysis {
    lz: bool,
    color: u8,
    interval: Duration,
    max_moves: usize,
    root_info: bool,
}
fn analysis_args(c: &Command, to_play: u8) -> Result<Analysis, String> {
    let mut a = Analysis {
        lz: c.name == "lz-analyze",
        color: to_play,
        interval: Duration::from_millis(300),
        max_moves: 20,
        root_info: false,
    };
    let mut i = 0;
    if let Some(color) = c.args.first().and_then(|s| parse_color(s).ok()) {
        a.color = color;
        i += 1;
    }
    if a.color != to_play {
        return Err("analysis player must match player to move".into());
    }
    let parse_interval = |s: &str| -> Result<Duration, String> {
        let cs: f64 = s.parse().map_err(|_| "invalid interval")?;
        if !cs.is_finite() || !(0.0..=360000.0).contains(&cs) {
            return Err("interval must be 0..360000 centiseconds".into());
        }
        // Zero asks for fastest publication. The service's snapshot cadence remains authoritative.
        Ok(Duration::from_secs_f64((cs * 0.01).max(0.001)))
    };
    if let Some(s) = c.args.get(i)
        && s.parse::<f64>().is_ok()
    {
        a.interval = parse_interval(s)?;
        i += 1;
    }
    while i < c.args.len() {
        let key = &c.args[i];
        let value = c
            .args
            .get(i + 1)
            .ok_or_else(|| "missing analysis option value".to_owned())?;
        match key.as_str() {
            "interval" => a.interval = parse_interval(value)?,
            "maxmoves" => {
                a.max_moves = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=20).contains(n))
                    .ok_or("maxmoves must be in 1..20")?;
            }
            "rootInfo" if !a.lz => {
                a.root_info = match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err("rootInfo must be true or false".into()),
                }
            }
            _ => return Err(format!("unsupported analysis option: {key}")),
        }
        i += 2;
    }
    Ok(a)
}
fn analysis_line(snapshot: &Value, a: &Analysis) -> Option<String> {
    let stats = &snapshot["analysis"];
    let candidates = stats["candidates"].as_array()?;
    let mut chunks = Vec::new();
    for candidate in candidates.iter().take(a.max_moves) {
        let Some(black) = candidate["winRateBlack"].as_f64().filter(|v| v.is_finite()) else {
            continue;
        };
        let Some(mv) = vertex(&candidate["index"]) else {
            continue;
        };
        let prior = candidate["prior"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
        let wr = (if a.color == 1 { black } else { 1.0 - black }).clamp(0.0, 1.0);
        let visits = candidate["visits"].as_u64().unwrap_or(0);
        let mut line = format!("info move {mv} visits {visits}");
        if a.lz {
            line.push_str(&format!(
                " winrate {:.0} prior {:.0}",
                (wr * 10000.0).round(),
                (prior * 10000.0).round()
            ));
        } else {
            line.push_str(&format!(" winrate {wr:.6} prior {prior:.6}"));
            if let Some(mean) = candidate["scoreLeadBlack"]
                .as_f64()
                .filter(|x| x.is_finite())
            {
                line.push_str(&format!(
                    " scoreSelfplay {:.6}",
                    if a.color == 1 { mean } else { -mean }
                ));
            }
        }
        line.push_str(&format!(" order {} pv", chunks.len()));
        if let Some(pv) = candidate["pv"].as_array() {
            for mv in pv {
                if let Some(v) = vertex(&mv["index"]) {
                    line.push(' ');
                    line.push_str(&v);
                }
            }
        }
        chunks.push(line);
    }
    if a.root_info
        && let Some(black) = stats["root"]["winRateBlack"]
            .as_f64()
            .filter(|x| x.is_finite())
    {
        let wr = if a.color == 1 { black } else { 1.0 - black };
        let mut root = format!(
            "rootInfo visits {} winrate {wr:.6}",
            stats["visits"].as_u64().unwrap_or(0)
        );
        if let Some(score) = stats["root"]["scoreLeadBlack"]
            .as_f64()
            .filter(|x| x.is_finite())
        {
            root.push_str(&format!(
                " scoreSelfplay {:.6}",
                if a.color == 1 { score } else { -score }
            ));
        }
        chunks.push(root);
    }
    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join(" ") + "\n")
    }
}
fn generated_move(snapshot: Value) -> Result<String, String> {
    let cursor = snapshot["position"]
        .as_u64()
        .ok_or("missing move position")? as usize;
    if cursor == 0 {
        return Err("genmove returned no move".into());
    }
    vertex(&snapshot["moves"][cursor - 1]["index"]).ok_or_else(|| "invalid generated move".into())
}
struct Pending {
    id: String,
    request_id: String,
    receiver: oneshot::Receiver<ApiResult>,
}
async fn finish_pending<W: AsyncWrite + Unpin>(out: &mut W, pending: Pending) -> io::Result<()> {
    let value = pending
        .receiver
        .await
        .map_err(|_| "session closed".to_owned())
        .and_then(|r| r.map_err(failure))
        .and_then(generated_move);
    reply(out, &pending.id, value).await
}

async fn ordinary(c: &Command, handle: &SessionHandle, connection: &str) -> Result<String, String> {
    match c.name.as_str() {
        "protocol_version" => {
            exact_args(c, 0)?;
            Ok("2".into())
        }
        "name" => {
            exact_args(c, 0)?;
            Ok("Go Server".into())
        }
        "version" => {
            exact_args(c, 0)?;
            Ok(env!("CARGO_PKG_VERSION").into())
        }
        "list_commands" => {
            exact_args(c, 0)?;
            Ok(COMMANDS.join("\n"))
        }
        "known_command" => {
            exact_args(c, 1)?;
            Ok(COMMANDS.contains(&c.args[0].as_str()).to_string())
        }
        "boardsize" => {
            exact_args(c, 1)?;
            if c.args[0] != "19" {
                return Err("unacceptable size: only 19 supported".into());
            }
            call(
                handle,
                connection,
                json!({"type":"new_game","boardSize":19}),
            )
            .await?;
            Ok(String::new())
        }
        "clear_board" => {
            exact_args(c, 0)?;
            call(handle, connection, json!({"type":"new_game"})).await?;
            Ok(String::new())
        }
        "komi" => {
            exact_args(c, 1)?;
            let komi = c.args[0]
                .parse::<f64>()
                .ok()
                .filter(|k| k.is_finite())
                .ok_or("invalid komi")?;
            call(handle, connection, json!({"type":"configure","komi":komi})).await?;
            Ok(String::new())
        }
        "get_komi" => {
            exact_args(c, 0)?;
            Ok(
                call(handle, connection, json!({"type":"snapshot"})).await?["settings"]["komi"]
                    .to_string(),
            )
        }
        "play" => {
            exact_args(c, 2)?;
            let color = parse_color(&c.args[0])?;
            let index = parse_vertex(&c.args[1])?;
            call(
                handle,
                connection,
                json!({"type":"play","color":color,"index":index}),
            )
            .await?;
            Ok(String::new())
        }
        "undo" => {
            exact_args(c, 0)?;
            call(handle, connection, json!({"type":"undo"})).await?;
            Ok(String::new())
        }
        "stop" => {
            exact_args(c, 0)?;
            call(
                handle,
                connection,
                json!({"type":"analyze","enabled":false}),
            )
            .await?;
            Ok(String::new())
        }
        "kata-get-rules" => {
            exact_args(c, 0)?;
            Ok(rules().to_string())
        }
        "kata-set-rules" => {
            let text = c.args.join(" ");
            if text != "chinese"
                && serde_json::from_str::<Value>(&text).ok().as_ref() != Some(&rules())
            {
                return Err("unsupported rules: only chinese preset supported".into());
            }
            call(
                handle,
                connection,
                json!({"type":"configure","rules":"chinese"}),
            )
            .await?;
            Ok(String::new())
        }
        "final_score" => {
            exact_args(c, 0)?;
            let snapshot = call(handle, connection, json!({"type":"snapshot"})).await?;
            let terminal = &snapshot["terminal"];
            if terminal["kind"] == "no_result" {
                return Err("game ended without a result".into());
            }
            let score = terminal["white_minus_black"]
                .as_f64()
                .ok_or("cannot score: game is not terminal")?;
            if score == 0.0 {
                Ok("0".into())
            } else {
                Ok(format!(
                    "{}+{:.1}",
                    if score > 0.0 { "W" } else { "B" },
                    score.abs()
                ))
            }
        }
        "quit" => {
            exact_args(c, 0)?;
            Ok(String::new())
        }
        _ => Err("unknown command".into()),
    }
}

async fn serve_incoming<W: AsyncWrite + Unpin>(
    mut incoming: mpsc::Receiver<Input>,
    mut output: W,
    sessions: Sessions,
) -> io::Result<()> {
    let handle = match sessions.open(None) {
        Ok(h) => h,
        Err(e) => {
            reply(&mut output, "", Err(failure(e))).await?;
            return Ok(());
        }
    };
    let connection = uuid::Uuid::new_v4().to_string();
    if let Err(e) = handle.attach(connection.clone()).await {
        reply(&mut output, "", Err(failure(e))).await?;
        return Ok(());
    }
    let result = async {
        let mut pending: Option<Pending> = None;
        let mut analyzing: Option<Analysis> = None;
        let mut snapshots = handle.snapshot.clone();
        let mut publish = tokio::time::interval(Duration::from_millis(300));
        publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_version = None;
        let mut serial = 0_u64;
        loop {
            tokio::select! {
                biased;
                input = incoming.recv() => {
                    // KataGo's streaming analysis ends on any input line, including blank/comment lines.
                    if analyzing.take().is_some() {
                        let _ = call(&handle, &connection, json!({"type":"analyze","enabled":false})).await;
                        output.write_all(b"\n").await?; output.flush().await?;
                    }
                    if let Some(p) = pending.take() {
                        let _ = call(&handle, &connection, json!({"type":"cancel","requestId":p.request_id})).await;
                        finish_pending(&mut output, p).await?;
                    }
                    let line = match input {
                        Some(Input::Line(line)) => line,
                        Some(Input::TooLong) => { reply(&mut output, "", Err("command exceeds 16384 bytes".into())).await?; continue; }
                        Some(Input::Error(e)) => return Err(e),
                        Some(Input::End) | None => break,
                    };
                    let c = match parse(&line) { Ok(Some(c)) => c, Ok(None) => continue, Err(e) => { reply(&mut output, &e.id, Err(e.message)).await?; continue; } };
                    if c.name == "genmove" {
                        let result = exact_args(&c, 1).and_then(|_| parse_color(&c.args[0]));
                        match result {
                            Ok(color) => {
                                serial += 1;
                                let request_id = format!("gtp-{serial}");
                                match handle.request(connection.clone(), json!({"type":"genmove","id":request_id,"color":color})) {
                                    Ok(receiver) => pending = Some(Pending { id:c.id, request_id, receiver }),
                                    Err(e) => reply(&mut output, &c.id, Err(failure(e))).await?,
                                }
                            }
                            Err(e) => reply(&mut output, &c.id, Err(e)).await?,
                        }
                    } else if c.name == "kata-analyze" || c.name == "lz-analyze" {
                        let to_play = handle.snapshot.borrow()["toPlay"].as_u64().unwrap_or(1) as u8;
                        match analysis_args(&c, to_play) {
                            Ok(a) => match call(&handle, &connection, json!({"type":"analyze","enabled":true})).await {
                                Ok(_) => {
                                    output.write_all(format!("={}\n", c.id).as_bytes()).await?; output.flush().await?;
                                    publish = tokio::time::interval(a.interval); publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                                    analyzing = Some(a); last_version = None;
                                }
                                Err(e) => reply(&mut output, &c.id, Err(e)).await?,
                            }
                            Err(e) => reply(&mut output, &c.id, Err(e)).await?,
                        }
                    } else {
                        let result = ordinary(&c, &handle, &connection).await;
                        let quit = c.name == "quit" && result.is_ok();
                        reply(&mut output, &c.id, result).await?;
                        if quit { break; }
                    }
                }
                received = async { (&mut pending.as_mut().expect("guarded pending").receiver).await }, if pending.is_some() => {
                    let p = pending.take().expect("guarded pending");
                    let result = received.map_err(|_| "session closed".to_owned()).and_then(|r| r.map_err(failure)).and_then(generated_move);
                    reply(&mut output, &p.id, result).await?;
                }
                _ = publish.tick(), if analyzing.is_some() => {
                    let snapshot = snapshots.borrow_and_update().clone();
                    let version = snapshot["version"].as_u64();
                    if last_version == version { continue; }
                    last_version = version;
                    if let Some(line) = analysis_line(&snapshot, analyzing.as_ref().expect("guarded analysis")) {
                        tokio::time::timeout(Duration::from_secs(5), async { output.write_all(line.as_bytes()).await?; output.flush().await }).await
                            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "GTP client is not reading"))??;
                    }
                }
            }
        }
        Ok(())
    }.await;
    handle.detach(connection);
    result
}

async fn read_lines<R: AsyncRead + Unpin>(input: R, tx: mpsc::Sender<Input>) {
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let buffer = match reader.fill_buf().await {
            Ok(b) => b,
            Err(e) => {
                let _ = tx.send(Input::Error(e)).await;
                return;
            }
        };
        if buffer.is_empty() {
            if too_long {
                let _ = tx.send(Input::TooLong).await;
            } else if !line.is_empty() {
                let _ = tx.send(Input::Line(line)).await;
            }
            let _ = tx.send(Input::End).await;
            return;
        }
        let newline = buffer.iter().position(|b| *b == b'\n');
        let consumed = newline.map_or(buffer.len(), |i| i + 1);
        if !too_long {
            if line.len() + consumed > MAX_LINE {
                line.clear();
                too_long = true;
            } else {
                line.extend_from_slice(&buffer[..consumed]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            let item = if too_long {
                Input::TooLong
            } else {
                Input::Line(std::mem::take(&mut line))
            };
            too_long = false;
            if tx.send(item).await.is_err() {
                return;
            }
        }
    }
}

/// Exposed for embedding and in-memory protocol verification.
pub async fn serve_connection<R, W>(input: R, output: W, sessions: Sessions) -> io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (tx, rx) = mpsc::channel(16);
    let reader = tokio::spawn(read_lines(input, tx));
    let result = serve_incoming(rx, output, sessions).await;
    reader.abort();
    result
}
pub async fn serve_tcp(
    address: SocketAddr,
    sessions: Sessions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    serve_listener(listener, sessions).await
}
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    sessions: Sessions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let (socket, _) = listener.accept().await?;
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let (r, w) = socket.into_split();
            if let Err(e) = serve_connection(r, w, sessions).await {
                tracing::debug!(error=%e,"GTP connection ended");
            }
        });
    }
}
pub async fn serve_stdio(sessions: Sessions) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel(16);
    // tokio::io::stdin uses an uncancellable runtime blocking task, preventing
    // process exit after `quit` with an open stdin pipe. A detached OS reader
    // does not keep the runtime alive; it owns no sessions or search resources.
    std::thread::Builder::new()
        .name("gtp-stdin".into())
        .spawn(move || {
            use std::io::BufRead;
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            let mut line = Vec::new();
            let mut too_long = false;
            loop {
                let buffer = match reader.fill_buf() {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.blocking_send(Input::Error(e));
                        return;
                    }
                };
                if buffer.is_empty() {
                    if too_long {
                        let _ = tx.blocking_send(Input::TooLong);
                    } else if !line.is_empty() {
                        let _ = tx.blocking_send(Input::Line(line));
                    }
                    let _ = tx.blocking_send(Input::End);
                    return;
                }
                let newline = buffer.iter().position(|b| *b == b'\n');
                let consumed = newline.map_or(buffer.len(), |i| i + 1);
                if !too_long {
                    if line.len() + consumed > MAX_LINE {
                        line.clear();
                        too_long = true;
                    } else {
                        line.extend_from_slice(&buffer[..consumed]);
                    }
                }
                reader.consume(consumed);
                if newline.is_some() {
                    let item = if too_long {
                        Input::TooLong
                    } else {
                        Input::Line(std::mem::take(&mut line))
                    };
                    too_long = false;
                    if tx.blocking_send(item).is_err() {
                        return;
                    }
                }
            }
        })?;
    serve_incoming(rx, tokio::io::stdout(), sessions).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coordinates_and_parser() {
        assert_eq!(parse_vertex("A19"), Ok(Some(0)));
        assert_eq!(parse_vertex("t1"), Ok(Some(360)));
        assert_eq!(parse_vertex("D4"), Ok(Some(288)));
        assert_eq!(parse_vertex("pass"), Ok(None));
        for invalid in ["I9", "A0", "T20", "resign", "é3"] {
            assert!(parse_vertex(invalid).is_err());
        }
        let c = parse(b" \t001play\tb D4 # comment\r\n")
            .ok()
            .flatten()
            .unwrap();
        assert_eq!(c.id, "001");
        assert_eq!(c.name, "play");
        assert_eq!(c.args, ["b", "D4"]);
        assert!(parse(b"# comment").unwrap_or(None).is_none());
    }
    #[test]
    fn analysis_units_perspective_and_missing_data() {
        let c = Command {
            id: String::new(),
            name: "lz-analyze".into(),
            args: vec!["w".into(), "30".into()],
        };
        let a = analysis_args(&c, 2).unwrap();
        assert_eq!(a.interval, Duration::from_millis(300));
        let s = json!({"analysis":{"visits":9,"candidates":[{"index":0,"visits":8,"winRateBlack":0.7,"prior":0.12345,"scoreLeadBlack":2.0,"pv":[{"index":0},{"index":null}]}]}});
        let line = analysis_line(&s, &a).unwrap();
        assert!(line.contains("move A19 visits 8 winrate 3000 prior 1235"));
        assert!(line.ends_with("pv A19 pass\n"));
        let mut kata = a.clone();
        kata.lz = false;
        let line = analysis_line(&s, &kata).unwrap();
        assert!(line.contains("winrate 0.300000"));
        assert!(line.contains("scoreSelfplay -2.000000"));
        assert!(!line.contains("lcb") && !line.contains("scoreLead"));
        assert!(analysis_line(&json!({"analysis":{"candidates":[]}}), &a).is_none());
    }
}
