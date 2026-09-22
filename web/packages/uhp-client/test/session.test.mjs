import assert from "node:assert/strict";
import test from "node:test";

class FakeBridge extends EventTarget {
  generation = "a".repeat(32);
  sequence = 42;
  streamParams = [];

  async connect() {}

  async request(method) {
    if (method === "uhp.capabilities") {
      return {
        type: "uhp_capabilities",
        server_generation: this.generation,
        session: "test",
        event_sequence: this.sequence,
        methods: ["events.subscribe", "session.snapshot"],
      };
    }
    if (method === "session.snapshot") {
      return {
        type: "session_snapshot",
        session: "test",
        server_generation: this.generation,
        event_sequence: this.sequence,
        workspaces: [],
      };
    }
    throw new Error(`unexpected method: ${method}`);
  }

  async openStream(method, params) {
    assert.equal(method, "events.subscribe");
    this.streamParams.push(params);
    return { id: "events", action: async () => ({}), close() {} };
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
