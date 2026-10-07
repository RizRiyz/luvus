import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdir, mkdtemp, readFile, rm } from "node:fs/promises";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";
import { checkBrowserLayout } from "./check-browser-layout.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const executable = path.resolve(process.env.LUVUS_BIN || path.join(root, "target/debug", process.platform === "win32" ? "luvus.exe" : "luvus"));
const browserBin = process.env.LUVUS_BROWSER_BIN;
assert.ok(browserBin, "Set LUVUS_BROWSER_BIN to an installed Chromium headless binary");
await mkdir(path.join(root, "target"), { recursive: true });
const screenshots = process.env.LUVUS_WEB_SCREENSHOTS === "1" ? await mkdtemp(path.join(root, "target", "web-layout-qa-")) : undefined;
const home = await mkdtemp(path.join(root, "target", "web-browser-"));
const workspace = path.join(home, "workspace");
const profile = path.join(home, "browser");
await mkdir(workspace);
const session = `browser-test-${process.pid}`;
const env = { ...process.env, LUVUS_HOME: home };
delete env.LUVUS_SOCKET_PATH;
delete env.LUVUS_SESSION;
delete env.LUVUS_BIN_PATH;
// npm's harness prefix must not leak into the interactive test shell (nvm).
delete env.npm_config_prefix;
delete env.NPM_CONFIG_PREFIX;
let bridge, browser, cdp;
const exceptions = [];

try {
  bridge = spawn(executable, ["--session", session, "web", "--control", "--port", "0", "--no-open"], {
    cwd: workspace, env, stdio: ["ignore", "pipe", "ignore"], windowsHide: true,
  });
  const lines = readline.createInterface({ input: bridge.stdout });
  const pairing = new URL(await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("Bridge startup timed out")), 20_000);
    lines.on("line", (line) => { if (line.startsWith("http://")) { clearTimeout(timer); resolve(line); } });
    bridge.once("error", () => { clearTimeout(timer); reject(new Error("Could not start the isolated bridge")); });
    bridge.once("exit", () => { clearTimeout(timer); reject(new Error("Isolated bridge exited")); });
  }));
  await launchBrowser();
  const first = await page(pairing.toString());
  await ready(first);
  assert.equal(await evaluate(first.sessionId, "location.hash"), "");
  assert.equal(await evaluate(first.sessionId, "!!localStorage.getItem('luvus.web.ticket') && sessionStorage.getItem('luvus.web.ticket') === null"), true);
  await checkBrowserLayout({ cdp, sessionId: first.sessionId, evaluate, until, screenshots });
  if (screenshots) console.log(`Layout screenshots: ${screenshots}`);
  const second = await page(pairing.origin);
  await ready(second);
  await evaluate(second.sessionId, "document.querySelector('[aria-label=Devices]').click()");
  await until(() => evaluate(second.sessionId, "document.querySelector('.device-copy')?.textContent?.startsWith('1 authorized')"));
  const extraTabs = [];
  for (let tab = 0; tab < 6; tab += 1) {
    const extra = await page(pairing.origin);
    await ready(extra);
    extraTabs.push(extra);
  }
  assert.equal(await evaluate(second.sessionId, "document.querySelector('.device-copy')?.textContent?.startsWith('1 authorized')"), true, "eight tabs still count as one paired browser");
  for (const extra of extraTabs) await cdp.call("Target.closeTarget", { targetId: extra.targetId });
  await cdp.call("Target.closeTarget", { targetId: first.targetId });
  await cdp.call("Target.closeTarget", { targetId: second.targetId });
  const reopened = await page(pairing.origin);
  await ready(reopened);
  await closeBrowser();
  await launchBrowser();
  const restarted = await page(pairing.origin);
  await ready(restarted);
  const sibling = await page(pairing.origin);
  await ready(sibling);
  await foreground(restarted);
  await evaluate(restarted.sessionId, "document.querySelector('[aria-label=Devices]').click()");
  await until(() => evaluate(restarted.sessionId, "!!document.querySelector('.device-pair:not(:disabled)')"));
  await evaluate(restarted.sessionId, "document.querySelector('.device-pair').click()");
  await until(() => evaluate(restarted.sessionId, "!!document.querySelector('.pairing-link')?.value"));
  const phoneLink = await evaluate(restarted.sessionId, "document.querySelector('.pairing-link').value");
  const { browserContextId } = await cdp.call("Target.createBrowserContext");
  const phone = await page(phoneLink, browserContextId);
  await ready(phone);
  await evaluate(phone.sessionId, "document.querySelector('[aria-label=Devices]').click()");
  await until(() => evaluate(phone.sessionId, "document.querySelector('.device-copy')?.textContent?.startsWith('2 authorized')"));
  // Background tabs can receive broadcasts while Chrome pauses their animation
  // frames. Activate the tab before checking its newly painted device count.
  await foreground(restarted);
  await until(() => evaluate(restarted.sessionId, "document.querySelector('.device-copy')?.textContent?.startsWith('2 authorized') && !!document.querySelector('.device-disconnect:not(:disabled)')"));
  assert.equal(await evaluate(restarted.sessionId, "document.querySelector('.device-disconnect')?.textContent"), "Disconnect this browser");
  await cdp.call("Network.enable", {}, restarted.sessionId);
  let forgetRequests = 0;
  cdp.events.on("Network.webSocketFrameSent", (event) => {
    if (event.sessionId !== restarted.sessionId) return;
    try {
      if (JSON.parse(event.params.response.payloadData).method === "web.devices.forget") forgetRequests += 1;
    } catch { /* Non-JSON frames are not device requests. */ }
  });
  let acceptDisconnect = false;
  const disconnectDialogs = [];
  cdp.events.on("Page.javascriptDialogOpening", (event) => {
    if (event.sessionId !== restarted.sessionId) return;
    disconnectDialogs.push(event.params.message);
    void cdp.call("Page.handleJavaScriptDialog", { accept: acceptDisconnect }, event.sessionId);
  });
  await evaluate(restarted.sessionId, "document.querySelector('.device-disconnect').click()");
  assert.equal(disconnectDialogs.length, 1);
  assert.match(disconnectDialogs[0], /every tab.*new QR\/code to reconnect.*Other browsers and running terminals are not affected/);
  await ready(restarted);
  await ready(sibling);
  assert.equal(forgetRequests, 0, "canceling disconnect sends no revocation request");
  assert.equal(await evaluate(restarted.sessionId, "!!localStorage.getItem('luvus.web.ticket') && document.querySelector('.device-copy')?.textContent?.startsWith('2 authorized')"), true, "canceling disconnect preserves the pairing");
  acceptDisconnect = true;
  await evaluate(restarted.sessionId, "document.querySelector('.device-disconnect').click()");
  assert.equal(disconnectDialogs.length, 2);
  await until(() => evaluate(restarted.sessionId, "!!document.querySelector('.access-problem') && localStorage.getItem('luvus.web.ticket') === null"));
  // Revocation must clear shared credentials without activating the sibling.
  await until(() => evaluate(sibling.sessionId, "localStorage.getItem('luvus.web.ticket') === null"));
  await foreground(sibling);
  await until(() => evaluate(sibling.sessionId, "!!document.querySelector('.access-problem') && localStorage.getItem('luvus.web.ticket') === null"));
  assert.equal(forgetRequests, 1, "confirming disconnect revokes this browser once");
  await foreground(phone);
  await ready(phone);
  await until(() => evaluate(phone.sessionId, "document.querySelector('.device-copy')?.textContent?.startsWith('1 authorized')"));
  assert.equal(exceptions.length, 0, `Unexpected browser exceptions: ${exceptions.length}`);
  console.log("real Chromium browser passed: one QR/code pairing across eight tabs, close/reopen tabs, browser restart, separate profile, disconnect cancellation, and cross-tab revocation");
} finally {
  await closeBrowser().catch(() => browser?.kill("SIGKILL"));
  if (bridge?.exitCode === null) { bridge.kill("SIGINT"); await exited(bridge).catch(() => bridge.kill("SIGKILL")); }
  for (const args of [["--session", session, "server", "stop"], ["session", "delete", session]]) {
    spawnSync(executable, args, { cwd: workspace, env, stdio: "ignore", timeout: 20_000, windowsHide: true });
  }
  await rm(home, { recursive: true, force: true });
}

