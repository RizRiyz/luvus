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
      return {
        type: "session_snapshot",
        session: this.sessionName,
        server_generation: this.generation,
        event_sequence: this.sequence,
        workspaces: [],
      };
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

  emitEvent(event) {
    this.sequence += 1;
    this.onEvent({ event, sequence: this.sequence, data: {} });
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

test("agent title events refresh the snapshot without refreshing on terminal output", async () => {
  const { LiveSession } = await import("../dist/index.js");
  const bridge = new FakeBridge();
  const session = new LiveSession(bridge);
  await session.start();

  bridge.emitEvent("terminal.output_ready");
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(bridge.snapshotRequests, 1);

  bridge.emitEvent("agent.title_changed");
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(bridge.snapshotRequests, 2);
  session.stop();
});
