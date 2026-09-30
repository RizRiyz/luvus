import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { runInNewContext } from "node:vm";

test("the service worker only removes its own caches and unregisters itself", async () => {
  const handlers = new Map();
  const deleted = [];
  let unregistered = false;
  const scope = {
    self: {
      addEventListener: (type, handler) => handlers.set(type, handler),
      skipWaiting: () => {},
      registration: { unregister: async () => { unregistered = true; return true; } },
    },
    caches: {
      keys: async () => ["luvus-web-v2", "luvus-web-v1", "someone-else"],
      delete: async (key) => { deleted.push(key); return true; },
    },
  };
  runInNewContext(await readFile(new URL("../public/sw.js", import.meta.url), "utf8"), scope);

  assert.equal(handlers.has("fetch"), false, "requests always go to the network");
  let activation;
  handlers.get("activate")({ waitUntil: (promise) => { activation = promise; } });
  await activation;
  assert.deepEqual(deleted.sort(), ["luvus-web-v1", "luvus-web-v2"]);
  assert.equal(unregistered, true);
});
