import { spawn } from "node:child_process";
import {
  buildAll,
  detachInput,
  isolatedEnv,
  repoRoot,
  run,
  settings,
  stopServer,
  webRoot,
  writeError,
  writeOutput,
} from "./common.mjs";

const args = new Set(process.argv.slice(2));
if (args.has("--help")) {
  writeOutput(`Usage: npm run dev -- [--read-only] [--no-open] [--skip-build]\n\nEnvironment:\n  LUVUS_WEB_SESSION      isolated session name (default: web-dev)\n  LUVUS_WEB_HOME         debug state directory (default: ~/.luvus-dev)\n  LUVUS_WEB_PORT         loopback port (default: 4174)\n  LUVUS_WEB_MAX_DEVICES  authorized browser limit, 1-8 (default: 2)\n  LUVUS_WEB_PUBLIC_URL   public HTTPS origin used in pairing links\n  LUVUS_BIN              exact existing binary; skips the Rust build\n`);
  process.exit(0);
}
for (const arg of args) {
  if (!["--read-only", "--no-open", "--skip-build"].includes(arg)) {
    throw new Error(`Unknown option: ${arg}`);
  }
}

detachInput();

const config = settings();
const readOnly = args.has("--read-only");
buildAll(config, args.has("--skip-build"));

writeOutput(`\nStarting isolated Luvus session '${config.session}'...\n`);
const serverEnv = isolatedEnv(config);
run(config.binary, ["--session", config.session, "server", "restart"], {
  cwd: repoRoot,
  env: serverEnv,
  timeout: 30_000,
});

// npm and terminal emulators do not deliver Ctrl+C identically. A detached,
// no-output guardian stops this exact isolated session if the wrapper vanishes
// before its signal handler can run.
const guardian = spawn(process.execPath, [
  "scripts/cleanup-watch.mjs",
  String(process.pid),
  config.binary,
  config.home,
  config.session,
], {
  cwd: webRoot,
  detached: true,
  stdio: "ignore",
  windowsHide: true,
});
guardian.unref();
guardian.on("error", (error) => {
  writeError(`Warning: cleanup guardian could not start: ${error.message}\n`);
});

const bridgeEnv = isolatedEnv(config, {
  LUVUS_BIN: config.binary,
  LUVUS_SESSION: config.session,
  LUVUS_WEB_PORT: config.port,
  ...(readOnly ? {} : { LUVUS_WEB_CONTROL: "1" }),
});
// The bridge deliberately receives the selected session after inherited
// selectors were removed by isolatedEnv.
bridgeEnv.LUVUS_SESSION = config.session;

writeOutput(`\nStarting ${readOnly ? "read-only" : "interactive"} web bridge...\n`);
const bridge = spawn(process.execPath, ["bridge/dist/index.js"], {
  cwd: webRoot,
  env: bridgeEnv,
  // The bridge is controlled over HTTP/WebSocket and never reads terminal
  // input. Owning the pane's stdin lets an interrupted npm process leave the
  // shell outside its line editor, where arrow escapes are echoed literally.
  // Keep every bridge descriptor off the controlling terminal. npm may return
  // to the shell before this child and its UHP process finish shutting down;
  // a late Node TTY wrapper would otherwise restore canonical mode over zsh.
  stdio: ["ignore", "pipe", "pipe"],
});

let output = "";
let opened = false;
bridge.stdout.setEncoding("utf8");
bridge.stdout.on("data", (chunk) => {
  writeOutput(chunk);
  if (opened || args.has("--no-open") || process.env.LUVUS_WEB_NO_OPEN === "1") return;
  output += chunk;
  const newline = output.indexOf("\n");
  if (newline < 0) return;
  try {
    const descriptor = JSON.parse(output.slice(0, newline));
    if (descriptor.type === "luvus_web_bridge" && typeof descriptor.url === "string") {
      opened = true;
      openBrowser(descriptor.url);
    }
  } catch {
    // The pairing URL remains printed for manual opening.
  }
});
bridge.stderr.setEncoding("utf8");
bridge.stderr.on("data", (chunk) => writeError(chunk));

let stopping = false;
function cleanup(code) {
  if (stopping) return;
  stopping = true;
  if (!bridge.killed) bridge.kill("SIGTERM");
  writeOutput(`\nStopping isolated Luvus session '${config.session}'...\n`);
  stopServer(config);
  guardian.kill("SIGTERM");
  process.exit(code);
}

// Under `npm run`, npm can return control to the shell before a signal handler
// that performs synchronous cleanup has finished. That late foreground child
// then restores the PTY's canonical mode after zsh has already entered its line
// editor, making arrows appear as literal `^[OA`/`^[[A`. Exit promptly and let
// the detached guardian stop only this isolated server after our PID vanishes.
function handOffSignal(code) {
  if (stopping) return;
  stopping = true;
  if (!bridge.killed) bridge.kill("SIGTERM");
  process.exit(code);
}

process.once("SIGINT", () => handOffSignal(130));
process.once("SIGTERM", () => handOffSignal(143));
bridge.once("error", (error) => {
  writeError(`Could not start web bridge: ${error.message}\n`);
  cleanup(1);
});
bridge.once("exit", (code, signal) => {
  if (!stopping) cleanup(signal ? 1 : code || 0);
});

function openBrowser(url) {
  let command;
  let commandArgs;
  if (process.platform === "darwin") {
    command = "open";
    commandArgs = [url];
  } else if (process.platform === "win32") {
    command = "cmd.exe";
    commandArgs = ["/c", "start", "", url];
  } else {
    command = "xdg-open";
    commandArgs = [url];
  }
  const opener = spawn(command, commandArgs, { stdio: "ignore", detached: true });
  opener.on("error", () => {});
  opener.unref();
}
