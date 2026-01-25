use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::header,
    response::IntoResponse,
    routing::get,
    Router,
};
use clap::Parser;
    use futures_util::{sink::SinkExt, stream::StreamExt};
use rand::{rngs::StdRng, RngCore, SeedableRng};
use serde::Deserialize;
use tokio::{
    sync::Mutex,
    time::{interval, sleep},
};
use tracing::{error, info};

type WsSender = futures_util::stream::SplitSink<WebSocket, Message>;

#[derive(Parser, Debug, Clone)]
#[command(name = "lan-speedtest")]
struct Cli {
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: String,

    #[arg(long, default_value_t = 5000)]
    ping_duration_ms: u64,
    #[arg(long)]
    ping_max_count: Option<u64>,

    #[arg(long, default_value_t = 5000)]
    download_duration_ms: u64,
    #[arg(long, default_value_t = 40_000_000)]
    download_max_bytes: u64,

    #[arg(long, default_value_t = 5000)]
    upload_duration_ms: u64,
    #[arg(long, default_value_t = 40_000_000)]
    upload_max_bytes: u64,

    #[arg(long, default_value_t = 65_536)]
    chunk_bytes: usize,

    #[arg(long, default_value_t = 250)]
    stats_interval_ms: u64,
}

#[derive(Debug, Clone)]
struct AppState {
    cfg: Cli,
    chunk_buf: Arc<Vec<u8>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Ping,
    Download,
    Upload,
}

#[derive(Debug, Deserialize)]
struct ControlMsg {
    phase: Phase,
    #[serde(default)]
    action: Option<String>, // "start" for phases
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    t_send: Option<u64>,
    #[serde(default)]
    done: Option<bool>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    max_count: Option<u64>,
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    chunk_bytes: Option<usize>,
    #[serde(default)]
    bytes_sent: Option<u64>,
    #[serde(default)]
    reason: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .compact()
        .init();

    let cfg = Cli::parse();

    let mut rng = StdRng::from_entropy();
    let mut buf = vec![0u8; cfg.chunk_bytes];
    rng.fill_bytes(&mut buf);
    let chunk_buf = Arc::new(buf);

