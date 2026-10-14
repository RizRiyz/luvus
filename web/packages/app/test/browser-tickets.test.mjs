import assert from "node:assert/strict";
import test from "node:test";
import { BrowserTickets, TICKET_KEY } from "../dist/test/browser-tickets.js";

function storage() {
  const data = new Map();
  return {
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => { data.set(key, value); },
    removeItem: (key) => { data.delete(key); },
  };
}

test("one browser ticket works across tabs and browser restarts", () => {
  const persistent = storage();
  const first = new BrowserTickets(() => persistent, storage);
  const second = new BrowserTickets(() => persistent, storage);
  first.set("paired-browser");
  assert.equal(second.get(), "paired-browser");
  const reopened = new BrowserTickets(() => persistent, storage);
  assert.equal(reopened.get(), "paired-browser");
  reopened.clear("paired-browser");
  assert.equal(first.get(), null);
  assert.equal(second.get(), null);
});

test("an older per-tab ticket migrates without overwriting a newer browser ticket", () => {
  const persistent = storage();
  const legacy = storage();
  legacy.setItem(TICKET_KEY, "legacy-ticket");
  const migrated = new BrowserTickets(() => persistent, () => legacy);
  assert.equal(migrated.get(), "legacy-ticket");
  assert.equal(persistent.getItem(TICKET_KEY), "legacy-ticket");
  assert.equal(legacy.getItem(TICKET_KEY), null);
  legacy.setItem(TICKET_KEY, "stale-tab");
  persistent.setItem(TICKET_KEY, "fresh-ticket");
  assert.equal(new BrowserTickets(() => persistent, () => legacy).get(), "fresh-ticket");
});

test("a stale tab cannot delete a newly paired ticket", () => {
  const persistent = storage();
  const first = new BrowserTickets(() => persistent, storage);
  const second = new BrowserTickets(() => persistent, storage);
  first.set("old-ticket");
  second.set("new-ticket");
  first.clear("old-ticket");
  assert.equal(second.get(), "new-ticket");
});

test("blocked getters and storage methods do not break a live connection", () => {
  const blocked = () => { throw new Error("SecurityError"); };
  for (const persistent of [blocked, () => ({ getItem: blocked, setItem: blocked, removeItem: blocked })]) {
    const tickets = new BrowserTickets(persistent, blocked);
    assert.equal(tickets.get(), null);
    tickets.set("live-ticket");
    assert.equal(tickets.get(), "live-ticket");
    tickets.clear("live-ticket");
    assert.equal(tickets.get(), null);
  }
});

test("a failed persistent write uses the fresh ticket, not a stale stored value", () => {
  const persistent = storage();
  persistent.setItem(TICKET_KEY, "old-ticket");
  persistent.setItem = () => { throw new Error("QuotaExceededError"); };
  const tickets = new BrowserTickets(() => persistent, storage);
  tickets.set("fresh-ticket");
  assert.equal(tickets.get(), "fresh-ticket");
});

test("legacy storage preserves access when persistent storage is blocked", () => {
  const legacy = storage();
  const blocked = () => { throw new Error("SecurityError"); };
  const tickets = new BrowserTickets(blocked, () => legacy);
  tickets.set("tab-ticket");
  assert.equal(new BrowserTickets(blocked, () => legacy).get(), "tab-ticket");
});
