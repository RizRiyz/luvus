import { BridgeError } from "@luvus/uhp-client";

const connectionErrors = new Set(["bridge_error", "closed", "disconnected", "stale_server", "stale_stream", "timeout", "unavailable"]);

export function recoverableConnectionError(error: unknown, retries = 4): boolean {
  if (!(error instanceof BridgeError)) return true;
  // A just-closed view may still hold its control lease upstream. Allow four
  // backoff attempts when opening, but not for ordinary input/action failures.
  if (error.code === "control_conflict") return retries < 4;
  return connectionErrors.has(error.code);
}
