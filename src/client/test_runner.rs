use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use url::Url;

#[derive(Parser, Debug, Clone)]
#[command(name = "lan-speedtest-client")]
pub struct ClientOpts {
    /// Server WebSocket URL
    #[arg(short, long, default_value = "ws://localhost:8080/ws")]
    pub server: String,

    /// Ping test duration in ms
    #[arg(long = "ping-duration-ms", default_value_t = 5000)]
    pub ping_duration_ms: u64,

    /// Max number of pings
    #[arg(long = "ping-max-count", default_value_t = 100)]
    pub ping_max_count: u64,

    /// Max bytes to download (e.g. 40MB)
    #[arg(long = "download-max-bytes", default_value = "40000000", value_parser = parse_bytes)]
    pub download_max_bytes: u64,

    /// Duration of download test in ms
    #[arg(long = "download-duration-ms", default_value_t = 5000)]
    pub download_duration_ms: u64,

    /// Max bytes to upload (e.g. 40MB)
    #[arg(long = "upload-max-bytes", default_value = "40000000", value_parser = parse_bytes)]
    pub upload_max_bytes: u64,

    /// Duration of upload test in ms
    #[arg(long = "upload-duration-ms", default_value_t = 5000)]
    pub upload_duration_ms: u64,

    /// Size of individual data chunks
    #[arg(long = "chunk-bytes", default_value = "65536", value_parser = parse_chunk_bytes)]
    pub chunk_bytes: usize,

    /// Output format (text, json)
    #[arg(long, default_value = "text")]
    pub format: String,
}

fn parse_bytes(s: &str) -> Result<u64, String> {
    byte_unit::Byte::from_str(s)
        .map_err(|e| e.to_string())
        .and_then(|b| u64::try_from(b.get_bytes()).map_err(|_| "value exceeds u64".to_string()))
}

fn parse_chunk_bytes(s: &str) -> Result<usize, String> {
    let v = parse_bytes(s)?;
    usize::try_from(v).map_err(|_| "chunk_bytes too large for usize".to_string())
}

#[derive(Debug, Default)]
struct TestResults {
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
}

