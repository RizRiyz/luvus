import assert from "node:assert/strict";
import test from "node:test";

import { RenderScheduler } from "../dist/test/render-scheduler.js";

function frames() {
  const queue = [];
  return {
    nextFrame: (callback) => queue.push(callback),
    run: () => { for (const callback of queue.splice(0)) callback(); },
    get pending() { return queue.length; },
  };
}

test("many live updates in one frame produce one redraw", () => {
  const frame = frames();
  let renders = 0;
  const scheduler = new RenderScheduler(() => { renders += 1; }, frame.nextFrame);
  for (let index = 0; index < 21; index += 1) scheduler.request();
  assert.equal(frame.pending, 1);
  frame.run();
  assert.equal(renders, 1);
  frame.run();
  assert.equal(renders, 1, "nothing left to draw");
});

test("a redraw waits while a pointer is pressed and runs after release", () => {
  const frame = frames();
  let renders = 0;
  const scheduler = new RenderScheduler(() => { renders += 1; }, frame.nextFrame);

  scheduler.request();
  scheduler.hold(); // pressed before the frame ran
  frame.run();
  scheduler.request();
  frame.run();
  assert.equal(renders, 0, "the pressed element stays in place");

  scheduler.release();
  frame.run();
  assert.equal(renders, 1, "one redraw once released");
});

test("a release that is never seen does not hold redraws forever", async () => {
  const frame = frames();
  let renders = 0;
  const scheduler = new RenderScheduler(() => { renders += 1; }, frame.nextFrame, 20);
  scheduler.hold();
  scheduler.request();
  frame.run();
  assert.equal(renders, 0);
  await new Promise((resolve) => setTimeout(resolve, 40));
  frame.run();
  assert.equal(renders, 1);
});
