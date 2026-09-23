import assert from "node:assert/strict";
import test from "node:test";

test("package exports are built", async () => {
  const module = await import("../dist/index.js");
  assert.equal(typeof module.BridgeClient, "function");
  assert.equal(typeof module.LiveSession, "function");
});

test("closing a stream rejects pending and future actions immediately", async () => {
  class FakeWebSocket extends EventTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    static instances = [];

    readyState = FakeWebSocket.CONNECTING;

    constructor() {
      super();
      FakeWebSocket.instances.push(this);
      queueMicrotask(() => {
        this.readyState = FakeWebSocket.OPEN;
        this.dispatchEvent(new Event("open"));
      });
    }

    send(raw) {
      const frame = JSON.parse(raw);
      if (frame.type === "authenticate") {
        this.server({
          type: "ready",
          expires_at: Math.floor(Date.now() / 1_000) + 60,
          authority: { mode: "control", scopes: [] },
        });
      } else if (frame.type === "stream.open") {
        this.server({ type: "response", id: frame.id, result: { type: "terminal_backend_stream" } });
      }
    }

    close() {
      this.readyState = FakeWebSocket.CLOSED;
    }

    server(frame) {
      const event = new Event("message");
      Object.defineProperty(event, "data", { value: JSON.stringify(frame) });
      this.dispatchEvent(event);
    }
  }

  const previous = globalThis.WebSocket;
  globalThis.WebSocket = FakeWebSocket;
  try {
    const { BridgeClient } = await import("../dist/index.js");
    const bridge = new BridgeClient("ws://bridge.test", () => ({ ticket: "ticket" }), () => {});
    await bridge.connect();
    const stream = await bridge.openStream("terminal.backend.control", {}, () => {}, () => {});
    const pending = stream.action("send_key", { key: "enter" });
    FakeWebSocket.instances[0].server({
      type: "stream.closed",
      stream_id: stream.id,
      reason: "upstream closed",
    });

    await assert.rejects(pending, (error) => error.code === "stale_stream");
    await assert.rejects(
      stream.action("send_key", { key: "enter" }),
      (error) => error.code === "stale_stream",
    );

    const closingStream = await bridge.openStream("terminal.backend.control", {}, () => {}, () => {});
    const closingAction = closingStream.action("send_key", { key: "enter" });
    FakeWebSocket.instances[0].readyState = FakeWebSocket.CLOSING;
    assert.doesNotThrow(() => closingStream.close());
    await assert.rejects(closingAction, (error) => error.code === "stale_stream");
    bridge.close();
  } finally {
    globalThis.WebSocket = previous;
  }
});