async function launchBrowser() {
  await rm(path.join(profile, "DevToolsActivePort"), { force: true });
  browser = spawn(browserBin, ["--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--disable-component-update", ...(process.platform === "linux" ? ["--no-sandbox"] : []), "about:blank"], { stdio: "ignore", windowsHide: true });
  let address;
  await until(async () => {
    try {
      const [port, endpoint] = (await readFile(path.join(profile, "DevToolsActivePort"), "utf8")).trim().split("\n");
      address = `ws://127.0.0.1:${port}${endpoint}`;
      return true;
    } catch { return false; }
  });
  const socket = new WebSocket(address);
  await new Promise((resolve, reject) => { socket.once("open", resolve); socket.once("error", reject); });
  cdp = new Cdp(socket);
  cdp.events.on("Runtime.exceptionThrown", () => exceptions.push(true));
}

async function closeBrowser() {
  if (!browser || browser.exitCode !== null) return;
  const done = exited(browser);
  await cdp.call("Browser.close").catch(() => {});
  await done;
  cdp.socket.close();
  browser = undefined;
}

async function page(url, browserContextId) {
  const { targetId } = await cdp.call("Target.createTarget", { url, ...(browserContextId ? { browserContextId } : {}) });
  const { sessionId } = await cdp.call("Target.attachToTarget", { targetId, flatten: true });
  await cdp.call("Page.enable", {}, sessionId);
  await cdp.call("Runtime.enable", {}, sessionId);
  const created = { targetId, sessionId };
  await foreground(created);
  return created;
}

async function foreground(page) {
  await cdp.call("Page.bringToFront", {}, page.sessionId);
  await until(() => evaluate(page.sessionId, "document.visibilityState === 'visible'"));
}

function ready(page) {
  return until(() => evaluate(page.sessionId, "!!document.querySelector('.mission-nav-status.ready') && !document.querySelector('.access-problem')"));
}

async function evaluate(sessionId, expression) {
  const result = await cdp.call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }, sessionId);
  if (result.exceptionDetails) throw new Error("Browser evaluation failed");
  return result.result.value;
}

async function until(check) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    try { if (await check()) return; } catch { /* A reload replaces execution contexts. */ }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error("Browser condition timed out");
}

function exited(child) {
  if (child.exitCode !== null) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("Test process did not exit")), 10_000);
    child.once("exit", () => { clearTimeout(timer); resolve(); });
  });
}

function Cdp(socket) {
    this.events = new EventEmitter();
    this.counter = 0;
    this.pending = new Map();
    this.socket = socket;
    socket.on("message", (data) => {
      const frame = JSON.parse(data.toString());
      if (frame.id) {
        const pending = this.pending.get(frame.id);
        if (!pending) return;
        this.pending.delete(frame.id);
        clearTimeout(pending.timer);
        if (frame.error) pending.reject(new Error(frame.error.message)); else pending.resolve(frame.result);
      } else this.events.emit(frame.method, frame);
    });
  this.call = (method, params = {}, sessionId) => {
    const id = ++this.counter;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 8_000);
      this.pending.set(id, { resolve, reject, timer });
      this.socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  };
}
