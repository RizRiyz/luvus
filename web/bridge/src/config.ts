import path from "node:path";
import { fileURLToPath } from "node:url";

export interface BridgeConfig {
  host: "127.0.0.1";
  port: number;
  luvusBin: string;
  luvusHome?: string;
  session?: string;
  control: boolean;
  origins: ReadonlySet<string>;
  appDir: string;
  browserTicketSeconds: number;
  browserMaxDevices: number;
  publicUrl?: string;
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): BridgeConfig {
  const port = boundedInteger(env.LUVUS_WEB_PORT, 4174, 0, 65_535, "LUVUS_WEB_PORT");
  const browserTicketSeconds = boundedInteger(
    env.LUVUS_WEB_TICKET_TTL,
    12 * 60 * 60,
    60,
    24 * 60 * 60,
    "LUVUS_WEB_TICKET_TTL",
  );
  const browserMaxDevices = boundedInteger(env.LUVUS_WEB_MAX_DEVICES, 2, 1, 8, "LUVUS_WEB_MAX_DEVICES");
  const origins = new Set(
    (env.LUVUS_WEB_ORIGINS ?? "")
      .split(",")
      .map((value) => value.trim())
      .filter(Boolean)
      .map(normalizeOrigin),
  );
  const here = path.dirname(fileURLToPath(import.meta.url));
  return {
    host: "127.0.0.1",
    port,
    luvusBin: env.LUVUS_BIN || "luvus",
    ...(env.LUVUS_HOME ? { luvusHome: env.LUVUS_HOME } : {}),
    ...(env.LUVUS_SESSION ? { session: env.LUVUS_SESSION } : {}),
    control: env.LUVUS_WEB_CONTROL === "1",
    origins,
    appDir: env.LUVUS_WEB_APP_DIR || path.resolve(here, "../../packages/app/dist"),
    browserTicketSeconds,
    browserMaxDevices,
    ...(env.LUVUS_WEB_PUBLIC_URL ? { publicUrl: normalizePublicUrl(env.LUVUS_WEB_PUBLIC_URL) } : {}),
  };
}

export function originAllowed(origin: string | undefined, host: string | undefined, configured: ReadonlySet<string>): boolean {
  if (!origin || !host) return false;
  let parsed: URL;
  try {
    parsed = new URL(origin);
  } catch {
    return false;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return false;
  const normalized = parsed.origin.toLowerCase();
  return configured.has(normalized) || parsed.host.toLowerCase() === host.toLowerCase();
}

function normalizeOrigin(value: string): string {
  const parsed = new URL(value);
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error(`LUVUS_WEB_ORIGINS contains unsupported origin: ${value}`);
  }
  return parsed.origin.toLowerCase();
}

function normalizePublicUrl(value: string): string {
  const parsed = new URL(value);
  if ((parsed.protocol !== "http:" && parsed.protocol !== "https:") || parsed.username || parsed.password || parsed.search || parsed.hash) {
    throw new Error("LUVUS_WEB_PUBLIC_URL must be an HTTP(S) URL without credentials, query, or fragment");
  }
  parsed.pathname = parsed.pathname.replace(/\/+$/, "") || "/";
  return parsed.toString().replace(/\/$/, "");
}

function boundedInteger(raw: string | undefined, fallback: number, minimum: number, maximum: number, name: string): number {
  if (raw === undefined) return fallback;
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${name} must be an integer from ${minimum} through ${maximum}`);
  }
  return value;
}
