import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { browserAddress, killBrowser, prepareBrowserProfile, watchBrowser } from "./browser-process.mjs";

const target = new URL("../../target/", import.meta.url);
await mkdir(target, { recursive: true });

async function fixture(t, source) {
  const profile = await mkdtemp(new URL("browser-process-test-", target));
  const child = spawn(process.execPath, ["-e", source], { stdio: ["ignore", "ignore", "pipe"] });
  const watch = watchBrowser(child);
  t.after(async () => {
    await killBrowser(child, watch);
    await rm(profile, { recursive: true, force: true });
  });
  return { profile, child, watch };
}

test("browser startup reports early exit and stderr instead of a generic timeout", async (t) => {
  const { profile, watch } = await fixture(t, "process.stderr.write('missing browser dependency\\n'); process.exitCode = 23");
  await assert.rejects(browserAddress(watch, profile), /Chromium exited \(code=23, signal=null\)[\s\S]*missing browser dependency/);
});

test("browser startup handles a missing executable without an unhandled error", async () => {
  const child = spawn(path.resolve("target", "missing-browser-executable"), [], { stdio: ["ignore", "ignore", "pipe"] });
  const watch = watchBrowser(child);
  await assert.rejects(browserAddress(watch, path.resolve("target")), /Chromium spawn failed:.*ENOENT/);
  assert.equal(watch.finished, true);
});

test("browser startup diagnostics are bounded", async (t) => {
  const { profile, watch } = await fixture(t, "process.stderr.write('x'.repeat(32 * 1024) + 'END', () => process.exit(1))");
  await assert.rejects(browserAddress(watch, profile), (error) => {
    assert.match(error.message, /END$/);
    assert.ok(error.message.length < 17 * 1024);
    return true;
  });
});

test("startup timeout retains stderr and a browser without CDP can be reaped", async (t) => {
  const { profile, child, watch } = await fixture(t, "process.stderr.write('initializing'); setInterval(() => {}, 1000)");
  await assert.rejects(browserAddress(watch, profile, 500), /startup timed out after 500ms[\s\S]*initializing/);
  await killBrowser(child, watch);
  assert.equal(watch.finished, true);
  assert.equal(child.signalCode, "SIGKILL");
});

test("restart removes only the stale endpoint and waits for a complete new one", async (t) => {
  const { profile, watch } = await fixture(t, "setInterval(() => {}, 1000)");
  const endpointFile = path.join(profile, "DevToolsActivePort");
  const ticketFile = path.join(profile, "ticket-fixture");
  await writeFile(ticketFile, "persistent pairing");
  await writeFile(endpointFile, "1234\n/devtools/browser/stale");
  await prepareBrowserProfile(profile);
  await assert.rejects(readFile(endpointFile), { code: "ENOENT" });
  await writeFile(endpointFile, "4567\n");
  await assert.rejects(browserAddress(watch, profile, 100), /startup timed out/);
  await writeFile(endpointFile, "4567\n/devtools/browser/fresh");
  assert.equal(await browserAddress(watch, profile), "ws://127.0.0.1:4567/devtools/browser/fresh");
  assert.equal(await readFile(ticketFile, "utf8"), "persistent pairing");
});
