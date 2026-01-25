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

  document.getElementById("start").addEventListener("click", run);

  async function run() {
    const url = document.getElementById("wsUrl").value;
    const chunkBytes = parseInt(document.getElementById("chunkBytes").value, 10);
    const pingDuration = parseInt(document.getElementById("pingDuration").value, 10);
    const pingMaxCountInput = document.getElementById("pingMaxCount").value;
    const pingMaxCount = pingMaxCountInput ? parseInt(pingMaxCountInput, 10) : null;
    const dlDuration = parseInt(document.getElementById("dlDuration").value, 10);
    const dlMaxBytes = parseInt(document.getElementById("dlMaxBytes").value, 10);
    const ulDuration = parseInt(document.getElementById("ulDuration").value, 10);
    const ulMaxBytes = parseInt(document.getElementById("ulMaxBytes").value, 10);

    log(`Connecting to ${url} ...`, { verbose: true });
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

    log("All phases done.");
  }

  function once(obj, event) {
    return new Promise((resolve) => obj.addEventListener(event, resolve, { once: true }));
  }

  // ---------------- Ping ----------------
  async function pingPhase(ctx) {
    log("Starting ping phase", { verbose: true });
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

    // Tell server we're done
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
        const rtt = nowMs() - tSend; // measure with client clock to avoid skew
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
        log(
          `Download done: ${(msg.bytes / 1e6).toFixed(2)} MB in ${msg.elapsed_ms} ms reason=${msg.reason}`
        );
      }
    } else if (msg.phase === "upload") {
      if (!msg.done) {
        log(`Upload interim: ${(msg.bytes / 1e6).toFixed(2)} MB in ${msg.elapsed_ms} ms`, {
          verbose: true,
        });
      } else {
        log(
          `Upload done: ${(msg.bytes / 1e6).toFixed(2)} MB in ${msg.elapsed_ms} ms reason=${msg.reason}`
        );
        ctx.uploadDoneAck?.();
      }
    }
  }

  // ---------------- Download ----------------
  async function downloadPhase(ctx) {
    log("Starting download phase", { verbose: true });
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

    // Wait for done control
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

    log(
      `Download collected ${(ctx.downloadBytes / 1e6).toFixed(
        2
      )} MB (${(ctx.downloadBytes * 8 / ctx.dlDuration / 1e3).toFixed(2)} Mbps approximate)`
    );
  }

  // ---------------- Upload ----------------
  async function uploadPhase(ctx) {
    log("Starting upload phase", { verbose: true });
    const buf = new Uint8Array(ctx.chunkBytes);
    crypto.getRandomValues(buf);

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

    while (performance.now() - start < ctx.ulDuration && sent < ctx.ulMaxBytes) {
      ctx.ws.send(buf);
      sent += buf.byteLength;
      if (sent % (ctx.chunkBytes * 8) === 0) await sleep(0);
    }

    const elapsed = performance.now() - start;
    const ackPromise = new Promise((resolve) => (ctx.uploadDoneAck = resolve));
    ctx.ws.send(
      JSON.stringify({
        phase: "upload",
        done: true,
        bytes_sent: sent,
        elapsed_ms: Math.round(elapsed),
      })
    );
    await ackPromise;
    log(
      `Upload sent ${(sent / 1e6).toFixed(2)} MB in ${elapsed.toFixed(
        1
      )} ms (~${((sent * 8) / elapsed / 1e3).toFixed(2)} Mbps)`
    );
  }

  function nowMs() {
    return Math.trunc(performance.now());
  }
})();