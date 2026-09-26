import assert from "node:assert/strict";
import test from "node:test";

class FakeBridge extends EventTarget {
  generation = "a".repeat(32);
  sequence = 42;
  sessionName = "test";
  streamParams = [];
  switches = [];
  switchTimeouts = [];
  snapshotRequests = 0;
  rateLimitSnapshots = false;
  snapshotGate;
  onEvent;

  async connect() {}

  async request(method, params = {}, timeoutMs) {
    if (method === "uhp.capabilities") {
      return {
        type: "uhp_capabilities",
        server_generation: this.generation,
        session: this.sessionName,
        event_sequence: this.sequence,
        methods: ["events.subscribe", "session.snapshot"],
      };
    }
    if (method === "session.snapshot") {
      this.snapshotRequests += 1;
      if (this.rateLimitSnapshots) {
        const { BridgeError } = await import("../dist/index.js");
        throw new BridgeError("private gateway request limit reached", "rate_limited");
      }
      const snapshot = {
        type: "session_snapshot",
        session: this.sessionName,
        server_generation: this.generation,
        event_sequence: this.sequence,
        workspaces: [{
          index: 0,
          name: "workspace",
          cwd: "/workspace",
          active: true,
          tabs: [{ index: 1, name: "tab", kind: "terminal", active: true, panes: [{
            pane_id: "1", kind: "terminal", focused: true, agent_session_title: "Old title",
          }] }],
        }],
      };
      if (this.snapshotGate) await this.snapshotGate;
      return snapshot;
    }
    if (method === "web.sessions.switch") {
      this.switches.push(params.name);
      this.switchTimeouts.push(timeoutMs);
      this.sessionName = params.name;
      this.generation = "c".repeat(32);
      this.sequence = 0;
      return {
        type: "browser_session_switch",
        session: { name: params.name, default: false, running: true },
      };
    }
    throw new Error(`unexpected method: ${method}`);
  }

  async openStream(method, params, onEvent) {
    assert.equal(method, "events.subscribe");
    this.streamParams.push(params);
    this.onEvent = onEvent;
    return { id: "events", action: async () => ({}), close() {} };
  }

  emitEvent(event, data = {}) {
    this.sequence += 1;
    this.onEvent({ event, sequence: this.sequence, data });
  }

  close() {}
}

test("restart clears an event cursor owned by the prior server generation", async () => {
  const { LiveSession } = await import("../dist/index.js");
  const bridge = new FakeBridge();
  const session = new LiveSession(bridge);

  await session.start();
  session.stop();
  bridge.generation = "b".repeat(32);
  bridge.sequence = 0;
  await session.start();

  assert.deepEqual(bridge.streamParams, [{}, {}]);
  session.stop();
});

test("same-generation reconnect resumes after the latest snapshot sequence", async () => {
  const { LiveSession } = await import("../dist/index.js");
  const bridge = new FakeBridge();
  const session = new LiveSession(bridge);

  await session.start();
  session.stop();
  await session.start();

  assert.deepEqual(bridge.streamParams, [{}, { after_sequence: 42 }]);
  session.stop();
});

test("session switching replaces the upstream generation and takes a fresh snapshot", async () => {
  const { LiveSession } = await import("../dist/index.js");
  const bridge = new FakeBridge();
  const session = new LiveSession(bridge);

  await session.start();
  await session.switchSession("review");

  assert.equal(session.state, "ready");
  assert.equal(session.snapshot.session, "review");
  assert.deepEqual(bridge.switches, ["review"]);
  assert.deepEqual(bridge.switchTimeouts, [120_000]);
  assert.deepEqual(bridge.streamParams, [{}, {}]);
  session.stop();
});

test("agent title events update the live snapshot without gateway requests", async () => {
  const { LiveSession } = await import("../dist/index.js");
  const bridge = new FakeBridge();
  const session = new LiveSession(bridge);
  let rendered = 0;
  session.addEventListener("snapshot", () => { rendered += 1; });
  await session.start();

  bridge.emitEvent("terminal.output_ready");
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(bridge.snapshotRequests, 1);

  for (let index = 0; index < 150; index += 1) {
    bridge.emitEvent("agent.title_changed", { pane: "1", title: `Title ${index}` });
  }
  assert.equal(session.snapshot.workspaces[0].tabs[0].panes[0].agent_session_title, "Title 149");
  assert.equal(bridge.snapshotRequests, 1);
  bridge.emitEvent("agent.title_changed", { pane: "1", title: null });
  assert.equal(session.snapshot.workspaces[0].tabs[0].panes[0].agent_session_title, null);
  assert.equal(bridge.snapshotRequests, 1);
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(rendered, 2);
  session.stop();
});

test("legacy title events still refresh, but rate limiting keeps the dashboard ready", async () => {
  const { LiveSession } = await import("../dist/index.js");
  const bridge = new FakeBridge();
  const session = new LiveSession(bridge);
  await session.start();

  bridge.rateLimitSnapshots = true;
  bridge.emitEvent("agent.title_changed");
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(bridge.snapshotRequests, 2);
  assert.equal(session.state, "ready");
  await session.refresh();
  assert.equal(bridge.snapshotRequests, 2);
  bridge.emitEvent("agent.title_changed");
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(bridge.snapshotRequests, 2);
  session.stop();
});

test("events arriving during a snapshot refresh are replayed onto its result", async () => {
  const { LiveSession } = await import("../dist/index.js");
  const bridge = new FakeBridge();
  const session = new LiveSession(bridge);
  await session.start();

  let release;
  bridge.snapshotGate = new Promise((resolve) => { release = resolve; });
  const refresh = session.refresh();
  bridge.emitEvent("agent.title_changed", { pane: "1", title: "Latest title" });
  release();
  await refresh;
  assert.equal(session.snapshot.workspaces[0].tabs[0].panes[0].agent_session_title, "Latest title");
  assert.equal(bridge.snapshotRequests, 2);
  session.stop();
});

test("session switching waits for an in-flight snapshot", async () => {
  const { LiveSession } = await import("../dist/index.js");
  const bridge = new FakeBridge();
  const session = new LiveSession(bridge);
  await session.start();

  let release;
  bridge.snapshotGate = new Promise((resolve) => { release = resolve; });
  const refresh = session.refresh();
  const switching = session.switchSession("review");
  assert.deepEqual(bridge.switches, []);
  release();
  await refresh;
  await switching;
  assert.equal(session.snapshot.session, "review");
  session.stop();
});
