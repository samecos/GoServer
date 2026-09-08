use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

#[tokio::test]
async fn gtp_quit_exits_even_when_the_parent_keeps_stdin_open() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_go-server"))
        .args(["--http", "127.0.0.1:0", "--grpc", "127.0.0.1:0", "--gtp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"1 protocol_version\n2 quit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("GTP quit must not wait for stdin EOF")
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        "=1 2\n\n=2\n\n"
    );
    drop(stdin);
}

#[tokio::test]
async fn an_unavailable_gtp_port_fails_startup_before_waiting_for_stdio() {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_go-server"))
        .args([
            "--http",
            "127.0.0.1:0",
            "--grpc",
            "127.0.0.1:0",
            "--gtp",
            "--gtp-tcp",
        ])
        .arg(occupied.local_addr().unwrap().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("a failed listener must fail the entire startup")
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    drop(stdin);
}
