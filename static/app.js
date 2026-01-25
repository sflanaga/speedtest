(() => {
  const logEl = document.getElementById("log");
  const verboseToggle = document.getElementById("verbose");
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
      const id = setInterval(() => {
        if (ws.bufferedAmount < threshold) {
          clearInterval(id);
          resolve();
        }
      }, 1);
    });

  const parseBytesInput = (s) => {
    if (!s || !s.trim()) throw new Error("Empty byte value");
    const m = s.trim().match(/^(\d+(?:\.\d+)?)(?:\s*(b|kb|kib|mb|mib|gb|gib))?$/i);
    if (!m) throw new Error("Invalid byte value");
    const num = parseFloat(m[1]);
    const unit = (m[2] || "b").toLowerCase();
    const mul = (() => {
      switch (unit) {
        case "b":
          return 1;
        case "kb":
        case "kib":
          return 1024;
        case "mb":
        case "mib":
          return 1024 ** 2;
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
      const url = document.getElementById("wsUrl").value;
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

      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";

      const ctx = {
        ws,
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

      ws.onclose = () => log("WebSocket closed");
      ws.onerror = (e) => log(`WebSocket error: ${e.message || e}`);

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

    ctx.ws.send(JSON.stringify({ phase: "ping", done: true }));
    const avg = ctx.pingStats.count ? ctx.pingStats.sum / ctx.pingStats.count : 0;
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
        duration_ms: ctx.ulDuration,
        max_bytes: ctx.ulMaxBytes,
        chunk_bytes: ctx.chunkBytes,
      })
    );

    const start = performance.now();
    let sent = 0;

    while (
      !ctx.uploadServerDone &&
      performance.now() - start < ctx.ulDuration &&
      sent < ctx.ulMaxBytes
    ) {
      if (ctx.ws.bufferedAmount > 512_000) {
        await waitForBufferedAmount(ctx.ws, 256_000);
      }
      ctx.ws.send(buf);
      sent += buf.byteLength;
      if (sent % (ctx.chunkBytes * 8) === 0) await sleep(0);
    }

    if (!ctx.uploadServerDone) {
      const elapsed = performance.now() - start;
      ctx.ws.send(
        JSON.stringify({
          phase: "upload",
          done: true,
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