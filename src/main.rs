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
        connect_info::ConnectInfo,
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::header,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use byte_unit::Byte;
use clap::Parser;
use futures_util::{sink::SinkExt, stream::StreamExt};
use rand::{rngs::StdRng, RngCore, SeedableRng};
use serde::Deserialize;
use tokio::{
    sync::Mutex,
    time::Instant,
};
use tracing::{debug, error, info};

type WsSender = futures_util::stream::SplitSink<WebSocket, Message>;
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

// Maximum limits to prevent resource exhaustion
const MAX_DURATION_MS: u64 = 300_000;       // 5 minutes
const MAX_BYTES: u64 = 10_737_418_240;      // 10 GB
const MAX_CHUNK_BYTES: usize = 1_048_576;   // 1 MB
const MAX_PING_COUNT: u64 = 5_000;          // 5,000 pings max
const MIN_CHUNK_BYTES: usize = 64;          // 64 bytes minimum

#[derive(Parser, Debug, Clone)]
#[command(name = "lan-speedtest")]
#[command(author, version, about)]
struct Cli {
    /// Address to bind to (e.g. 0.0.0.0:8080)
    #[arg(short = 'b', long, default_value = "0.0.0.0:8080")]
    bind: String,

    /// Duration of ping test in ms
    #[arg(short = 'p', long, default_value_t = 5000)]
    ping_duration_ms: u64,

    /// Max number of pings to send
    #[arg(short = 'c', long, default_value_t = 100)]
    ping_max_count: u64,

    /// Max bytes to download (e.g. 40MB)
    #[arg(short = 'D', long, default_value = "40000000", value_parser = parse_bytes)]
    download_max_bytes: u64,

    /// Duration of download test in ms
    #[arg(short = 'd', long, default_value_t = 5000)]
    download_duration_ms: u64,

    /// Max bytes to upload (e.g. 40MB)
    #[arg(short = 'U', long, default_value = "40000000", value_parser = parse_bytes)]
    upload_max_bytes: u64,

    /// Duration of upload test in ms
    #[arg(short = 'u', long, default_value_t = 5000)]
    upload_duration_ms: u64,

    /// Size of individual data chunks
    #[arg(short = 'k', long, default_value = "65536", value_parser = parse_chunk_bytes)]
    chunk_bytes: usize,

    /// Interval between stats updates in ms
    #[arg(short = 's', long, default_value_t = 250)]
    stats_interval_ms: u64,
}

fn parse_bytes(s: &str) -> Result<u64, String> {
    Byte::from_str(s)
        .map_err(|e| e.to_string())
        .and_then(|b| u64::try_from(b.get_bytes()).map_err(|_| "value exceeds u64".to_string()))
}

fn parse_chunk_bytes(s: &str) -> Result<usize, String> {
    let v = parse_bytes(s)?;
    usize::try_from(v).map_err(|_| "chunk_bytes too large for usize".to_string())
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

#[derive(Debug)]
struct ValidationError {
    field: String,
    value: String,
    max: String,
    message: String,
}

impl ValidationError {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self.message,
            "field": self.field,
            "value": self.value,
            "max": self.max
        })
    }
}

#[derive(Debug, Deserialize)]
struct ControlMsg {
    phase: Phase,
    #[serde(default)]
    action: Option<String>, // "start" for phases
    #[serde(default)]
    test_id: Option<u64>,
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
    #[serde(default)]
    ping_count: Option<u64>,
    #[serde(default)]
    ping_avg_ms: Option<f64>,
    #[serde(default)]
    ping_min_ms: Option<f64>,
    #[serde(default)]
    ping_max_ms: Option<f64>,
}

/// Tracks results for a single test run
#[derive(Debug, Default)]
struct TestResults {
    test_id: u64,
    ping_count: u64,
    ping_avg_ms: f64,
    ping_min_ms: f64,
    ping_max_ms: f64,
    download_bytes: u64,
    download_ms: u64,
    download_mbps: f64,
    upload_bytes: u64,
    upload_ms: u64,
    upload_mbps: f64,
    logged_complete: bool,
}

