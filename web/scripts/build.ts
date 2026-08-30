import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { gzipSync } from "node:zlib";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");
const repository = resolve(root, "..");
const dist = join(root, "dist");
const build = join(root, ".build");
const assets = join(dist, "assets");

await rm(dist, { recursive: true, force: true });
await rm(build, { recursive: true, force: true });
await mkdir(assets, { recursive: true });
await mkdir(build, { recursive: true });

await run(["go", "mod", "verify"], join(root, "wasm"));
await run(
  ["go", "build", "-trimpath", "-ldflags=-s -w -buildid=", "-o", join(build, "tailcat.wasm"), "."],
  join(root, "wasm"),
  { GOOS: "js", GOARCH: "wasm", GOTOOLCHAIN: "go1.26.5" },
);

const goroot = (await output(["go", "env", "GOROOT"], root, { GOTOOLCHAIN: "go1.26.5" })).trim();
const wasmExecCandidates = [
  join(goroot, "lib", "wasm", "wasm_exec.js"),
  join(goroot, "misc", "wasm", "wasm_exec.js"),
];
let wasmExec: Uint8Array | null = null;
for (const candidate of wasmExecCandidates) {
  try {
    wasmExec = await readFile(candidate);
    break;
  } catch {
    // Go moved wasm_exec.js between releases; accept only these known paths.
  }
}
if (!wasmExec) throw new Error("wasm_exec.js was not found in the selected Go toolchain");

const appBuild = await Bun.build({
  entrypoints: [join(root, "app", "src", "main.ts")],
  outdir: build,
  target: "browser",
  format: "esm",
  minify: true,
  sourcemap: "none",
});
if (!appBuild.success || appBuild.outputs.length !== 1) {
  throw new Error(`browser build failed: ${appBuild.logs.map(String).join("\n")}`);
}

const wasm = await readFile(join(build, "tailcat.wasm"));
const app = new Uint8Array(await appBuild.outputs[0].arrayBuffer());
const styles = await readFile(join(root, "app", "styles.css"));
const files = {
  wasm: await emit("tailcat", "wasm", wasm),
  app: await emit("app", "js", app),
  styles: await emit("styles", "css", styles),
  wasmExec: await emit("wasm_exec", "js", wasmExec),
};
const compressed = gzipSync(wasm, { level: 9 });
await writeFile(join(assets, `${files.wasm.name}.gz`), compressed);

const template = await readFile(join(root, "app", "index.html"), "utf8");
const html = template
  .replace("__STYLES__", `./assets/${files.styles.name}`)
  .replace("__WASM_EXEC__", `./assets/${files.wasmExec.name}`)
  .replace("__APP__", `./assets/${files.app.name}`);
if (html.includes("__")) throw new Error("index.html still contains a build placeholder");
await writeFile(join(dist, "index.html"), html);
await cp(join(root, "app", "_headers"), join(dist, "_headers"));
await cp(
  join(repository, "protocol", "uhp", "v1", "schema"),
  join(dist, "protocol", "uhp", "v1", "schema"),
  { recursive: true },
);

const manifest = {
  version: 1,
  tailcat: "v0.2.0",
  tailcatCommit: "a34089b378fea36d49ea2276d83b9237a32bb338",
  wasm: `assets/${files.wasm.name}`,
  wasmSha256: files.wasm.sha256,
  wasmBytes: wasm.byteLength,
  wasmGzip: `assets/${files.wasm.name}.gz`,
  wasmGzipSha256: sha256(compressed),
  wasmGzipBytes: compressed.byteLength,
  schemas: "protocol/uhp/v1/schema/",
};
await writeFile(join(dist, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);

if (compressed.byteLength > 8 * 1024 * 1024) {
  throw new Error(`compressed WASM is ${compressed.byteLength} bytes; the 8 MiB budget is exceeded`);
}

console.log(`Built ${dist}`);
console.log(`Tailcat WASM: ${(wasm.byteLength / 1024 / 1024).toFixed(2)} MiB raw, ${(compressed.byteLength / 1024 / 1024).toFixed(2)} MiB gzip`);

async function emit(stem: string, extension: string, bytes: Uint8Array) {
  const digest = sha256(bytes);
  const name = `${stem}-${digest.slice(0, 16)}.${extension}`;
  await writeFile(join(assets, name), bytes);
  return { name, sha256: digest };
}

function sha256(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

async function run(command: string[], cwd: string, extraEnv: Record<string, string> = {}) {
  const child = Bun.spawn(command, {
    cwd,
    env: { ...process.env, ...extraEnv },
    stdin: "ignore",
    stdout: "inherit",
    stderr: "inherit",
  });
  const status = await child.exited;
  if (status !== 0) throw new Error(`${command[0]} exited with status ${status}`);
}

async function output(command: string[], cwd: string, extraEnv: Record<string, string> = {}) {
  const child = Bun.spawn(command, {
    cwd,
    env: { ...process.env, ...extraEnv },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "inherit",
  });
  const text = await new Response(child.stdout).text();
  const status = await child.exited;
  if (status !== 0) throw new Error(`${command[0]} exited with status ${status}`);
  return text;
}
