import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";

const bridgeRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(bridgeRoot, "../..");
const binary = path.resolve(process.env.LUVUS_BIN || path.join(repoRoot, "target/debug/luvus"));
const homePrefix = path.join(os.homedir(), ".luvus-web-integration-");
const home = await mkdtemp(homePrefix);
const session = `web-integration-${process.pid}`;
const commonEnv = { ...process.env, LUVUS_HOME: home };
delete commonEnv.LUVUS_SOCKET_PATH;
delete commonEnv.LUVUS_SESSION;

let bridge;
try {
  command(binary, ["--session", session, "server", "restart"], commonEnv);
  bridge = spawn(process.execPath, [path.join(bridgeRoot, "dist/index.js")], {
    cwd: bridgeRoot,
    env: {
      ...commonEnv,
      LUVUS_BIN: binary,
      LUVUS_SESSION: session,
      LUVUS_WEB_PORT: "0",
      LUVUS_WEB_CONTROL: "1",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const descriptor = JSON.parse(await childLine(bridge, 10_000));
  assert.equal(descriptor.type, "luvus_web_bridge");
  const page = new URL(descriptor.url);
  const code = new URLSearchParams(page.hash.slice(1)).get("pair");
  assert.ok(code);
  const origin = page.origin;
  const socketUrl = `${page.protocol === "https:" ? "wss:" : "ws:"}//${page.host}/bridge`;

  await assert.rejects(connect(socketUrl, "https://invalid.example"));
  const first = await connect(socketUrl, origin);
  const ready = await exchange(first, { type: "authenticate", code }, "ready");
  assert.equal(ready.authority.mode, "control");
  assert.ok(ready.ticket);
  first.close();

  const socket = await connect(socketUrl, origin);
  const paired = await exchange(socket, { type: "authenticate", ticket: ready.ticket }, "ready");
  assert.equal(paired.ticket, undefined);
  const capabilities = await request(socket, "caps", "uhp.capabilities", {});
  assert.equal(capabilities.type, "uhp_capabilities");
  assert.ok(capabilities.access.allowed_methods.includes("terminal.backend.control"));
  assert.ok(capabilities.access.allowed_methods.includes("search.query"));
  assert.ok(capabilities.terminal.features.includes("stream_cursor"));
  const completions = await request(socket, "complete", "search.query", {
    query: "Cargo",
    scope: "files",
    case_sensitive: false,
    all_sessions: false,
    limit: 8,
  });
  assert.equal(completions.type, "search_query");
  assert.ok(completions.matches.some((match) => match.kind === "file"));

  const eventAck = waitFor(socket, (frame) => frame.type === "response" && frame.id === "events");
  socket.send(JSON.stringify({ type: "stream.open", id: "events", method: "events.subscribe", params: {} }));
  assert.equal((await eventAck).result.type, "subscription_started");

  const snapshot = await request(socket, "snapshot", "session.snapshot", {});
  assert.equal(snapshot.type, "session_snapshot");
  const pane = snapshot.workspaces.flatMap((workspace) => workspace.tabs)
    .flatMap((tab) => tab.panes)
    .find((candidate) => candidate.kind === "terminal" && candidate.terminal_id);
  assert.ok(pane, "isolated server should expose a terminal pane");

  const controlAck = waitFor(socket, (frame) => frame.type === "response" && frame.id === "control");
  socket.send(JSON.stringify({
    type: "stream.open",
    id: "control",
    method: "terminal.backend.control",
    params: {
      server_generation: snapshot.server_generation,
      terminal_id: pane.terminal_id,
      pane_id: pane.pane_id,
      mode: "recent_unwrapped",
      lines: 80,
      ansi: true,
      cursor: true,
    },
  }));
  assert.equal((await controlAck).result.type, "terminal_backend_stream");
  const marker = `LUVUS_WEB_${process.pid}`;
  const actionReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "action");
  const output = waitFor(socket, (frame) => frame.type === "stream.frame"
    && frame.stream_id === "control"
    && frame.frame?.event === "terminal.frame"
    && frame.frame.data?.text?.includes(marker), 10_000);
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "action",
    action: "submit_text",
    // Keep the final marker out of the echoed command line, and emit it in a
    // later PTY burst. The stream must deliver that tail without needing a
    // second command to wake it.
    params: { text: `printf LUVUS_WEB_; sleep 0.2; printf '${process.pid}\\n'` },
  }));
  assert.equal((await actionReply).result.type, "terminal_backend_action");
  const outputFrame = await output;
  assert.ok(Object.hasOwn(outputFrame.frame.data, "cursor"));
  assert.ok(outputFrame.frame.data.cursor === null
    || Number.isSafeInteger(outputFrame.frame.data.cursor.offset));

  const pasteReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "paste");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "paste",
    action: "paste_text",
    params: { text: "printf '\\120\\101\\123\\124\\105\\137\\117\\113\\n'" },
  }));
  assert.equal((await pasteReply).result.type, "terminal_backend_action");
  const pasteOutput = waitFor(socket, (frame) => frame.type === "stream.frame"
    && frame.stream_id === "control"
    && frame.frame?.event === "terminal.frame"
    && frame.frame.data?.text?.includes("PASTE_OK"), 10_000);
  const enterReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "enter");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "enter",
    action: "send_key",
    params: { key: "enter" },
  }));
  assert.equal((await enterReply).result.type, "terminal_backend_action");
  await pasteOutput;

  const uploadStartReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "upload-start");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "upload-start",
    action: "upload_start",
    params: { name: "remote note.txt", size: 8 },
  }));
  const uploadStart = await uploadStartReply;
  assert.equal(uploadStart.result.type, "terminal_upload");
  assert.match(uploadStart.result.upload_id, /^[0-9a-f]{32}$/);
  const uploadChunkReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "upload-chunk");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "upload-chunk",
    action: "upload_chunk",
    params: { upload_id: uploadStart.result.upload_id, offset: 0, data_base64: "d2ViIGZpbGU=" },
  }));
  assert.equal((await uploadChunkReply).result.received, 8);
  const uploadPath = waitFor(socket, (frame) => frame.type === "stream.frame"
    && frame.stream_id === "control"
    && frame.frame?.event === "terminal.frame"
    && frame.frame.data?.text?.includes("remote_note.txt"), 10_000);
  const uploadFinishReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "upload-finish");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "upload-finish",
    action: "upload_finish",
    params: { upload_id: uploadStart.result.upload_id },
  }));
  assert.equal((await uploadFinishReply).result.type, "terminal_backend_action");
  await uploadPath;
  const clearUploadReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "clear-upload");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "clear-upload",
    action: "send_key",
    params: { key: "ctrl-c" },
  }));
  assert.equal((await clearUploadReply).result.type, "terminal_backend_action");

  const imageReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "image");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "image",
    action: "paste_image",
    params: {
      png_base64: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgZGL+DwABFAEG1rmmRQAAAABJRU5ErkJggg==",
    },
  }));
  assert.equal((await imageReply).result.type, "terminal_backend_action");
  const cancelReply = waitFor(socket, (frame) => frame.type === "response" && frame.id === "cancel-image");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "cancel-image",
    action: "send_key",
    params: { key: "ctrl-c" },
  }));
  assert.equal((await cancelReply).result.type, "terminal_backend_action");

  command(binary, ["--session", session, "server", "restart"], commonEnv);
  const restored = await retryRequest(socket, "restored", "session.snapshot", {}, 12_000);
  assert.equal(restored.type, "session_snapshot");
  assert.notEqual(restored.server_generation, snapshot.server_generation);
  socket.close();
  process.stdout.write("live web bridge integration passed\n");
} finally {
  bridge?.kill("SIGTERM");
  command(binary, ["--session", session, "server", "stop"], commonEnv, true);
  if (home.startsWith(homePrefix)) await rm(home, { recursive: true, force: true });
}

