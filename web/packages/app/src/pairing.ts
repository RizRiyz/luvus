// Browser-side pairing decisions, kept pure so they can be tested without a
// DOM or a bridge.

/** What a tab presented when it tried to authenticate. */
export type SentCredential = { ticket: boolean; code: boolean };

/**
 * The credentials to send. A saved ticket is always sent when present, and a
 * pairing code alongside it when the tab has one. The bridge prefers a valid
 * ticket and does not spend a code sent with it, so reopening a pairing link
 * that was already used never costs a working tab its access.
 */
export function pairingCredential(ticket: string | null | undefined, code: string | undefined): { ticket?: string; code?: string } {
  return {
    ...(ticket ? { ticket } : {}),
    ...(code ? { code } : {}),
  };
}

/** Pairing codes are base64url tokens. Anything else is not a code. */
const CODE_PATTERN = /^[A-Za-z0-9_-]{16,128}$/;

/**
 * Read a pairing code from what the user pasted: a full pairing link, a bare
 * `#pair=` fragment, or the code itself. Only the code is used. The page never
 * navigates to a pasted address, so a pasted link cannot send this tab to
 * another site.
 */
export function parsePairingInput(input: string): string | undefined {
  const text = input.trim();
  if (!text) return undefined;
  const hash = text.indexOf("#");
  const candidate = hash >= 0
    ? new URLSearchParams(text.slice(hash + 1)).get("pair") ?? ""
    : text;
  return CODE_PATTERN.test(candidate) ? candidate : undefined;
}

/** Why this tab could not connect, in terms of what it actually sent. */
export function accessProblem(sent: SentCredential): { title: string; body: string } {
  const newLink = "Press Enter in the terminal running luvus web to print a new one-use link, "
    + "or create one from Devices on a browser that is still connected.";
  if (sent.ticket) {
    return {
      title: "This browser's access ended",
      body: `The web bridge restarted, this device was revoked, or its access expired. ${newLink}`,
    };
  }
  if (sent.code) {
    return {
      title: "This pairing link was already used",
      body: `Each link pairs one browser and expires after five minutes. Paired browsers can open more tabs without a new link. ${newLink}`,
    };
  }
  return {
    title: "This browser is not paired yet",
    body: `Pair this browser once to remember access across tabs and browser restarts. ${newLink}`,
  };
}
