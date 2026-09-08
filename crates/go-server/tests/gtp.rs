use go_server::{
    gtp::serve_connection,
    session::{SessionConfig, Sessions},
    worker::WorkerPool,
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn sessions() -> Sessions {
    Sessions::new(
        SessionConfig::default(),
        WorkerPool::new(None, Duration::from_secs(5)),
    )
}
async fn frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut result = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            result.push_str(&line);
            if line == "\n" {
                return result;
            }
        }
    })
    .await
    .expect("GTP response did not finish")
}

#[tokio::test]
async fn standard_commands_mutate_one_authoritative_session() {
    let sessions = sessions();
    let (client, server) = tokio::io::duplex(65536);
    let (sr, sw) = tokio::io::split(server);
    let task = tokio::spawn(serve_connection(sr, sw, sessions.clone()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut reader = BufReader::new(cr);
    for (command, expected) in [
        ("1 protocol_version", "=1 2\n\n"),
        ("2 known_command genmove", "=2 true\n\n"),
        ("3 known_command loadsgf", "=3 false\n\n"),
        ("4 boardsize 19", "=4\n\n"),
        ("5 play b D4", "=5\n\n"),
        ("6 play w Q16", "=6\n\n"),
        ("7 undo", "=7\n\n"),
        ("8 play w pass", "=8\n\n"),
        ("9 clear_board", "=9\n\n"),
        ("10 komi 6.5", "=10\n\n"),
        ("11 get_komi", "=11 6.5\n\n"),
    ] {
        cw.write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(frame(&mut reader).await, expected);
    }
    for command in [
        "12 boardsize 9",
        "13 kata-set-rules japanese",
        "14 play w D4",
        "15 undo",
        "16 final_score",
        "17 play b I9",
    ] {
        cw.write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        let id = command.split_whitespace().next().unwrap();
        assert!(frame(&mut reader).await.starts_with(&format!("?{id} ")));
    }
    for (command, expected) in [
        ("18 play b pass", "=18\n\n"),
        ("19 play w pass", "=19\n\n"),
        ("20 final_score", "=20 W+6.5\n\n"),
        ("21 quit", "=21\n\n"),
    ] {
        cw.write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(frame(&mut reader).await, expected);
    }
    task.await.unwrap().unwrap();
    sessions.shutdown();
}

#[tokio::test]
async fn streaming_analysis_has_one_response_and_blank_input_terminates_it() {
    let sessions = sessions();
    let (client, server) = tokio::io::duplex(65536);
    let (sr, sw) = tokio::io::split(server);
    let task = tokio::spawn(serve_connection(sr, sw, sessions.clone()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut reader = BufReader::new(cr);
    cw.write_all(b"10 kata-analyze b 30 rootInfo true\n")
        .await
        .unwrap();
    let mut header = String::new();
    reader.read_line(&mut header).await.unwrap();
    assert_eq!(header, "=10\n");
    // No Worker means no fabricated info row; the existing response ends first.
    cw.write_all(b"11 stop\n").await.unwrap();
    assert_eq!(frame(&mut reader).await, "\n");
    assert_eq!(frame(&mut reader).await, "=11\n\n");
    cw.write_all(b"12 lz-analyze 30\n").await.unwrap();
    header.clear();
    reader.read_line(&mut header).await.unwrap();
    assert_eq!(header, "=12\n");
    cw.write_all(b"\n13 protocol_version\n").await.unwrap();
    assert_eq!(frame(&mut reader).await, "\n");
    assert_eq!(frame(&mut reader).await, "=13 2\n\n");
    cw.write_all(b"14 kata-analyze ownership true\n15 quit\n")
        .await
        .unwrap();
    assert!(
        frame(&mut reader)
            .await
            .starts_with("?14 unsupported analysis option")
    );
    assert_eq!(frame(&mut reader).await, "=15\n\n");
    task.await.unwrap().unwrap();
    sessions.shutdown();
}

#[tokio::test]
async fn overlong_input_is_drained_without_losing_next_command() {
    let sessions = sessions();
    let (client, server) = tokio::io::duplex(65536);
    let (sr, sw) = tokio::io::split(server);
    let task = tokio::spawn(serve_connection(sr, sw, sessions.clone()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut reader = BufReader::new(cr);
    cw.write_all(&vec![b'x'; 17000]).await.unwrap();
    cw.write_all(b"\n77 protocol_version\n78 quit\n")
        .await
        .unwrap();
    assert!(frame(&mut reader).await.starts_with("? command exceeds"));
    assert_eq!(frame(&mut reader).await, "=77 2\n\n");
    assert_eq!(frame(&mut reader).await, "=78\n\n");
    task.await.unwrap().unwrap();
    sessions.shutdown();
}

#[tokio::test]
async fn failed_genmove_and_pipelined_stop_do_not_block_input() {
    let sessions = sessions();
    let (client, server) = tokio::io::duplex(65536);
    let (sr, sw) = tokio::io::split(server);
    let task = tokio::spawn(serve_connection(sr, sw, sessions.clone()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut reader = BufReader::new(cr);
    cw.write_all(b"31 genmove b\n32 stop\n33 quit\n")
        .await
        .unwrap();
    let first = frame(&mut reader).await;
    assert!(first.starts_with("?31 NO_WORKERS"), "{first}");
    assert_eq!(frame(&mut reader).await, "=32\n\n");
    assert_eq!(frame(&mut reader).await, "=33\n\n");
    task.await.unwrap().unwrap();
    sessions.shutdown();
}
