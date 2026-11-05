import { readFile, rm } from "node:fs/promises";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";

const stderrLimit = 16 * 1024;

// Attach immediately after spawn: a failed executable must not turn into a
// generic polling timeout, and Chromium's stderr is the only startup evidence
// available on a disposable CI runner. Keep that evidence bounded.
export function watchBrowser(child) {
  let stderr = "";
  let failure;
  let finished = false;
  const done = new Promise((resolve) => {
    child.once("error", (error) => {
      failure = `spawn failed: ${error.message}`;
      finished = true;
      resolve();
    });
    // `close` follows stderr drainage; `exit` can precede its final data.
    child.once("close", (code, signal) => {
      failure = `exited (code=${code}, signal=${signal})`;
      finished = true;
      resolve();
    });
  });
  child.stderr?.on("data", (chunk) => {
    stderr = (stderr + chunk.toString()).slice(-stderrLimit);
  });
  return {
    done,
    get finished() { return finished; },
    error(reason) {
      return new Error(`Chromium ${failure || reason}\nstderr (last ${stderrLimit} characters):\n${stderr || "<empty>"}`);
    },
  };
}

export async function prepareBrowserProfile(profile) {
  // A browser restart deliberately reuses its tickets, but never its old CDP
  // endpoint. Only remove the endpoint after the previous process has exited.
  await rm(path.join(profile, "DevToolsActivePort"), { force: true });
}

export async function browserAddress(watch, profile, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (watch.finished) throw watch.error("startup failed");
    try {
      const [port, endpoint] = (await readFile(path.join(profile, "DevToolsActivePort"), "utf8")).trim().split("\n");
      if (/^\d+$/.test(port) && Number(port) > 0 && Number(port) <= 65535 && /^\/devtools\/browser\/[^\s]+$/.test(endpoint)) {
        if (watch.finished) throw watch.error("startup failed");
        return `ws://127.0.0.1:${port}${endpoint}`;
      }
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    await Promise.race([delay(50), watch.done]);
  }
  throw watch.error(`startup timed out after ${timeoutMs}ms waiting for DevToolsActivePort`);
}

export async function killBrowser(child, watch) {
  if (watch.finished) return;
  child.kill("SIGKILL");
  let timer;
  try {
    await Promise.race([
      watch.done,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(watch.error("did not exit after SIGKILL")), 10_000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
