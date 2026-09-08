use clap::Parser;
use go_protocol::v1::worker_service_server::WorkerServiceServer;
use go_server::{
    app::App,
    session::{SessionConfig, Sessions},
    worker::WorkerPool,
};
use std::{net::SocketAddr, time::Duration};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "CPU MCGS Go service with remote KataGo inference workers"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8090")]
    http: SocketAddr,
    #[arg(long, default_value = "0.0.0.0:50051")]
    grpc: SocketAddr,
    #[arg(long)]
    model_sha256: Option<String>,
    #[arg(long, default_value_t = 512)]
    graph_memory_mib: usize,
    #[arg(long, default_value_t = 100000)]
    max_nodes: usize,
    #[arg(long, default_value_t = 128)]
    max_in_flight: usize,
    #[arg(long, default_value_t = 4)]
    max_sessions: usize,
    #[arg(long, default_value_t = 120)]
    session_retention_secs: u64,
    #[arg(long, default_value_t = 300)]
    publish_ms: u64,
    #[arg(long, default_value_t = 20000)]
    lease_ms: u64,
    #[arg(long)]
    gtp: bool,
    #[arg(long)]
    gtp_tcp: Option<SocketAddr>,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "go_server=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    if args.publish_ms == 0
        || args.max_sessions == 0
        || args.lease_ms == 0
        || args.session_retention_secs == 0
    {
        return Err("capacities and intervals must be positive".into());
    }
    if args
        .model_sha256
        .as_ref()
        .is_some_and(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("model-sha256 must be 64 hex digits".into());
    }
    let mut config = SessionConfig::default();
    config.search.max_memory_bytes = args
        .graph_memory_mib
        .checked_mul(1024 * 1024)
        .ok_or("graph memory limit overflow")?;
    config.search.max_nodes = args.max_nodes;
    config.search.max_in_flight = args.max_in_flight;
    config.max_sessions = args.max_sessions;
    config.retention = Duration::from_secs(args.session_retention_secs);
    config.publish = Duration::from_millis(args.publish_ms);
    go_core::Search::new(go_core::Position::new(19, 7.5)?, config.search.clone())?;
    let pool = WorkerPool::new(
        args.model_sha256.map(|s| s.to_ascii_lowercase()),
        Duration::from_millis(args.lease_ms),
    );
    let sessions = Sessions::new(config, pool.clone());
    let app = App::new(sessions.clone(), pool.clone());
    let listener = tokio::net::TcpListener::bind(args.http).await?;
    let grpc_socket = tokio::net::TcpListener::bind(args.grpc).await?;
    let gtp_socket = match args.gtp_tcp {
        Some(address) => Some(tokio::net::TcpListener::bind(address).await?),
        None => None,
    };
    tracing::info!(http=%args.http,grpc=%args.grpc,"service listening; awaiting compatible inference workers");
    let mut http = tokio::spawn(async move { axum::serve(listener, app.router()).await });
    let grpc_service = WorkerServiceServer::new(pool.clone())
        .max_decoding_message_size(4 * 1024 * 1024)
        .max_encoding_message_size(4 * 1024 * 1024);
    let mut grpc = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(grpc_service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(grpc_socket))
            .await
    });
    let maintenance = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            pool.maintenance();
        }
    });
    let mut gtp_listener = if let Some(listener) = gtp_socket {
        let sessions = sessions.clone();
        Some(tokio::spawn(async move {
            go_server::gtp::serve_listener(listener, sessions).await
        }))
    } else {
        None
    };
    let outcome=async {
        tokio::select! {
            r=&mut http=>{r??;},
            r=&mut grpc=>{r??;},
            r=async {if args.gtp {go_server::gtp::serve_stdio(sessions.clone()).await}else{std::future::pending().await}}=>{r?;},
            r=async {if let Some(task)=&mut gtp_listener {task.await?.map_err(|e|e as Box<dyn std::error::Error>)}else{std::future::pending().await}}=>{r?;},
            r=tokio::signal::ctrl_c()=>{r?;}
        }
        Ok::<(),Box<dyn std::error::Error>>(())
    }.await;
    sessions.shutdown();
    maintenance.abort();
    http.abort();
    grpc.abort();
    if let Some(gtp) = gtp_listener {
        gtp.abort();
    }
    outcome
}