fn validate_control_msg(ctrl: &ControlMsg, cfg: &Cli) -> Option<ValidationError> {
    match ctrl.phase {
        Phase::Ping => {
            if let Some(duration) = ctrl.duration_ms {
                if duration > MAX_DURATION_MS {
                    return Some(ValidationError {
                        field: "ping_duration_ms".to_string(),
                        value: duration.to_string(),
                        max: MAX_DURATION_MS.to_string(),
                        message: format!("Ping duration exceeds maximum of {}ms", MAX_DURATION_MS),
                    });
                }
            }
            if let Some(count) = ctrl.max_count {
                if count > MAX_PING_COUNT {
                    return Some(ValidationError {
                        field: "ping_max_count".to_string(),
                        value: count.to_string(),
                        max: MAX_PING_COUNT.to_string(),
                        message: format!("Ping count exceeds maximum of {}", MAX_PING_COUNT),
                    });
                }
                // Also check against CLI default if not overridden
                if count > cfg.ping_max_count && count != cfg.ping_max_count {
                    // Allow override but log warning (will be logged at call site)
                    debug!(cli_default=cfg.ping_max_count, requested=count, "Ping count exceeds CLI default");
                }
            }
        }
        Phase::Download => {
            if let Some(duration) = ctrl.duration_ms {
                if duration > MAX_DURATION_MS {
                    return Some(ValidationError {
                        field: "download_duration_ms".to_string(),
                        value: duration.to_string(),
                        max: MAX_DURATION_MS.to_string(),
                        message: format!("Download duration exceeds maximum of {}ms", MAX_DURATION_MS),
                    });
                }
            }
            if let Some(bytes) = ctrl.max_bytes {
                if bytes > MAX_BYTES {
                    return Some(ValidationError {
                        field: "download_max_bytes".to_string(),
                        value: bytes.to_string(),
                        max: MAX_BYTES.to_string(),
                        message: format!("Download bytes exceed maximum of {}", MAX_BYTES),
                    });
                }
            }
            if let Some(chunk) = ctrl.chunk_bytes {
                if chunk < MIN_CHUNK_BYTES || chunk > MAX_CHUNK_BYTES {
                    return Some(ValidationError {
                        field: "chunk_bytes".to_string(),
                        value: chunk.to_string(),
                        max: MAX_CHUNK_BYTES.to_string(),
                        message: format!("Chunk size must be between {} and {} bytes", MIN_CHUNK_BYTES, MAX_CHUNK_BYTES),
                    });
                }
            }
        }
        Phase::Upload => {
            if let Some(duration) = ctrl.duration_ms {
                if duration > MAX_DURATION_MS {
                    return Some(ValidationError {
                        field: "upload_duration_ms".to_string(),
                        value: duration.to_string(),
                        max: MAX_DURATION_MS.to_string(),
                        message: format!("Upload duration exceeds maximum of {}ms", MAX_DURATION_MS),
                    });
                }
            }
            if let Some(bytes) = ctrl.max_bytes {
                if bytes > MAX_BYTES {
                    return Some(ValidationError {
                        field: "upload_max_bytes".to_string(),
                        value: bytes.to_string(),
                        max: MAX_BYTES.to_string(),
                        message: format!("Upload bytes exceed maximum of {}", MAX_BYTES),
                    });
                }
            }
            if let Some(chunk) = ctrl.chunk_bytes {
                if chunk < MIN_CHUNK_BYTES || chunk > MAX_CHUNK_BYTES {
                    return Some(ValidationError {
                        field: "chunk_bytes".to_string(),
                        value: chunk.to_string(),
                        max: MAX_CHUNK_BYTES.to_string(),
                        message: format!("Chunk size must be between {} and {} bytes", MIN_CHUNK_BYTES, MAX_CHUNK_BYTES),
                    });
                }
            }
        }
    }
    None
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
        .route("/config", get(config))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
        .into_make_service_with_connect_info::<SocketAddr>();

    let addr: SocketAddr = cfg.bind.parse().expect("invalid bind address");
    info!("Listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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

async fn config(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = &state.cfg;
    (
        [
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate, max-age=0"),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "0"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        Json(serde_json::json!({
            "ping_duration_ms": cfg.ping_duration_ms,
            "ping_max_count": cfg.ping_max_count,
            "download_duration_ms": cfg.download_duration_ms,
            "download_max_bytes": cfg.download_max_bytes,
            "upload_duration_ms": cfg.upload_duration_ms,
            "upload_max_bytes": cfg.upload_max_bytes,
            "chunk_bytes": cfg.chunk_bytes,
            "max_ping_duration_ms": MAX_DURATION_MS,
            "max_ping_count": MAX_PING_COUNT,
            "max_download_duration_ms": MAX_DURATION_MS,
            "max_download_bytes": MAX_BYTES,
            "max_upload_duration_ms": MAX_DURATION_MS,
            "max_upload_bytes": MAX_BYTES,
            "max_chunk_bytes": MAX_CHUNK_BYTES,
            "min_chunk_bytes": MIN_CHUNK_BYTES,
        })),
    )
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
    ws.on_upgrade(move |socket| handle_socket(socket, state, conn_id, addr))
}

