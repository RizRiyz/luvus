import { createHash, randomBytes, timingSafeEqual } from "node:crypto";

const PAIRING_SECONDS = 5 * 60;

export class BrowserAuthority {
  readonly pairingCode = randomBytes(24).toString("base64url");
  readonly pairingExpiresAt = unixNow() + PAIRING_SECONDS;
  #pairingSpent = false;
  #tickets = new Map<string, number>();

  constructor(private readonly ticketSeconds: number) {}

  authenticate(input: { code?: string; ticket?: string }): { accepted: boolean; ticket?: string; expiresAt?: number } {
    this.#purge();
    if (input.ticket) {
      const key = digest(input.ticket);
      const expiresAt = this.#tickets.get(key);
      if (expiresAt && expiresAt > unixNow()) return { accepted: true, expiresAt };
    }
    if (!input.code || this.#pairingSpent || unixNow() > this.pairingExpiresAt) return { accepted: false };
    if (!constantTimeTextEqual(input.code, this.pairingCode)) return { accepted: false };
    this.#pairingSpent = true;
    const ticket = randomBytes(32).toString("base64url");
    const expiresAt = unixNow() + this.ticketSeconds;
    this.#tickets.set(digest(ticket), expiresAt);
    return { accepted: true, ticket, expiresAt };
  }

  revokeAll(): void {
    this.#tickets.clear();
  }

  #purge(): void {
    const now = unixNow();
    for (const [key, expiresAt] of this.#tickets) {
      if (expiresAt <= now) this.#tickets.delete(key);
    }
  }
}

function digest(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

function constantTimeTextEqual(left: string, right: string): boolean {
  const a = Buffer.from(left);
  const b = Buffer.from(right);
  if (a.length !== b.length) {
    timingSafeEqual(a, a);
    return false;
  }
  return timingSafeEqual(a, b);
}

function unixNow(): number {
  return Math.floor(Date.now() / 1000);
}
