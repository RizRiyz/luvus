import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(webRoot, "..");
const executable = path.resolve(process.env.LUVUS_BIN
  || path.join(repoRoot, "target", "debug", process.platform === "win32" ? "luvus.exe" : "luvus"));
const home = await mkdtemp(path.join(os.tmpdir(), "luvus-native-web-"));
const workspace = path.join(home, "workspace");
await mkdir(workspace);
const session = "native-web-" + process.pid;
const env = { ...process.env, LUVUS_HOME: home };
delete env.LUVUS_SOCKET_PATH;
delete env.LUVUS_SESSION;
let child;
let stderr = "";
const sockets = new Set();

try {
  const paired = await startBridge();
  const code = paired.hash.startsWith("#pair=") ? decodeURIComponent(paired.hash.slice(6)) : "";
  assert.ok(code);

  const { socket, ready } = await authenticate(paired, { code });
  assert.equal(ready.authority.mode, "control");
  assert.equal(ready.expires_on_close, true);
  assert.equal(ready.expires_at, undefined);
  assert.ok(ready.ticket);
  const second = await authenticate(paired, { ticket: ready.ticket });
  assert.equal(second.ready.expires_on_close, true);
  assert.equal(second.ready.ticket, undefined, "another tab reuses the same grant");
  assert.equal((await request(second.socket, "tab-devices", "web.devices.status", {})).paired_devices, 1);
  await request(second.socket, "tab-snapshot", "session.snapshot", {});
  await rejected(paired, { code }); // Spent links still cannot pair a different browser.

  const capabilities = await request(socket, "capabilities", "uhp.capabilities", {});
  assert.equal(capabilities.type, "uhp_capabilities");
  assert.ok(capabilities.access.allowed_methods.includes("terminal.backend.control"));

  const deviceStatus = await request(socket, "device-status", "web.devices.status", {});
  assert.equal(deviceStatus.public_url, null);
  const publicStatus = await request(socket, "public-url", "web.devices.set_public_url", {
    url: "https://phone.example",
  });
  assert.equal(publicStatus.public_url, "https://phone.example");
  const phonePairing = await request(socket, "phone-pairing", "web.devices.create_pairing", {});
  assert.equal(phonePairing.url, `https://phone.example/#pair=${encodeURIComponent(phonePairing.code)}`);
  const phone = await authenticate(paired, { code: phonePairing.code });
  assert.notEqual(phone.ready.ticket, ready.ticket);
  assert.equal((await request(phone.socket, "phone-devices", "web.devices.status", {})).paired_devices, 2);

  const snapshot = await request(socket, "snapshot", "session.snapshot", {});
  const pane = snapshot.workspaces.flatMap((workspace) => workspace.tabs)
    .flatMap((tab) => tab.panes)
    .find((candidate) => candidate.kind === "terminal" && candidate.terminal_id);
  assert.ok(pane);

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
  const marker = "NATIVE_WEB_" + process.pid;
  const output = waitFor(socket, (frame) => frame.type === "stream.frame"
    && frame.stream_id === "control"
    && frame.frame?.event === "terminal.frame"
    && frame.frame.data?.text?.includes(marker), 10_000);
  const action = waitFor(socket, (frame) => frame.type === "response" && frame.id === "action");
  socket.send(JSON.stringify({
    type: "stream.action",
    stream_id: "control",
    id: "action",
    action: "submit_text",
    params: { text: "printf '" + marker + "\\n'" },
  }));
  assert.equal((await action).result.type, "terminal_backend_action");
  await output;

  // Exercise the interactive-input budget without overflowing the intentionally
  // tiny terminal observation queue on a slow CI runner. Each action must
  // receive its own successful response before the next is sent.
  const burstCount = 180;
  const burstStarted = performance.now();
  for (let index = 0; index < burstCount; index += 1) {
    const id = `burst-${index}`;
    const response = waitForCount(socket, (frame) => frame.type === "response"
      && frame.id === id, 1, 10_000);
    socket.send(JSON.stringify({
      type: "stream.action",
      stream_id: "control",
      id,
      action: "send_key",
      params: { key: index % 2 === 0 ? "left" : "right" },
    }));
    const [result] = await response;
    assert.equal(result.result?.type, "terminal_backend_action",
      `terminal input ${id} failed: ${JSON.stringify(result)}`);
  }
  const burstElapsed = performance.now() - burstStarted;
  const tabsClosed = Promise.all([closed(socket), closed(second.socket)]);
  socket.close();
  second.socket.close();
  await tabsClosed;
  const reopened = await authenticate(paired, { ticket: ready.ticket });
  const reopenedTab = await authenticate(paired, { ticket: ready.ticket });
  await request(reopened.socket, "reopened-snapshot", "session.snapshot", {});
  assert.equal((await request(reopened.socket, "reopened-devices", "web.devices.status", {})).paired_devices, 2);
  await assert.rejects(request(reopened.socket, "foreign-forget", "web.devices.forget", { ticket: phone.ready.ticket }), /method_not_found/);
  const forgottenTabs = Promise.all([closed(reopened.socket), closed(reopenedTab.socket)]);
  const forgotten = await request(reopened.socket, "forget", "web.devices.forget", {});
  assert.equal(forgotten.type, "browser_device_forgotten");
  await forgottenTabs;
  await rejected(paired, { ticket: ready.ticket });
  assert.equal((await request(phone.socket, "remaining-devices", "web.devices.status", {})).paired_devices, 1);
  await request(phone.socket, "still-live", "session.snapshot", {});

  const phoneClosed = closed(phone.socket);
  child.kill("SIGINT");
  await exited(child, 10_000);
  await phoneClosed;
  child = undefined;
  const status = run(["--session", session, "server", "status"], env);
  assert.equal(status.status, 0, status.stderr);
  assert.match(status.stdout, /running/);
  const finite = await startBridge(["--ticket-ttl", "2"]);
  await rejected(finite, { ticket: phone.ready.ticket }); // Stopping a bridge revoked all its tickets.
  const limited = await authenticate(finite, { code: finite.hash.slice(6) });
  assert.equal(typeof limited.ready.expires_at, "number");
  assert.equal(limited.ready.expires_on_close, undefined);
  await closed(limited.socket);
  await rejected(finite, { ticket: limited.ready.ticket });
  child.kill("SIGINT");
  await exited(child, 10_000);
  child = undefined;
  process.stdout.write(`native luvus web integration passed (multi-tab, reconnect, revoke, finite expiry, shutdown; ${burstCount} inputs in ${Math.round(burstElapsed)}ms)\n`);
} finally {
  for (const socket of sockets) socket.terminate();
  if (child?.exitCode === null) child.kill("SIGINT");
  run(["--session", session, "server", "stop"], env, true);
  run(["session", "delete", session], env, true);
  await rm(home, { recursive: true, force: true });
}

