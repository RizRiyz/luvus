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
  scheduler.hold(1); // pressed before the frame ran
  assert.equal(scheduler.holding, true);
  frame.run();
  scheduler.request();
  frame.run();
  assert.equal(renders, 0, "the pressed element stays in place");

  scheduler.release(1);
  assert.equal(scheduler.holding, false);
  frame.run();
  assert.equal(renders, 1, "one redraw once released");
});

test("a long press keeps holding until that pointer is released", async () => {
  const frame = frames();
  let renders = 0;
  const scheduler = new RenderScheduler(() => { renders += 1; }, frame.nextFrame, 10_000);
  scheduler.hold(1);
  scheduler.request();
  await new Promise((resolve) => setTimeout(resolve, 1_200));
  frame.run();
  assert.equal(renders, 0, "still pressed after more than a second");
  scheduler.release(1);
  frame.run();
  assert.equal(renders, 1);
});

test("with several pointers pressed, one release does not end the hold", () => {
  const frame = frames();
  let renders = 0;
  const scheduler = new RenderScheduler(() => { renders += 1; }, frame.nextFrame);
  scheduler.hold(1);
  scheduler.hold(2);
  scheduler.request();
  scheduler.release(1);
  frame.run();
  assert.equal(renders, 0, "the second finger is still down");
  scheduler.release(7); // an unknown pointer changes nothing
  frame.run();
  assert.equal(renders, 0);
  scheduler.release(2);
  frame.run();
  assert.equal(renders, 1);
});

test("releases the page never saw still let redraws resume", async () => {
  const frame = frames();
  let renders = 0;
  const scheduler = new RenderScheduler(() => { renders += 1; }, frame.nextFrame, 20);
  scheduler.hold(1);
  scheduler.request();
  scheduler.releaseAll(); // window blur or a hidden tab
  frame.run();
  assert.equal(renders, 1);

  scheduler.hold(2);
  scheduler.request();
  await new Promise((resolve) => setTimeout(resolve, 40)); // last-resort timeout
  frame.run();
  assert.equal(renders, 2);
});
