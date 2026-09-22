import { createHash, randomBytes } from "node:crypto";

const PAIRING_SECONDS = 5 * 60;

export type BrowserPairing = {
  code: string;
  expiresAt: number;
};

export type BrowserDeviceStatus = {
  pairedDevices: number;
  pendingPairings: number;
  maxDevices: number;
};

export class BrowserAuthority {
  readonly initialPairing: BrowserPairing;
  #maxDevices: number;
  #pairings = new Map<string, number>();
  #tickets = new Map<string, number>();

  constructor(private readonly ticketSeconds: number, maxDevices: number) {
    this.#maxDevices = maxDevices;
    const initial = this.createPairing();
    if (!initial) throw new Error("browser device limit must allow initial pairing");
    this.initialPairing = initial;
  }

  authenticate(input: { code?: string; ticket?: string }): { accepted: boolean; ticket?: string; expiresAt?: number } {
    this.#purge();
    if (input.ticket) {
      const expiresAt = this.#tickets.get(digest(input.ticket));
      if (expiresAt && expiresAt > unixNow()) return { accepted: true, expiresAt };
    }
    if (!input.code) return { accepted: false };
    const pairingKey = digest(input.code);
    const pairingExpiresAt = this.#pairings.get(pairingKey);
    if (!pairingExpiresAt || pairingExpiresAt <= unixNow()) return { accepted: false };
    this.#pairings.delete(pairingKey);
    if (this.#tickets.size >= this.#maxDevices) return { accepted: false };
    const ticket = randomBytes(32).toString("base64url");
    const expiresAt = unixNow() + this.ticketSeconds;
    this.#tickets.set(digest(ticket), expiresAt);
    return { accepted: true, ticket, expiresAt };
  }

  createPairing(): BrowserPairing | undefined {
    this.#purge();
    if (this.#tickets.size + this.#pairings.size >= this.#maxDevices) return undefined;
    const code = randomBytes(24).toString("base64url");
    const expiresAt = unixNow() + PAIRING_SECONDS;
    this.#pairings.set(digest(code), expiresAt);
    return { code, expiresAt };
  }

  setMaxDevices(maxDevices: number): boolean {
    this.#purge();
    if (!Number.isSafeInteger(maxDevices) || maxDevices < this.#tickets.size + this.#pairings.size) return false;
    this.#maxDevices = maxDevices;
    return true;
  }

  status(): BrowserDeviceStatus {
    this.#purge();
    return {
      pairedDevices: this.#tickets.size,
      pendingPairings: this.#pairings.size,
      maxDevices: this.#maxDevices,
    };
  }

  revokeAll(): void {
    this.#pairings.clear();
    this.#tickets.clear();
  }

  #purge(): void {
    const now = unixNow();
    for (const [key, expiresAt] of this.#pairings) {
      if (expiresAt <= now) this.#pairings.delete(key);
    }
    for (const [key, expiresAt] of this.#tickets) {
      if (expiresAt <= now) this.#tickets.delete(key);
    }
  }
}

function digest(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

function unixNow(): number {
  return Math.floor(Date.now() / 1000);
}
