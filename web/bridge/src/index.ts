import { loadConfig } from "./config.js";
import { BridgeServer } from "./server.js";
import { UhpAccess } from "./uhp.js";

const config = loadConfig();
const uhp = new UhpAccess();
const bridge = new BridgeServer(config, uhp);
try {
  await uhp.start(config);
  await bridge.start();
} catch (error) {
  bridge.stop();
  uhp.stop();
  throw error;
}

const localUrl = `http://${config.host}:${bridge.port}`;
const pairing = bridge.authority.initialPairing;
const pairedUrl = `${config.publicUrl ?? localUrl}/#pair=${encodeURIComponent(pairing.code)}`;
process.stdout.write(`${JSON.stringify({
  type: "luvus_web_bridge",
  url: pairedUrl,
  pairing_expires_at: pairing.expiresAt,
  authority: uhp.authority,
})}\n`);

let stopping = false;
const stop = () => {
  if (stopping) return;
  stopping = true;
  bridge.stop();
  uhp.stop();
  setTimeout(() => process.exit(0), 50).unref();
};
process.once("SIGINT", stop);
process.once("SIGTERM", stop);
uhp.addEventListener("exit", () => { void uhp.restart().catch(() => {}); });