async fn handle_socket(stream: WebSocket, state: AppState, conn_id: u64, addr: SocketAddr) {
    info!(%conn_id, %addr, "ws connected");

    let (sender, mut receiver) = stream.split();
    let sender = Arc::new(Mutex::new(sender));

    // Shared flags/counters for download/upload
    let download_running = Arc::new(AtomicBool::new(false));
    let upload_running = Arc::new(AtomicBool::new(false));

    let upload_bytes = Arc::new(AtomicU64::new(0));
    // Store start time as nanos since an arbitrary epoch (Instant::now() at connection start)
    let connection_epoch = Instant::now();
    let upload_start_nanos = Arc::new(AtomicU64::new(0)); // 0 means not started
    
    // Store client's requested upload limits (set when upload starts)
    let upload_max_bytes = Arc::new(AtomicU64::new(state.cfg.upload_max_bytes));
    let upload_duration_ms = Arc::new(AtomicU64::new(state.cfg.upload_duration_ms));

    // Track current test results
    let current_test: Arc<Mutex<TestResults>> = Arc::new(Mutex::new(TestResults::default()));
    
    // Track ping count for this connection
    let ping_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let ping_max_count_for_test: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(txt) => {
                let parsed: serde_json::Result<ControlMsg> = serde_json::from_str(&txt);
                let Ok(ctrl) = parsed else {
                    error!(%conn_id, %addr, raw=?txt, "Bad control message");
                    continue;
                };
                match ctrl.phase {
                    Phase::Ping => {
                        if ctrl.action.as_deref() == Some("start") {
                            // Validate parameters before starting
                            if let Some(err) = validate_control_msg(&ctrl, &state.cfg) {
                                error!(%conn_id, %addr, field=%err.field, value=%err.value, max=%err.max, "Validation failed");
                                let _ = send_control(
                                    sender.clone(),
                                    err.to_json(),
                                ).await;
                                continue;
                            }
                            
                            // Reset ping count and set max for this test
                            ping_count.store(0, Ordering::Relaxed);
                            let max_count = ctrl.max_count.unwrap_or(state.cfg.ping_max_count).min(MAX_PING_COUNT);
                            ping_max_count_for_test.store(max_count, Ordering::Relaxed);
                            
                            let test_id = ctrl.test_id.unwrap_or(0);
                            {
                                let mut test = current_test.lock().await;
                                *test = TestResults::default();
                                test.test_id = test_id;
                            }
                            info!(%conn_id, %addr, test_id, max_count, "ping start");
                        } else if ctrl.done.unwrap_or(false) {
                            // Client sends final ping stats
                            let test_id = ctrl.test_id.unwrap_or(0);
                            let ping_count = ctrl.ping_count.unwrap_or(0);
                            let ping_avg = ctrl.ping_avg_ms.unwrap_or(0.0);
                            let ping_min = ctrl.ping_min_ms.unwrap_or(0.0);
                            let ping_max = ctrl.ping_max_ms.unwrap_or(0.0);
                            {
                                let mut test = current_test.lock().await;
                                test.ping_count = ping_count;
                                test.ping_avg_ms = ping_avg;
                                test.ping_min_ms = ping_min;
                                test.ping_max_ms = ping_max;
                            }
                            info!(%conn_id, %addr, test_id, ping_count, ping_avg_ms=format!("{:.2}", ping_avg), ping_min_ms=format!("{:.2}", ping_min), ping_max_ms=format!("{:.2}", ping_max), "ping done");
                        } else {
                            handle_ping_echo(ctrl, sender.clone(), conn_id, addr, ping_count.clone(), ping_max_count_for_test.clone()).await;
                        }
                    }
                    Phase::Download => {
                        if ctrl.action.as_deref() == Some("start") {
                            // Validate parameters before starting
                            if let Some(err) = validate_control_msg(&ctrl, &state.cfg) {
                                error!(%conn_id, %addr, field=%err.field, value=%err.value, max=%err.max, "Validation failed");
                                let _ = send_control(
                                    sender.clone(),
                                    err.to_json(),
                                ).await;
                                continue;
                            }
                            
                            if download_running.swap(true, Ordering::SeqCst) == false {
                                let cfg_clone = state.cfg.clone();
                                let chunk_buf = state.chunk_buf.clone();
                                let test_id = ctrl.test_id.unwrap_or(0);
                                info!(
                                    %conn_id,
                                    %addr,
                                    test_id,
                                    duration_ms=?ctrl.duration_ms.unwrap_or(cfg_clone.download_duration_ms),
                                    max_bytes=?ctrl.max_bytes.unwrap_or(cfg_clone.download_max_bytes),
                                    chunk_bytes=?ctrl.chunk_bytes.unwrap_or(cfg_clone.chunk_bytes),
                                    "download start"
                                );
                                tokio::spawn(download_phase(
                                    sender.clone(),
                                    chunk_buf,
                                    cfg_clone,
                                    ctrl,
                                    download_running.clone(),
                                    current_test.clone(),
                                    conn_id,
                                    addr,
                                ));
                            }
                        }
                    }
                    Phase::Upload => {
                        let test_id = ctrl.test_id.unwrap_or(0);
                        if ctrl.action.as_deref() == Some("start") {
                            // Validate parameters before starting
                            if let Some(err) = validate_control_msg(&ctrl, &state.cfg) {
                                error!(%conn_id, %addr, field=%err.field, value=%err.value, max=%err.max, "Validation failed");
                                let _ = send_control(
                                    sender.clone(),
                                    err.to_json(),
                                ).await;
                                continue;
                            }
                            
                            upload_bytes.store(0, Ordering::Relaxed);
                            let nanos = connection_epoch.elapsed().as_nanos() as u64;
                            upload_start_nanos.store(nanos, Ordering::Relaxed);
                            // Store client's requested limits
                            let req_max_bytes = ctrl.max_bytes.unwrap_or(state.cfg.upload_max_bytes);
                            let req_duration_ms = ctrl.duration_ms.unwrap_or(state.cfg.upload_duration_ms);
                            upload_max_bytes.store(req_max_bytes, Ordering::Relaxed);
                            upload_duration_ms.store(req_duration_ms, Ordering::Relaxed);
                            upload_running.store(true, Ordering::Relaxed);
                            info!(
                                %conn_id,
                                %addr,
                                test_id,
                                duration_ms=req_duration_ms,
                                max_bytes=req_max_bytes,
                                chunk_bytes=?ctrl.chunk_bytes.unwrap_or(state.cfg.chunk_bytes),
                                "upload start"
                            );
                        }
                        if ctrl.done.unwrap_or(false) {
                            let start_nanos = upload_start_nanos.load(Ordering::Relaxed);
                            let now_nanos = connection_epoch.elapsed().as_nanos() as u64;
                            let elapsed = (now_nanos.saturating_sub(start_nanos)) / 1_000_000;
                            let bytes = ctrl.bytes_sent.unwrap_or(0);
                            let rate_mbps = if elapsed > 0 { (bytes as f64 * 8.0) / (elapsed as f64 * 1000.0) } else { 0.0 };
                            
                            // Update test results and log summary (only once)
                            {
                                let mut test = current_test.lock().await;
                                if !test.logged_complete {
                                    test.upload_bytes = bytes;
                                    test.upload_ms = elapsed;
                                    test.upload_mbps = rate_mbps;
                                    test.logged_complete = true;
                                    
                                    info!(
                                        %conn_id,
                                        %addr,
                                        test_id = test.test_id,
                                        ping_count = test.ping_count,
                                        ping_avg_ms = format!("{:.2}", test.ping_avg_ms),
                                        ping_min_ms = format!("{:.2}", test.ping_min_ms),
                                        ping_max_ms = format!("{:.2}", test.ping_max_ms),
                                        download_mbps = format!("{:.2}", test.download_mbps),
                                        download_bytes = test.download_bytes,
                                        download_ms = test.download_ms,
                                        upload_mbps = format!("{:.2}", test.upload_mbps),
                                        upload_bytes = test.upload_bytes,
                                        upload_ms = test.upload_ms,
                                        "TEST COMPLETE"
                                    );
                                }
                            }
                            
                            info!(%conn_id, %addr, test_id, bytes, elapsed_ms=elapsed, rate_mbps=format!("{:.2}", rate_mbps), reason="client_done", "upload done");
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
                            upload_running.store(false, Ordering::Relaxed);
                        }
                    }
                }
            }
            Message::Binary(bin) => {
                if upload_running.load(Ordering::Relaxed) {
                    let bytes = upload_bytes.fetch_add(bin.len() as u64, Ordering::Relaxed) + bin.len() as u64;
                    let max_bytes = upload_max_bytes.load(Ordering::Relaxed);
                    let duration_ms = upload_duration_ms.load(Ordering::Relaxed);
                    
                    // Only check time periodically (every ~1MB) to reduce overhead
                    let check_time = bytes % 1_048_576 < bin.len() as u64 || bytes >= max_bytes;
                    
                    let elapsed = if check_time {
                        let start_nanos = upload_start_nanos.load(Ordering::Relaxed);
                        let now_nanos = connection_epoch.elapsed().as_nanos() as u64;
                        (now_nanos.saturating_sub(start_nanos)) / 1_000_000
                    } else {
                        0 // Skip time check
                    };
                    
                    // Early stop if limits reached
                    if bytes >= max_bytes || (check_time && elapsed >= duration_ms) {
                        let reason = if bytes >= max_bytes {
                            "byte-cap"
                        } else {
                            "duration"
                        };
                        let rate_mbps = if elapsed > 0 { (bytes as f64 * 8.0) / (elapsed as f64 * 1000.0) } else { 0.0 };
                        
                        // Update test results and log summary (only once)
                        {
                            let mut test = current_test.lock().await;
                            if !test.logged_complete {
                                test.upload_bytes = bytes;
                                test.upload_ms = elapsed;
                                test.upload_mbps = rate_mbps;
                                test.logged_complete = true;
                                
                                info!(
                                    %conn_id,
                                    %addr,
                                    test_id = test.test_id,
                                    ping_count = test.ping_count,
                                    ping_avg_ms = format!("{:.2}", test.ping_avg_ms),
                                    ping_min_ms = format!("{:.2}", test.ping_min_ms),
                                    ping_max_ms = format!("{:.2}", test.ping_max_ms),
                                    download_mbps = format!("{:.2}", test.download_mbps),
                                    download_bytes = test.download_bytes,
                                    download_ms = test.download_ms,
                                    upload_mbps = format!("{:.2}", test.upload_mbps),
                                    upload_bytes = test.upload_bytes,
                                    upload_ms = test.upload_ms,
                                    "TEST COMPLETE"
                                );
                            }
                        }
                        
                        info!(%conn_id, %addr, bytes, elapsed_ms=elapsed, rate_mbps=format!("{:.2}", rate_mbps), reason, "upload done (server early_finish)");
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
                        upload_running.store(false, Ordering::Relaxed);
                    }
                }
            }
            Message::Close(frame) => {
                info!(%conn_id, %addr, ?frame, "ws close frame");
                
                let mut guard = sender.lock().await;

                // 1. Attempt to send (this might fail if library already queued a reply)
                if let Err(e) = guard.send(Message::Close(frame)).await {
                    tracing::warn!(%conn_id, %addr, error = %e, "failed to send close frame");
                }

                // 2. CRITICAL FIX: Force a flush.
                // If 'send' failed above, it skipped flushing. We must manually flush
                // to push the library's auto-generated Close frame out to the OS.
                if let Err(e) = guard.flush().await {
                    tracing::warn!(%conn_id, %addr, error = %e, "failed to flush close frame");
                }

                // Drop the lock explicitly (optional, but good practice before sleep)
                drop(guard);

                // 3. Wait for OS to transmit
                tokio::time::sleep(Duration::from_millis(100)).await;

                break;
            }
            _ => {}
        }
    }

    info!(%conn_id, %addr, "ws disconnected");
}

