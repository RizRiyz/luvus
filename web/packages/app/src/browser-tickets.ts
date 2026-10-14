export const TICKET_KEY = "luvus.web.ticket";

type TicketStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** Origin-scoped browser access, with a fail-soft fallback for blocked storage. */
export class BrowserTickets {
  #fallback: string | null = null;
  #writeFailed = false;

  constructor(
    private readonly persistent: () => TicketStorage = () => localStorage,
    private readonly legacy: () => TicketStorage = () => sessionStorage,
  ) {
    // Preserve access for a tab paired by an older build, without replacing a
    // newer ticket another tab has already saved for this origin.
    const previous = this.#read(this.legacy);
    if (this.get() === null && previous) this.set(previous);
    else {
      try {
        if (this.persistent().getItem(TICKET_KEY) !== null) this.#remove(this.legacy);
      } catch { /* Retain a legacy ticket if persistent storage is unavailable. */ }
    }
  }

  get(): string | null {
    if (this.#writeFailed) return this.#fallback;
    try {
      const ticket = this.persistent().getItem(TICKET_KEY);
      this.#fallback = ticket;
      return ticket;
    } catch { /* A browser can block the storage getter as well as its methods. */ }
    this.#fallback = this.#read(this.legacy) ?? this.#fallback;
    return this.#fallback;
  }

  set(ticket: string): void {
    this.#fallback = ticket;
    try {
      this.persistent().setItem(TICKET_KEY, ticket);
      this.#writeFailed = false;
      this.#remove(this.legacy);
    } catch {
      this.#writeFailed = true;
      try { this.legacy().setItem(TICKET_KEY, ticket); } catch { /* Keep this tab connected in memory. */ }
    }
  }

  /** A stale tab must not erase a ticket freshly paired by another tab. */
  clear(expected: string | null): void {
    if (this.get() !== expected) return;
    this.#remove(this.persistent);
    this.#remove(this.legacy);
    this.#fallback = null;
    this.#writeFailed = false;
  }

  #read(storage: () => TicketStorage): string | null {
    try { return storage().getItem(TICKET_KEY); } catch { return null; }
  }

  #remove(storage: () => TicketStorage): void {
    try { storage().removeItem(TICKET_KEY); } catch { /* Server revocation still invalidates access. */ }
  }
}
