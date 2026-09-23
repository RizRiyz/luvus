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

try {
  child = spawn(executable, [
    "--session", session, "web", "--control", "--port", "0", "--no-open",
  ], {
    cwd: workspace,
    env,
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-8192); });
  const lines = readline.createInterface({ input: child.stdout });
  const url = await waitForLine(lines, (line) => line.startsWith("http://"), 20_000);
  const paired = new URL(url);
  const code = paired.hash.startsWith("#pair=") ? decodeURIComponent(paired.hash.slice(6)) : "";
  assert.ok(code);

  const socket = new WebSocket(paired.origin.replace(/^http/, "ws") + "/bridge", {
    origin: paired.origin,
  });
  await opened(socket);
  socket.send(JSON.stringify({ type: "authenticate", code }));
  const ready = await waitFor(socket, (frame) => frame.type === "ready");
  assert.equal(ready.authority.mode, "control");

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

  // Interactive input has its own high-rate budget. This burst crosses both
  // former administrative limits (90 in the web bridge and 120 in UHP access)
  // while also exercising latest-frame coalescing under output pressure.
  const burstCount = 180;
  const burstStarted = performance.now();
  const burst = waitForCount(socket, (frame) => frame.type === "response"
    && typeof frame.id === "string"
    && frame.id.startsWith("burst-"), burstCount, 10_000);
  for (let index = 0; index < burstCount; index += 1) {
    socket.send(JSON.stringify({
      type: "stream.action",
      stream_id: "control",
      id: `burst-${index}`,
      action: "send_key",
      params: { key: index % 2 === 0 ? "left" : "right" },
    }));
  }
  const burstFrames = await burst;
  const burstElapsed = performance.now() - burstStarted;
  assert.ok(burstFrames.every((frame) => frame.result?.type === "terminal_backend_action"));
  assert.ok(burstElapsed < 5_000, `terminal input burst took ${Math.round(burstElapsed)}ms`);
  socket.close();

  child.kill("SIGINT");
  await exited(child, 10_000);
  child = undefined;
  const status = run(["--session", session, "server", "status"], env);
  assert.equal(status.status, 0, status.stderr);
  assert.match(status.stdout, /running/);
  process.stdout.write(`native luvus web integration passed (${burstCount} inputs in ${Math.round(burstElapsed)}ms)\n`);
} finally {
  if (child?.exitCode === null) child.kill("SIGINT");
  run(["--session", session, "server", "stop"], env, true);
  run(["session", "delete", session], env, true);
  await rm(home, { recursive: true, force: true });
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
    const timer = setTimeout(() => finish(new Error("WebSocket response burst timed out")), timeoutMs);
    const onMessage = (data) => {
      let frame;
      try { frame = JSON.parse(data.toString()); } catch { return; }
      if (!predicate(frame)) return;
      frames.push(frame);
      if (frames.length === count) finish(undefined, frames);
    };
    const onClose = () => finish(new Error("WebSocket closed during response burst"));
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
