export interface Credentials {
  token: string;
  expiresAt: number;
}

let credentials: Credentials | null = null;

export function rememberCredentials(next: Credentials): void {
  forgetCredentials();
  credentials = next;
}

export function currentCredentials(): Credentials {
  if (!credentials || Date.now() >= credentials.expiresAt * 1000) {
    forgetCredentials();
    throw new Error("Web access expired. Start `luvus web` again.");
  }
  return credentials;
}

export function forgetCredentials(): void {
  if (credentials) credentials.token = "";
  credentials = null;
}

export function safeError(error: unknown): string {
  const message = error instanceof Error ? error.message : "Connection failed";
  return message
    .replace(/tc[A-Za-z0-9_-]{16,}/g, "[address]")
    .replace(/luv_tok_[a-f0-9]+/g, "[credential]")
    .slice(0, 240);
}