function command(executable, args, env, allowFailure = false) {
  const result = spawnSync(executable, args, { cwd: repoRoot, env, encoding: "utf8", timeout: 20_000 });
  if (!allowFailure && result.status !== 0) throw new Error(result.stderr || result.stdout || `command failed: ${args.join(" ")}`);
}

function childLine(child, timeoutMs) {
  return new Promise((resolve, reject) => {
    let stdout = "", stderr = "";
    const timer = setTimeout(() => fail(new Error("bridge startup timed out")), timeoutMs);
    const fail = (error) => { clearTimeout(timer); reject(new Error(`${error.message}\n${stderr}`)); };
    child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-4096); });
    child.once("error", fail);
    child.once("exit", (code) => fail(new Error(`bridge exited with ${code}`)));
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
      const newline = stdout.indexOf("\n");
      if (newline >= 0) { clearTimeout(timer); resolve(stdout.slice(0, newline)); }
    });
  });
}

function connect(url, origin) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url, { origin });
    const timer = setTimeout(() => { socket.terminate(); reject(new Error("WebSocket connection timed out")); }, 5_000);
    socket.once("open", () => { clearTimeout(timer); resolve(socket); });
    socket.once("unexpected-response", (_request, response) => {
      clearTimeout(timer);
      reject(new Error(`WebSocket rejected with ${response.statusCode}`));
    });
    socket.once("error", (error) => { clearTimeout(timer); reject(error); });
  });
}

function exchange(socket, frame, expectedType) {
  const response = waitFor(socket, (candidate) => candidate.type === expectedType);
  socket.send(JSON.stringify(frame));
  return response;
}

async function request(socket, id, method, params) {
  const response = waitFor(socket, (frame) => frame.type === "response" && frame.id === id);
  socket.send(JSON.stringify({ type: "request", id, method, params }));
  const frame = await response;
  if (frame.error) throw new Error(`${frame.error.code}: ${frame.error.message}`);
  return frame.result;
}

async function retryRequest(socket, prefix, method, params, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let attempt = 0;
  let lastError;
  while (Date.now() < deadline) {
    try {
      return await request(socket, `${prefix}-${++attempt}`, method, params);
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
  throw lastError || new Error("request retry timed out");
}

function waitFor(socket, predicate, timeoutMs = 5_000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => finish(new Error("WebSocket response timed out")), timeoutMs);
    const onClose = () => finish(new Error("WebSocket closed before response"));
    const onMessage = (raw) => {
      let frame;
      try { frame = JSON.parse(raw.toString()); }
      catch { return finish(new Error("Bridge returned invalid JSON")); }
      if (predicate(frame)) finish(undefined, frame);
    };
    const finish = (error, frame) => {
      clearTimeout(timer);
      socket.off("message", onMessage);
      socket.off("close", onClose);
      if (error) reject(error); else resolve(frame);
    };
    socket.on("message", onMessage);
    socket.once("close", onClose);
  });
}