/// Responds to each ping echo message.
async fn handle_ping_echo(
    ctrl: ControlMsg,
    sender: Arc<Mutex<WsSender>>,
    conn_id: u64,
    addr: SocketAddr,
    ping_count: Arc<AtomicU64>,
    ping_max_count: Arc<AtomicU64>,
) {
    if let Some(t_send) = ctrl.t_send {
        // Check if we've exceeded the ping count limit
        let current_count = ping_count.fetch_add(1, Ordering::Relaxed) + 1;
        let max_count = ping_max_count.load(Ordering::Relaxed);
        
        if current_count > max_count {
            error!(%conn_id, %addr, current_count, max_count, "Ping count exceeded");
            let _ = send_control(
                sender,
                serde_json::json!({
                    "phase": "ping",
                    "error": format!("Ping count exceeded maximum of {}", max_count),
                    "done": true,
                }),
            ).await;
            return;
        }
        
        debug!(%conn_id, %addr, seq=?ctrl.seq, count=current_count, max=max_count, "ping echo");
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
/// Optimized: batches multiple chunk sends before yielding, avoids per-chunk allocations.
async fn download_phase(
    sender: Arc<Mutex<WsSender>>,
    chunk_buf: Arc<Vec<u8>>,
    cfg: Cli,
    ctrl: ControlMsg,
    running: Arc<AtomicBool>,
    current_test: Arc<Mutex<TestResults>>,
    conn_id: u64,
    addr: SocketAddr,
) {
    let test_id = ctrl.test_id.unwrap_or(0);
    let duration_ms = ctrl.duration_ms.unwrap_or(cfg.download_duration_ms);
    let max_bytes = ctrl.max_bytes.unwrap_or(cfg.download_max_bytes);
    let chunk_bytes = ctrl.chunk_bytes.unwrap_or(cfg.chunk_bytes);

    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut last_stats = Instant::now();
    let stats_interval = Duration::from_millis(cfg.stats_interval_ms);

    // Pre-slice the chunk to avoid repeated slicing
    let chunk: Bytes = Bytes::copy_from_slice(&chunk_buf[..chunk_bytes.min(chunk_buf.len())]);
    
    // Batch size: send multiple chunks before yielding to reduce async overhead
    const BATCH_SIZE: u32 = 16;

    let mut reason = "duration";
    'outer: loop {
        // Send stats periodically
        if last_stats.elapsed() >= stats_interval {
            let _ = send_control(sender.clone(), serde_json::json!({
                "phase": "download",
                "bytes": sent,
                "elapsed_ms": start.elapsed().as_millis() as u64,
                "done": false
            })).await;
            last_stats = Instant::now();
        }

        // Batch send chunks
        {
            let mut guard = sender.lock().await;
            for _ in 0..BATCH_SIZE {
                // Check termination conditions
                if start.elapsed().as_millis() as u64 >= duration_ms {
                    reason = "duration";
                    break 'outer;
                }
                if sent >= max_bytes {
                    reason = "byte-cap";
                    break 'outer;
                }

                // Send chunk using Bytes (zero-copy clone)
                if let Err(err) = guard.send(Message::Binary(chunk.to_vec())).await {
                    error!(%conn_id, %addr, %err, "Failed to send chunk");
                    break 'outer;
                }
                sent += chunk.len() as u64;
            }
        }

        // Yield to allow other tasks to run
        tokio::task::yield_now().await;
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let rate_mbps = if elapsed_ms > 0 { (sent as f64 * 8.0) / (elapsed_ms as f64 * 1000.0) } else { 0.0 };
    
    // Update test results
    {
        let mut test = current_test.lock().await;
        test.download_bytes = sent;
        test.download_ms = elapsed_ms;
        test.download_mbps = rate_mbps;
    }
    
    info!(%conn_id, %addr, test_id, bytes=sent, elapsed_ms, rate_mbps=format!("{:.2}", rate_mbps), reason, "download done");
    let _ = send_control(sender.clone(), serde_json::json!({
        "phase": "download",
        "bytes": sent,
        "elapsed_ms": elapsed_ms,
        "done": true,
        "reason": reason
    }))
    .await;

    running.store(false, Ordering::Relaxed);
}

async fn send_control(
    sender: Arc<Mutex<WsSender>>,
    payload: serde_json::Value,
) -> Result<(), axum::Error> {
    let txt = serde_json::to_string(&payload).expect("serialize control");
    sender.lock().await.send(Message::Text(txt)).await
}