    let state = AppState {
        cfg: cfg.clone(),
        chunk_buf,
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    let addr: SocketAddr = cfg.bind.parse().expect("invalid bind address");
    info!("Listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate, max-age=0"),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "0"),
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        ],
        include_str!("../static/index.html"),
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate, max-age=0"),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "0"),
            (header::CONTENT_TYPE, "application/javascript"),
        ],
        include_str!("../static/app.js"),
    )
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(stream: WebSocket, state: AppState) {
    let (sender, mut receiver) = stream.split();
    let sender = Arc::new(Mutex::new(sender));

    // Shared flags/counters for download/upload
    let download_running = Arc::new(AtomicBool::new(false));
    let upload_running = Arc::new(AtomicBool::new(false));

    let upload_bytes = Arc::new(AtomicU64::new(0));
    let upload_start = Arc::new(AtomicU64::new(0));

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(txt) => {
                let parsed: serde_json::Result<ControlMsg> = serde_json::from_str(&txt);
                let Ok(ctrl) = parsed else {
                    error!("Bad control message: {txt}");
                    continue;
                };
                match ctrl.phase {
                    Phase::Ping => {
                        if ctrl.action.as_deref() == Some("start") {
                            // No-op: client drives ping timing & completion
                        } else {
                            handle_ping_echo(ctrl, sender.clone()).await;
                        }
                    }
                    Phase::Download => {
                        if ctrl.action.as_deref() == Some("start") {
                            if download_running.swap(true, Ordering::SeqCst) == false {
                                tokio::spawn(download_phase(
                                    sender.clone(),
                                    state.chunk_buf.clone(),
                                    state.cfg.clone(),
                                    ctrl,
                                    download_running.clone(),
                                ));
                            }
                        }
                    }
                    Phase::Upload => {
                        if ctrl.action.as_deref() == Some("start") {
                            upload_bytes.store(0, Ordering::SeqCst);
                            upload_start.store(now_ms(), Ordering::SeqCst);
                            upload_running.store(true, Ordering::SeqCst);
                        }
                        if ctrl.done.unwrap_or(false) {
                            let elapsed = now_ms() - upload_start.load(Ordering::SeqCst);
                            let bytes = ctrl.bytes_sent.unwrap_or(0);
                            let _ = send_control(
                                sender.clone(),
                                serde_json::json!({
                                    "phase": "upload",
                                    "done": true,
                                    "bytes": bytes,
                                    "elapsed_ms": elapsed,
                                    "reason": "client_done"
                                }),
                            )
                            .await;
                            upload_running.store(false, Ordering::SeqCst);
                        }
                    }
                }
            }
            Message::Binary(bin) => {
                if upload_running.load(Ordering::SeqCst) {
                    upload_bytes.fetch_add(bin.len() as u64, Ordering::SeqCst);
                    let elapsed = now_ms() - upload_start.load(Ordering::SeqCst);
                    let bytes = upload_bytes.load(Ordering::SeqCst);
                    // Early stop if limits reached
                    if bytes >= state.cfg.upload_max_bytes || elapsed >= state.cfg.upload_duration_ms {
                        let reason = if bytes >= state.cfg.upload_max_bytes {
                            "byte-cap"
                        } else {
                            "duration"
                        };
                        let _ = send_control(
                            sender.clone(),
                            serde_json::json!({
                                "phase": "upload",
                                "done": true,
                                "bytes": bytes,
                                "elapsed_ms": elapsed,
                                "reason": reason,
                                "early_finish": true
                            }),
                        )
                        .await;
                        upload_running.store(false, Ordering::SeqCst);
                    } else {
                        // Periodic stats could be added via timer; keep binary path lightweight
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

/// Responds to each ping echo message.
async fn handle_ping_echo(ctrl: ControlMsg, sender: Arc<Mutex<WsSender>>) {
    if let Some(t_send) = ctrl.t_send {
        let _ = send_control(
            sender,
            serde_json::json!({
                "phase": "ping",
                "seq": ctrl.seq,
                "t_send": t_send,
                // t_recv omitted; client measures RTT locally to avoid clock skew
                "done": ctrl.done.unwrap_or(false),
            }),
        )
        .await;
    }
}

/// Download phase: server → client binary chunks.
async fn download_phase(
    sender: Arc<Mutex<WsSender>>,
    chunk_buf: Arc<Vec<u8>>,
    cfg: Cli,
    ctrl: ControlMsg,
    running: Arc<AtomicBool>,
) {
    let duration_ms = ctrl.duration_ms.unwrap_or(cfg.download_duration_ms);
    let max_bytes = ctrl.max_bytes.unwrap_or(cfg.download_max_bytes);
    let chunk_bytes = ctrl.chunk_bytes.unwrap_or(cfg.chunk_bytes);

    let start = std::time::Instant::now();
    let mut sent: u64 = 0;
    let mut ticker = interval(Duration::from_millis(cfg.stats_interval_ms));

    let mut reason = "duration";
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let _ = send_control(sender.clone(), serde_json::json!({
                    "phase": "download",
                    "bytes": sent,
                    "elapsed_ms": start.elapsed().as_millis() as u64,
                    "done": false
                })).await;
            }
            _ = sleep(Duration::from_millis(0)) => {}
        }

        if start.elapsed().as_millis() as u64 >= duration_ms {
            reason = "duration";
            break;
        }
        if sent >= max_bytes {
            reason = "byte-cap";
            break;
        }

        let chunk = &chunk_buf[..chunk_bytes.min(chunk_buf.len())];
        if let Err(err) = sender.lock().await.send(Message::Binary(chunk.to_vec())).await {
            error!("Failed to send chunk: {err}");
            break;
        }
        sent += chunk.len() as u64;
    }

    let _ = send_control(sender.clone(), serde_json::json!({
        "phase": "download",
        "bytes": sent,
        "elapsed_ms": start.elapsed().as_millis() as u64,
        "done": true,
        "reason": reason
    }))
    .await;

    running.store(false, Ordering::SeqCst);
}

async fn send_control(
    sender: Arc<Mutex<WsSender>>,
    payload: serde_json::Value,
) -> Result<(), axum::Error> {
    let txt = serde_json::to_string(&payload).expect("serialize control");
    sender.lock().await.send(Message::Text(txt)).await
}

fn now_ms() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    now
}