async function startBridge(extra = []) {
  stderr = "";
  child = spawn(executable, [
    "--session", session, "web", "--control", "--port", "0", "--no-open", ...extra,
  ], { cwd: workspace, env, stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-8192); });
  const lines = readline.createInterface({ input: child.stdout });
  return new URL(await waitForLine(lines, (line) => line.startsWith("http://"), 20_000));
}

async function authenticate(address, credential) {
  const socket = new WebSocket(address.origin.replace(/^http/, "ws") + "/bridge", { origin: address.origin });
  sockets.add(socket);
  socket.once("close", () => sockets.delete(socket));
  await opened(socket);
  const response = waitFor(socket, (frame) => frame.type === "ready" || frame.type === "error");
  socket.send(JSON.stringify({ type: "authenticate", ...credential }));
  return { socket, ready: await response };
}

async function rejected(address, credential) {
  const { socket, ready } = await authenticate(address, credential);
  assert.equal(ready.type, "error");
  assert.equal(ready.code, "forbidden");
  await closed(socket);
}

function closed(socket) {
  if (socket.readyState === WebSocket.CLOSED) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      socket.off("close", finish);
      reject(new Error("WebSocket did not close"));
    }, 5_000);
    const finish = () => { clearTimeout(timer); resolve(); };
    socket.once("close", finish);
  });
}

function run(args, runEnv, allowFailure = false) {
  const result = spawnSync(executable, args, {
    cwd: workspace,
    env: runEnv,
    encoding: "utf8",
    timeout: 20_000,
    windowsHide: true,
  });
  if (!allowFailure && (result.error || result.status !== 0)) {
    throw result.error || new Error(result.stderr || "luvus exited " + result.status);
  }
  return result;
}

function waitForLine(lines, predicate, timeoutMs) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => finish(new Error("native web startup timed out: " + stderr)), timeoutMs);
    const onLine = (line) => {
      if (predicate(line)) finish(undefined, line);
    };
    const onClose = () => finish(new Error("native web exited before startup: " + stderr));
    const finish = (error, value) => {
      clearTimeout(timer);
      lines.off("line", onLine);
      lines.off("close", onClose);
      if (error) reject(error); else resolve(value);
    };
    lines.on("line", onLine);
    lines.on("close", onClose);
  });
}

function opened(socket) {
  return new Promise((resolve, reject) => {
    socket.once("open", resolve);
    socket.once("error", reject);
  });
}

function request(socket, id, method, params) {
  const response = waitFor(socket, (frame) => frame.type === "response" && frame.id === id);
  socket.send(JSON.stringify({ type: "request", id, method, params }));
  return response.then((frame) => {
    if (frame.error) throw new Error(frame.error.code + ": " + frame.error.message);
    return frame.result;
  });
}

function waitFor(socket, predicate, timeoutMs = 5_000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => finish(new Error("WebSocket response timed out")), timeoutMs);
    const onMessage = (data) => {
      let frame;
      try { frame = JSON.parse(data.toString()); } catch { return; }
      if (predicate(frame)) finish(undefined, frame);
    };
    const onClose = () => finish(new Error("WebSocket closed"));
    const finish = (error, value) => {
      clearTimeout(timer);
      socket.off("message", onMessage);
      socket.off("close", onClose);
      if (error) reject(error); else resolve(value);
    };
    socket.on("message", onMessage);
    socket.on("close", onClose);
  });
}

function waitForCount(socket, predicate, count, timeoutMs) {
  return new Promise((resolve, reject) => {
    const frames = [];
    const timer = setTimeout(() => finish(new Error(`WebSocket response window timed out (${frames.length}/${count})`)), timeoutMs);
    const onMessage = (data) => {
      let frame;
      try { frame = JSON.parse(data.toString()); } catch { return; }
      if (frame.type === "stream.closed" && frame.stream_id === "control") {
        finish(new Error(`Terminal control stream closed during response window: ${JSON.stringify(frame)}`));
        return;
      }
      if (!predicate(frame)) return;
      frames.push(frame);
      if (frames.length === count) finish(undefined, frames);
    };
    const onClose = () => finish(new Error("WebSocket closed during response window"));
    const finish = (error, value) => {
      clearTimeout(timer);
      socket.off("message", onMessage);
      socket.off("close", onClose);
      if (error) reject(error); else resolve(value);
    };
    socket.on("message", onMessage);
    socket.on("close", onClose);
  });
}

function exited(process, timeoutMs) {
  return new Promise((resolve, reject) => {
    if (process.exitCode !== null) return resolve();
    const timer = setTimeout(() => reject(new Error("native web did not stop")), timeoutMs);
    process.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}
