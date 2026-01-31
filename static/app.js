(() => {
  const logEl = document.getElementById("log");
  const verboseToggle = document.getElementById("verbose");
  const wsInput = document.getElementById("wsUrl");

  const defaultWsUrl = () => {
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    return `${proto}//${window.location.host}/ws`;
  };

  // Auto-fill WS URL from the current page location
  wsInput.value = defaultWsUrl();

  const log = (msg, { verbose = false } = {}) => {
    if (verbose && !verboseToggle.checked) return;
    const t = new Date().toISOString();
    logEl.textContent += `[${t}] ${msg}\n`;
    logEl.scrollTop = logEl.scrollHeight;
  };

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const waitForBufferedAmount = (ws, threshold = 256_000) =>
    new Promise((resolve) => {
      if (ws.bufferedAmount < threshold) return resolve();
      // Poll at 1ms intervals - browsers clamp to ~4ms but that's still faster than rAF
      const id = setInterval(() => {
        if (ws.bufferedAmount < threshold) {
          clearInterval(id);
          resolve();
        }
      }, 1);
    });

  // Yield via microtask queue (faster than setTimeout which has 4ms minimum)
  const yieldMicrotask = () => new Promise((resolve) => queueMicrotask(resolve));

  // Accepts B, K, KB, KiB, M, MB, MiB, G, GB, GiB (case-insensitive)
  const parseBytesInput = (s) => {
    if (!s || !s.trim()) throw new Error("Empty byte value");
    const m = s.trim().match(/^(\d+(?:\.\d+)?)(?:\s*(b|k|kb|kib|m|mb|mib|g|gb|gib))?$/i);
    if (!m) throw new Error("Invalid byte value");
    const num = parseFloat(m[1]);
    const unit = (m[2] || "b").toLowerCase();
    const mul = (() => {
      switch (unit) {
        case "b":
          return 1;
        case "k":
        case "kb":
        case "kib":
          return 1024;
        case "m":
        case "mb":
        case "mib":
          return 1024 ** 2;
        case "g":
        case "gb":
        case "gib":
          return 1024 ** 3;
        default:
          return 1;
      }
    })();
    return Math.round(num * mul);
  };

  document.getElementById("start").addEventListener("click", run);

  async function loadDefaults() {
    try {
      const cfg = await fetch("/config").then((r) => r.json());
      document.getElementById("chunkBytes").value = cfg.chunk_bytes;
      document.getElementById("pingDuration").value = cfg.ping_duration_ms;
      document.getElementById("pingMaxCount").value = cfg.ping_max_count ?? "";
      document.getElementById("dlDuration").value = cfg.download_duration_ms;
      document.getElementById("dlMaxBytes").value = cfg.download_max_bytes;
      document.getElementById("ulDuration").value = cfg.upload_duration_ms;
      document.getElementById("ulMaxBytes").value = cfg.upload_max_bytes;
    } catch (e) {
      console.warn("Failed to load defaults from /config", e);
    }
  }

  loadDefaults();

  async function run() {
    try {
      const url = wsInput.value;
      const chunkBytes = parseBytesInput(document.getElementById("chunkBytes").value);
      const pingDuration = parseInt(document.getElementById("pingDuration").value, 10);
      const pingMaxCountInput = document.getElementById("pingMaxCount").value;
      const pingMaxCount = pingMaxCountInput ? parseInt(pingMaxCountInput, 10) : null;
      const dlDuration = parseInt(document.getElementById("dlDuration").value, 10);
      const dlMaxBytes = parseBytesInput(document.getElementById("dlMaxBytes").value);
      const ulDuration = parseInt(document.getElementById("ulDuration").value, 10);
      const ulMaxBytes = parseBytesInput(document.getElementById("ulMaxBytes").value);

      log(
        `===== Starting LAN speed test: ws=${url} chunk=${chunkBytes}B ping_dur=${pingDuration}ms ping_max=${pingMaxCount ?? "none"} dl_dur=${dlDuration}ms dl_max=${dlMaxBytes}B ul_dur=${ulDuration}ms ul_max=${ulMaxBytes}B =====`
      );

      const t0 = performance.now();
      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";

      // Generate a unique test ID for this run
      const testId = Date.now();
      
      const ctx = {
        ws,
        testId,
        chunkBytes,
        pingDuration,
        pingMaxCount,
        dlDuration,
        dlMaxBytes,
        ulDuration,
        ulMaxBytes,
        uploadServerDone: false,
        uploadDoneAck: null,
        downloadResult: null,
        uploadResult: null,
      };

      ws.onopen = () => {
        const dt = (performance.now() - t0).toFixed(2);
        log(`WebSocket open (dt=${dt} ms)`, { verbose: true });
      };
      ws.onclose = (e) => {
        log(
          `WebSocket closed: code=${e.code} reason=${e.reason || "<none>"} wasClean=${e.wasClean}`
        );
      };
      ws.onerror = (e) => {
        log(
          `WebSocket error: ${e.message || e.type || e} ${
            e?.currentTarget?.readyState !== undefined
              ? `(readyState=${e.currentTarget.readyState})`
              : ""
          }`
        );
      };

      await once(ws, "open");
      log("Connected", { verbose: true });

      ws.onmessage = (evt) => {
        if (typeof evt.data === "string") {
          const msg = JSON.parse(evt.data);
          handleControl(msg, ctx);
        } else {
          // Binary (download data)
          ctx.downloadBytes = (ctx.downloadBytes || 0) + evt.data.byteLength;
        }
      };

      await pingPhase(ctx);
      await downloadPhase(ctx);
      await uploadPhase(ctx);

      const pingAvg = ctx.pingStats.count ? ctx.pingStats.sum / ctx.pingStats.count : 0;
      const dlRate = ctx.downloadResult?.rate_mbps ?? 0;
      const ulRate = ctx.uploadResult?.rate_mbps ?? 0;
      log(
        `===== Summary: ping_avg=${pingAvg.toFixed(2)} ms download=${dlRate.toFixed(
          2
        )} Mbps upload=${ulRate.toFixed(2)} Mbps =====`
      );
      log("All phases done.");
      
      // Close the WebSocket cleanly
      // Create a promise that resolves when close completes with proper code
      const closePromise = new Promise((resolve) => {
        const originalOnClose = ws.onclose;
        ws.onclose = (e) => {
          if (originalOnClose) originalOnClose(e);
          resolve(e);
        };
      });
      
      ws.close(1000, "test complete");
      await closePromise;
    } catch (err) {
      log(`Error: ${err.message || err}`);
    }
  }

  function once(obj, event) {
    return new Promise((resolve) => obj.addEventListener(event, resolve, { once: true }));
  }

  // ---------------- Ping ----------------
  async function pingPhase(ctx) {
    log(">>> Ping phase starting");
    ctx.pingStats = { count: 0, sum: 0, min: Infinity, max: 0 };
    ctx.awaiting = new Map();

    ctx.ws.send(
      JSON.stringify({
        phase: "ping",
        action: "start",
        test_id: ctx.testId,
        duration_ms: ctx.pingDuration,
        max_count: ctx.pingMaxCount ?? undefined,
      })
    );

    const endTime = performance.now() + ctx.pingDuration;
    let seq = 0;
    while (performance.now() < endTime) {
      if (ctx.pingMaxCount && seq >= ctx.pingMaxCount) break;
      const tSend = nowMs();
      ctx.awaiting.set(seq, tSend);
      ctx.ws.send(
        JSON.stringify({
          phase: "ping",
          seq,
          t_send: tSend,
        })
      );
      await waitForEcho(ctx, seq);
      seq += 1;
    }

    const avg = ctx.pingStats.count ? ctx.pingStats.sum / ctx.pingStats.count : 0;
    const minVal = ctx.pingStats.min === Infinity ? 0 : ctx.pingStats.min;
    const maxVal = ctx.pingStats.max;
    
    // Send ping stats to server
    ctx.ws.send(JSON.stringify({
      phase: "ping",
      done: true,
      test_id: ctx.testId,
      ping_count: ctx.pingStats.count,
      ping_avg_ms: avg,
      ping_min_ms: minVal,
      ping_max_ms: maxVal,
    }));
    log(
      `Ping done: count=${ctx.pingStats.count} avg=${avg.toFixed(2)} ms min=${ctx.pingStats.min.toFixed(
        2
      )} ms max=${ctx.pingStats.max.toFixed(2)} ms`
    );
  }

  function waitForEcho(ctx, seq) {
    return new Promise((resolve) => {
      const check = setInterval(() => {
        if (!ctx.awaiting.has(seq)) {
          clearInterval(check);
          resolve();
        }
      }, 1);
    });
  }

  function handleControl(msg, ctx) {
    if (msg.phase === "ping" && msg.t_send !== undefined) {
      const seq = msg.seq;
      const tSend = ctx.awaiting.get(seq);
      if (tSend != null) {
        const rtt = nowMs() - tSend;
        ctx.awaiting.delete(seq);
        ctx.pingStats.count += 1;
        ctx.pingStats.sum += rtt;
        ctx.pingStats.min = Math.min(ctx.pingStats.min, rtt);
        ctx.pingStats.max = Math.max(ctx.pingStats.max, rtt);
        log(`Ping seq=${seq} rtt=${rtt.toFixed(2)} ms`, { verbose: true });
      }
    } else if (msg.phase === "download") {
      if (!msg.done) {
        log(`Download interim: ${(msg.bytes / 1e6).toFixed(2)} MB in ${msg.elapsed_ms} ms`, {
          verbose: true,
        });
      } else {
        const rateMbps = msg.elapsed_ms > 0 ? (msg.bytes * 8) / msg.elapsed_ms / 1000 : 0;
        ctx.downloadResult = {
          bytes: msg.bytes,
          elapsed_ms: msg.elapsed_ms,
          rate_mbps: rateMbps,
          reason: msg.reason,
        };
        log(
          `Download done: ${(msg.bytes / 1e6).toFixed(2)} MB in ${msg.elapsed_ms} ms reason=${msg.reason} (${rateMbps.toFixed(
            2
          )} Mbps)`
        );
      }
    } else if (msg.phase === "upload") {
      if (!msg.done) {
        log(`Upload interim: ${(msg.bytes / 1e6).toFixed(2)} MB in ${msg.elapsed_ms} ms`, {
          verbose: true,
        });
      } else {
        const rateMbps = msg.elapsed_ms > 0 ? (msg.bytes * 8) / msg.elapsed_ms / 1000 : 0;
        ctx.uploadServerDone = true;
        ctx.uploadServerResult = msg;
        ctx.uploadResult = {
          bytes: msg.bytes,
          elapsed_ms: msg.elapsed_ms,
          rate_mbps: rateMbps,
          reason: msg.reason,
        };
        log(
          `Upload done: ${(msg.bytes / 1e6).toFixed(2)} MB in ${msg.elapsed_ms} ms reason=${msg.reason} (${rateMbps.toFixed(
            2
          )} Mbps)`
        );
        ctx.uploadDoneAck?.();
      }
    }
  }

  // ---------------- Download ----------------
  async function downloadPhase(ctx) {
    log(">>> Download phase starting");
    ctx.downloadBytes = 0;
    ctx.ws.send(
      JSON.stringify({
        phase: "download",
        action: "start",
        test_id: ctx.testId,
        duration_ms: ctx.dlDuration,
        max_bytes: ctx.dlMaxBytes,
        chunk_bytes: ctx.chunkBytes,
      })
    );

    await new Promise((resolve) => {
      const handler = (evt) => {
        if (typeof evt.data === "string") {
          const msg = JSON.parse(evt.data);
          if (msg.phase === "download" && msg.done) {
            ctx.ws.removeEventListener("message", handler);
            resolve();
          }
        }
      };
      ctx.ws.addEventListener("message", handler);
    });

    if (ctx.downloadResult) {
      log(
        `Download collected ${(ctx.downloadResult.bytes / 1e6).toFixed(
          2
        )} MB (${ctx.downloadResult.rate_mbps.toFixed(2)} Mbps)`
      );
    }
  }

  // ---------------- Upload ----------------
  async function uploadPhase(ctx) {
    log(">>> Upload phase starting");
    const buf = new Uint8Array(ctx.chunkBytes);
    crypto.getRandomValues(buf);

    ctx.uploadServerDone = false;
    ctx.uploadServerResult = null;
    ctx.uploadResult = null;
    const ackPromise = new Promise((resolve) => (ctx.uploadDoneAck = resolve));

    ctx.ws.send(
      JSON.stringify({
        phase: "upload",
        action: "start",
        test_id: ctx.testId,
        duration_ms: ctx.ulDuration,
        max_bytes: ctx.ulMaxBytes,
        chunk_bytes: ctx.chunkBytes,
      })
    );

    const start = performance.now();
    const endTime = start + ctx.ulDuration;
    let sent = 0;
    
    // Larger batch size - send more before yielding
    const BATCH_SIZE = 64;
    // Higher buffer threshold for better throughput (4MB high, 2MB low)
    const BUFFER_HIGH = 4_000_000;
    const BUFFER_LOW = 2_000_000;
    
    // Track iterations for periodic time/macrotask checks
    let iterations = 0;

    let backpressureWaits = 0;
    let lastLogTime = start;
    
    while (!ctx.uploadServerDone && sent < ctx.ulMaxBytes) {
      iterations++;
      
      // Check time periodically
      if (iterations % 16 === 0) {
        if (performance.now() >= endTime) break;
      }
      
      // Backpressure check - only when buffer is very full
      if (ctx.ws.bufferedAmount > BUFFER_HIGH) {
        backpressureWaits++;
        await waitForBufferedAmount(ctx.ws, BUFFER_LOW);
        continue;
      }
      
      // Batch send multiple chunks
      for (let i = 0; i < BATCH_SIZE && sent < ctx.ulMaxBytes && !ctx.uploadServerDone; i++) {
        ctx.ws.send(buf);
        sent += buf.byteLength;
      }
      
      // Debug: log progress every second
      const now = performance.now();
      if (now - lastLogTime > 1000) {
        log(`Upload progress: ${(sent / 1e6).toFixed(2)} MB, iterations=${iterations}, backpressureWaits=${backpressureWaits}, bufferedAmount=${ctx.ws.bufferedAmount}`, { verbose: true });
        lastLogTime = now;
      }
      
      // Every 4 iterations, use setTimeout to allow macrotask queue (WebSocket messages) to process
      // This is critical - microtasks don't yield to the event loop for onmessage handlers
      if (iterations % 4 === 0) {
        await sleep(0); // macrotask yield - allows onmessage to fire
      } else {
        // Yield via microtask for most iterations (fast)
        await yieldMicrotask();
      }
    }
    
    log(`Upload loop ended: iterations=${iterations}, backpressureWaits=${backpressureWaits}`, { verbose: true });

    if (!ctx.uploadServerDone) {
      const elapsed = performance.now() - start;
      ctx.ws.send(
        JSON.stringify({
          phase: "upload",
          done: true,
          test_id: ctx.testId,
          bytes_sent: sent,
          elapsed_ms: Math.round(elapsed),
        })
      );
    }

    await ackPromise; // wait for server's done/ack message

    if (ctx.uploadResult) {
      log(
        `Upload sent ${(ctx.uploadResult.bytes / 1e6).toFixed(2)} MB (${ctx.uploadResult.rate_mbps.toFixed(
          2
        )} Mbps)`
      );
    }
  }

  function nowMs() {
    return Math.trunc(performance.now());
  }
})();