pub async fn run_client(opts: ClientOpts) {
    let url = Url::parse(&opts.server).expect("Invalid server URL");
    info!("Connecting to {}", url);

    let (ws_stream, _) = connect_async(url.as_str()).await.expect("Failed to connect");
    info!("Connected");

    let (mut write, mut read) = ws_stream.split();
    let test_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let results = Arc::new(Mutex::new(TestResults::default()));

    // Ping phase
    info!("Starting ping phase...");
    let start_msg = serde_json::json!({
        "phase": "ping",
        "action": "start",
        "test_id": test_id,
        "duration_ms": opts.ping_duration_ms,
        "max_count": opts.ping_max_count,
    });
    write.send(Message::Text(start_msg.to_string())).await.unwrap();

    let ping_start = Instant::now();
    let mut ping_stats: (u64, f64, f64, f64) = (0, 0.0, f64::MAX, 0.0); // count, sum, min, max
    let mut seq = 0;

    while ping_start.elapsed().as_millis() < opts.ping_duration_ms as u128 
        && (opts.ping_max_count == 0 || seq < opts.ping_max_count) {
        let t_send = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        let ping_msg = serde_json::json!({
            "phase": "ping",
            "seq": seq,
            "t_send": t_send,
        });
        write.send(Message::Text(ping_msg.to_string())).await.unwrap();

        // Wait for echo
        let mut echo_received = false;
        let timeout = Duration::from_millis(1000);
        let timeout_start = Instant::now();
        
        while !echo_received && timeout_start.elapsed() < timeout {
            if let Some(Ok(msg)) = read.next().await {
                match msg {
                    Message::Text(txt) => {
                        let response: serde_json::Value = serde_json::from_str(&txt).unwrap();
                        if let (Some(phase), Some(response_seq), Some(t_recv)) = (
                            response.get("phase").and_then(|v| v.as_str()),
                            response.get("seq").and_then(|v| v.as_u64()),
                            Some(std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64),
                        ) {
                            if phase == "ping" && response_seq == seq {
                                let rtt = t_recv - t_send;
                                ping_stats.0 += 1;
                                ping_stats.1 += rtt as f64;
                                ping_stats.2 = ping_stats.2.min(rtt as f64);
                                ping_stats.3 = ping_stats.3.max(rtt as f64);
                                echo_received = true;
                                
                                if ping_stats.0 % 10 == 0 {
                                    info!("Ping {} RTT: {:.2} ms", seq, rtt);
                                }
                            }
                        }
                    }
                    Message::Close(_) => {
                        error!("Connection closed during ping phase");
                        return;
                    }
                    _ => {}
                }
            }
        }
        
        if !echo_received {
            error!("Ping {} timeout", seq);
        }
        
        seq += 1;
    }

    // Send ping results
    let avg = if ping_stats.0 > 0 {
        ping_stats.1 / ping_stats.0 as f64
    } else {
        0.0
    };
    let min = if ping_stats.2 == f64::MAX { 0.0 } else { ping_stats.2 };
    
    let ping_done = serde_json::json!({
        "phase": "ping",
        "done": true,
        "test_id": test_id,
        "ping_count": ping_stats.0,
        "ping_avg_ms": avg,
        "ping_min_ms": min,
        "ping_max_ms": ping_stats.3,
    });
    write.send(Message::Text(ping_done.to_string())).await.unwrap();
    
    {
        let mut r = results.lock().await;
        r.ping_count = ping_stats.0;
        r.ping_avg_ms = avg;
        r.ping_min_ms = min;
        r.ping_max_ms = ping_stats.3;
    }
    
    info!("Ping phase complete: {} pings, avg {:.2} ms", ping_stats.0, avg);

    // Download phase
    info!("Starting download phase...");
    let download_start = serde_json::json!({
        "phase": "download",
        "action": "start",
        "test_id": test_id,
        "duration_ms": opts.download_duration_ms,
        "max_bytes": opts.download_max_bytes,
        "chunk_bytes": opts.chunk_bytes,
    });
    write.send(Message::Text(download_start.to_string())).await.unwrap();

    let mut download_bytes = 0u64;
    let download_start_time = Instant::now();
    
    while let Some(Ok(msg)) = read.next().await {
        match msg {
            Message::Binary(data) => {
                download_bytes += data.len() as u64;
            }
            Message::Text(txt) => {
                let response: serde_json::Value = serde_json::from_str(&txt).unwrap();
                if let (Some(phase), Some(done)) = (
                    response.get("phase").and_then(|v| v.as_str()),
                    response.get("done").and_then(|v| v.as_bool()),
                ) {
                    if phase == "download" && done {
                        let elapsed = download_start_time.elapsed().as_millis() as u64;
                        let rate_mbps = if elapsed > 0 {
                            (download_bytes as f64 * 8.0) / (elapsed as f64 * 1000.0)
                        } else {
                            0.0
                        };
                        
                        {
                            let mut r = results.lock().await;
                            r.download_bytes = download_bytes;
                            r.download_ms = elapsed;
                            r.download_mbps = rate_mbps;
                        }
                        
                        info!("Download phase complete: {} MB in {} ms ({:.2} Mbps)", 
                            download_bytes / 1_000_000, elapsed, rate_mbps);
                        break;
                    }
                }
            }
            Message::Close(_) => {
                error!("Connection closed during download phase");
                return;
            }
            _ => {}
        }
    }

    // Upload phase
    info!("Starting upload phase...");
    let upload_start = serde_json::json!({
        "phase": "upload",
        "action": "start",
        "test_id": test_id,
        "duration_ms": opts.upload_duration_ms,
        "max_bytes": opts.upload_max_bytes,
        "chunk_bytes": opts.chunk_bytes,
    });
    write.send(Message::Text(upload_start.to_string())).await.unwrap();

    // Generate random data
    let mut buf = vec![0u8; opts.chunk_bytes];
    getrandom::getrandom(&mut buf).expect("Failed to generate random data");
    
    let upload_start_time = Instant::now();
    let mut sent_bytes = 0u64;
    let mut upload_done = false;
    let mut last_log = Instant::now();
    
    info!("Upload phase: sending up to {} MB over {} ms", 
        opts.upload_max_bytes / 1_000_000, opts.upload_duration_ms);
    
    // Send chunks continuously while checking for server messages periodically
    let mut chunk_count = 0;
    while upload_start_time.elapsed().as_millis() < opts.upload_duration_ms as u128 
        && sent_bytes < opts.upload_max_bytes 
        && !upload_done {
        
        chunk_count += 1;
        if chunk_count == 1 {
            info!("Sending first chunk...");
        }
        
        // Send chunks in batches like download does
        const BATCH_SIZE: usize = 16;
        for _ in 0..BATCH_SIZE {
            // Check loop conditions before each send
            if upload_start_time.elapsed().as_millis() >= opts.upload_duration_ms as u128 
                || sent_bytes >= opts.upload_max_bytes {
                break;
            }
            
            // Send chunk
            debug!("Attempting to send chunk #{}", chunk_count);
            match write.send(Message::Binary(buf.clone())).await {
                Ok(_) => {
                    sent_bytes += buf.len() as u64;
                    chunk_count += 1;
                    debug!("Successfully sent chunk #{}", chunk_count);
                }
                Err(e) => {
                    error!("Failed to send chunk: {}", e);
                    break;
                }
            }
        }
        
        // Log progress every 1 second or every 10000 chunks
        if last_log.elapsed().as_millis() >= 1000 || chunk_count % 10000 == 0 {
            info!("Upload progress: {} MB sent ({} chunks)", sent_bytes / 1_000_000, chunk_count);
            last_log = Instant::now();
        }
        
        // Only check for server messages every 100ms (server only sends when done)
        if upload_start_time.elapsed().as_millis() % 100 == 0 {
            debug!("Checking for server messages...");
            // Try to read a message with a timeout
            match tokio::time::timeout(Duration::from_millis(1), read.next()).await {
                Ok(Some(Ok(msg))) => {
                    debug!("Got message from server");
                    match msg {
                        Message::Text(txt) => {
                            debug!("Received server message during upload: {}", txt);
                            let response: serde_json::Value = serde_json::from_str(&txt).unwrap();
                            if let (Some(phase), Some(done)) = (
                                response.get("phase").and_then(|v| v.as_str()),
                                response.get("done").and_then(|v| v.as_bool()),
                            ) {
                                if phase == "upload" && done {
                                    upload_done = true;
                                    let elapsed = upload_start_time.elapsed().as_millis() as u64;
                                    let rate_mbps = if elapsed > 0 {
                                        (sent_bytes as f64 * 8.0) / (elapsed as f64 * 1000.0)
                                    } else {
                                        0.0
                                    };
                                    
                                    {
                                        let mut r = results.lock().await;
                                        r.upload_bytes = sent_bytes;
                                        r.upload_ms = elapsed;
                                        r.upload_mbps = rate_mbps;
                                    }
                                    
                                    info!("Upload phase complete: {} MB in {} ms ({:.2} Mbps)", 
                                        sent_bytes / 1_000_000, elapsed, rate_mbps);
                                }
                            }
                        }
                        Message::Close(_) => {
                            error!("Connection closed during upload phase");
                            return;
                        }
                        _ => {}
                    }
                }
                Ok(_) => {
                    debug!("No message available");
                }
                Err(_) => {
                    debug!("Read timeout");
                }
            }
        }
        
        // Check loop conditions
        let elapsed = upload_start_time.elapsed().as_millis();
        if elapsed >= opts.upload_duration_ms as u128 {
            info!("Time limit reached: {} ms", elapsed);
            break;
        }
        if sent_bytes >= opts.upload_max_bytes {
            info!("Byte limit reached: {} bytes", sent_bytes);
            break;
        }
        
        // Yield to scheduler but don't sleep - let the network stack handle flow control
        tokio::task::yield_now().await;
    }
    
    info!("Upload loop finished - sent: {} MB, done: {}, elapsed: {} ms", 
        sent_bytes / 1_000_000, upload_done, upload_start_time.elapsed().as_millis());

    // Update results if upload wasn't marked as done by server
    if !upload_done && sent_bytes > 0 {
        let elapsed = upload_start_time.elapsed().as_millis() as u64;
        let rate_mbps = if elapsed > 0 {
            (sent_bytes as f64 * 8.0) / (elapsed as f64 * 1000.0)
        } else {
            0.0
        };
        
        {
            let mut r = results.lock().await;
            r.upload_bytes = sent_bytes;
            r.upload_ms = elapsed;
            r.upload_mbps = rate_mbps;
        }
        
        info!("Upload completed by client: {} MB in {} ms ({:.2} Mbps)", 
            sent_bytes / 1_000_000, elapsed, rate_mbps);
    }

    // Send final upload done if server didn't stop us
    if !upload_done {
        let elapsed = upload_start_time.elapsed().as_millis() as u64;
        let upload_done_msg = serde_json::json!({
            "phase": "upload",
            "done": true,
            "test_id": test_id,
            "bytes_sent": sent_bytes,
            "elapsed_ms": elapsed,
        });
        write.send(Message::Text(upload_done_msg.to_string())).await.unwrap();
    }

    // Print results
    let r = results.lock().await;
    
    // Single info log with all results
    info!("Speed Test Complete: Ping: {} packets, avg {:.2} ms | Download: {:.2} MB in {:.2} Mbps | Upload: {:.2} MB in {:.2} Mbps", 
        r.ping_count, r.ping_avg_ms,
        r.download_bytes as f64 / 1_000_000.0, r.download_mbps,
        r.upload_bytes as f64 / 1_000_000.0, r.upload_mbps);
    
    match opts.format.as_str() {
        "json" => {
            println!("{}", serde_json::json!({
                "ping": {
                    "count": r.ping_count,
                    "avg_ms": r.ping_avg_ms,
                    "min_ms": r.ping_min_ms,
                    "max_ms": r.ping_max_ms,
                },
                "download": {
                    "bytes": r.download_bytes,
                    "ms": r.download_ms,
                    "mbps": r.download_mbps,
                },
                "upload": {
                    "bytes": r.upload_bytes,
                    "ms": r.upload_ms,
                    "mbps": r.upload_mbps,
                }
            }));
        }
        _ => {
            println!("\n=== Speed Test Results ===");
            println!("Ping: {} packets, avg {:.2} ms (min {:.2} ms, max {:.2} ms)", 
                r.ping_count, r.ping_avg_ms, r.ping_min_ms, r.ping_max_ms);
            println!("Download: {:.2} MB in {} ms ({:.2} Mbps)", 
                r.download_bytes as f64 / 1_000_000.0, r.download_ms, r.download_mbps);
            println!("Upload: {:.2} MB in {} ms ({:.2} Mbps)", 
                r.upload_bytes as f64 / 1_000_000.0, r.upload_ms, r.upload_mbps);
        }
    }
    
    // Close connection
    let _ = write.close().await;
}
