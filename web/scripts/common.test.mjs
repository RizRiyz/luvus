import assert from "node:assert/strict";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { settings } from "./common.mjs";

test("managed panes do not reuse their parent session or home", () => {
  const config = settings({
    LUVUS_SOCKET_PATH: "/owner/session.sock",
    LUVUS_PANE_ID: "17",
    LUVUS_SESSION: "owner",
    LUVUS_HOME: "/owner/home",
  });

  assert.equal(config.session, "web-dev");
  assert.equal(config.home, path.join(os.homedir(), ".luvus-dev"));
});

test("dedicated web selectors override inherited pane selectors", () => {
  const config = settings({
    LUVUS_SOCKET_PATH: "/owner/session.sock",
    LUVUS_SESSION: "owner",
    LUVUS_HOME: "/owner/home",
    LUVUS_WEB_SESSION: "browser-test",
    LUVUS_WEB_HOME: "/isolated/web-home",
  });

  assert.equal(config.session, "browser-test");
  assert.equal(config.home, path.resolve("/isolated/web-home"));
});

test("legacy selectors remain available outside a managed pane", () => {
  const config = settings({
    LUVUS_SESSION: "standalone-test",
    LUVUS_HOME: "/standalone/home",
  });

  assert.equal(config.session, "standalone-test");
  assert.equal(config.home, path.resolve("/standalone/home"));
});
