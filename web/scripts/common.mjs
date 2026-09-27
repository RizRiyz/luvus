import { closeSync, existsSync, openSync, writeSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const scriptsDir = path.dirname(fileURLToPath(import.meta.url));
export const webRoot = path.resolve(scriptsDir, "..");
export const repoRoot = path.resolve(webRoot, "..");
export const npmCommand = process.platform === "win32" ? "npm.cmd" : "npm";
export const cargoCommand = process.platform === "win32" ? "cargo.exe" : "cargo";

// Long-running npm scripts can outlive npm briefly while handling Ctrl+C.
// Writing through Node's lazy TTY wrappers makes that late child restore the
// canonical mode it inherited after the shell has already re-entered its line
// editor. Raw descriptor writes preserve output without claiming TTY mode
// ownership. File descriptors 1 and 2 are portable Node stdout/stderr handles.
export function writeOutput(text) {
  writeSync(1, text);
}

export function writeError(text) {
  writeSync(2, text);
}

export function detachInput() {
  // The web launcher is controlled by signals and HTTP, never terminal input.
  // Replacing only its inherited stdin prevents a late Node/libuv exit from
  // restoring npm's canonical PTY state over the shell's active line editor.
  try {
    closeSync(0);
  } catch (error) {
    if (error?.code !== "EBADF") throw error;
  }
  const descriptor = openSync(os.devNull, "r");
  if (descriptor !== 0) {
    closeSync(descriptor);
    throw new Error("Could not isolate web launcher input");
  }
}

export function settings(env = process.env) {
  const managedPane = Boolean(env.LUVUS_SOCKET_PATH || env.LUVUS_PANE_ID);
  // A pane inherits the server selector that owns it. Reusing that selector
  // here would make web development restart and later stop its own parent
  // session. Dedicated web overrides remain explicit, while the legacy names
  // still work when the launcher is run from an ordinary terminal.
  const selectedHome = env.LUVUS_WEB_HOME || (!managedPane ? env.LUVUS_HOME : undefined);
  const selectedSession = env.LUVUS_WEB_SESSION || (!managedPane ? env.LUVUS_SESSION : undefined);
  const defaultBinary = path.join(repoRoot, "target", "debug", process.platform === "win32" ? "luvus.exe" : "luvus");
  return {
    binary: path.resolve(env.LUVUS_BIN || defaultBinary),
    customBinary: Boolean(env.LUVUS_BIN),
    home: path.resolve(selectedHome || path.join(os.homedir(), ".luvus-dev")),
    session: selectedSession || "web-dev",
    port: env.LUVUS_WEB_PORT || "4174",
  };
}

export function isolatedEnv(config, additions = {}) {
  const env = { ...process.env, ...additions, LUVUS_HOME: config.home };
  delete env.LUVUS_SOCKET_PATH;
  delete env.LUVUS_SESSION;
  return env;
}

export function run(executable, args, options = {}) {
  const result = spawnSync(executable, args, {
    cwd: options.cwd || repoRoot,
    env: options.env || process.env,
    stdio: options.quiet ? "pipe" : "inherit",
    encoding: options.quiet ? "utf8" : undefined,
    timeout: options.timeout || 10 * 60 * 1000,
  });
  if (result.error) throw result.error;
  if (result.status !== 0 && !options.allowFailure) {
    const detail = options.quiet ? String(result.stderr || result.stdout || "").trim() : "";
    throw new Error(detail || `${path.basename(executable)} ${args.join(" ")} failed with status ${result.status}`);
  }
  return result;
}

export function ensureDependencies() {
  if (existsSync(path.join(webRoot, "node_modules", "ws", "package.json"))) return;
  writeOutput("\nInstalling web dependencies...\n");
  run(npmCommand, ["install"], { cwd: webRoot });
}

export function buildAll(config, skipRust = false) {
  ensureDependencies();
  buildLuvus(config, skipRust);
  writeOutput("\nBuilding web client and bridge...\n");
  run(npmCommand, ["run", "build"], { cwd: webRoot });
}

export function buildLuvus(config, skipRust = false) {
  if (!skipRust && !config.customBinary) {
    writeOutput("\nBuilding Luvus debug binary...\n");
    run(cargoCommand, ["build"], { cwd: repoRoot });
  }
  if (!existsSync(config.binary)) {
    throw new Error(`Luvus binary not found: ${config.binary}`);
  }
}

export function stopServer(config, quiet = false) {
  return run(config.binary, ["--session", config.session, "server", "stop"], {
    cwd: repoRoot,
    env: isolatedEnv(config),
    allowFailure: true,
    quiet,
    timeout: 20_000,
  });
}
