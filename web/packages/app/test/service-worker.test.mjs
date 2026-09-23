import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { runInNewContext } from "node:vm";

test("service worker clones a response before returning it to the browser", async () => {
  const handlers = new Map();
  let releaseCache;
  const cacheReady = new Promise((resolve) => { releaseCache = resolve; });
  let cached;
  const response = {
    bodyUsed: false,
    ok: true,
    clone() {
      assert.equal(this.bodyUsed, false);
      return { copy: true };
    },
  };
  const scope = {
    URL,
    self: {
      location: { origin: "http://127.0.0.1:4174" },
      addEventListener: (type, handler) => handlers.set(type, handler),
    },
    fetch: async () => response,
    caches: {
      open: async () => {
        await cacheReady;
        return { put: async (_request, copy) => { cached = copy; } };
      },
    },
  };
  runInNewContext(await readFile(new URL("../public/sw.js", import.meta.url), "utf8"), scope);
  let responsePromise;
  let cachePromise;
  handlers.get("fetch")({
    request: { method: "GET", url: "http://127.0.0.1:4174/app.js" },
    respondWith: (promise) => { responsePromise = promise; },
    waitUntil: (promise) => { cachePromise = promise; },
  });

  assert.equal(await responsePromise, response);
  response.bodyUsed = true;
  releaseCache();
  await cachePromise;
  assert.deepEqual(cached, { copy: true });
});
