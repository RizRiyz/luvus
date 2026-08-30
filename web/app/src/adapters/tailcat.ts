import type { ByteTransportClient } from "../transport";

const MANIFEST_TIMEOUT_MS = 10_000;
const WASM_TIMEOUT_MS = 120_000;
const textDecoder = new TextDecoder("utf-8", { fatal: true });

interface BuildManifest {
  wasm: string;
  wasmSha256: string;
  tailcat: string;
}

declare global {
  interface Window {
    Go: new () => {
      importObject: WebAssembly.Imports;
      run(instance: WebAssembly.Instance): Promise<void>;
    };
    luvusTailcatConnect(options: { address: string }): Promise<ByteTransportClient>;
    onLuvusTailcatReady?: () => void;
  }
}

let wasmReady: Promise<void> | null = null;

/** Tailcat/WASM reference adapter. UHP client code does not depend on it. */
export async function connectTailcat(
  address: string,
  report: (message: string) => void,
): Promise<ByteTransportClient> {
  await loadWasm(report);
  report("Connecting to the encrypted relay…");
  return window.luvusTailcatConnect({ address });
}

async function loadWasm(report: (message: string) => void): Promise<void> {
  if (wasmReady) return wasmReady;
  wasmReady = (async () => {
    report("Loading the web client manifest…");
    const manifestBytes = await fetchBytes(
      "./manifest.json",
      "no-store",
      MANIFEST_TIMEOUT_MS,
      "Web client manifest timed out",
    );
    const manifest = JSON.parse(textDecoder.decode(manifestBytes)) as BuildManifest;
    if (!/^assets\/tailcat-[a-f0-9]{16}\.wasm$/.test(manifest.wasm)) {
      throw new Error("Web client manifest is invalid");
    }
    report("Downloading the encrypted transport…");
    const bytes = await fetchBytes(
      `./${manifest.wasm}`,
      "force-cache",
      WASM_TIMEOUT_MS,
      "Tailcat WebAssembly download timed out",
    );
    report("Verifying the encrypted transport…");
    const digest = await crypto.subtle.digest("SHA-256", bytes);
    const actual = [...new Uint8Array(digest)]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("");
    if (actual !== manifest.wasmSha256) throw new Error("Tailcat WebAssembly checksum failed");
    if (typeof window.Go !== "function") {
      throw new Error("Go WebAssembly runtime failed to load");
    }
    report("Starting the encrypted transport…");
    const go = new window.Go();
    const result = await WebAssembly.instantiate(bytes, go.importObject);
    void go.run(result.instance);
    await new Promise<void>((resolve, reject) => {
      const timeout = window.setTimeout(
        () => reject(new Error("Tailcat WebAssembly startup timed out")),
        10_000,
      );
      if (typeof window.luvusTailcatConnect === "function") {
        window.clearTimeout(timeout);
        resolve();
        return;
      }
      window.onLuvusTailcatReady = () => {
        window.clearTimeout(timeout);
        delete window.onLuvusTailcatReady;
        resolve();
      };
    });
  })().catch((error) => {
    wasmReady = null;
    throw error;
  });
  return wasmReady;
}

async function fetchBytes(
  url: string,
  cache: RequestCache,
  timeoutMs: number,
  timeoutMessage: string,
): Promise<ArrayBuffer> {
  const controller = new AbortController();
  const timeout = window.setTimeout(() => controller.abort(timeoutMessage), timeoutMs);
  try {
    const response = await fetch(url, {
      cache,
      credentials: "omit",
      signal: controller.signal,
    });
    if (!response.ok) throw new Error(`${url} returned HTTP ${response.status}`);
    return await response.arrayBuffer();
  } catch (error) {
    if (controller.signal.aborted) throw new Error(timeoutMessage);
    throw error;
  } finally {
    window.clearTimeout(timeout);
  }
}
