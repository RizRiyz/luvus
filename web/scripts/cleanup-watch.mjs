import { spawnSync } from "node:child_process";
import path from "node:path";

const [parentRaw, binary, home, session] = process.argv.slice(2);
const parent = Number(parentRaw);
if (!Number.isSafeInteger(parent) || parent <= 1 || !path.isAbsolute(binary) || !path.isAbsolute(home) || !session) {
  process.exit(2);
}

const timer = setInterval(() => {
  if (alive(parent)) return;
  clearInterval(timer);
  const env = { ...process.env, LUVUS_HOME: home };
  delete env.LUVUS_SOCKET_PATH;
  delete env.LUVUS_SESSION;
  spawnSync(binary, ["--session", session, "server", "stop"], {
    cwd: path.dirname(binary),
    env,
    stdio: "ignore",
    timeout: 20_000,
    windowsHide: true,
  });
  process.exit(0);
}, 250);

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code !== "ESRCH";
  }
}
