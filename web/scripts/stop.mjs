import { settings, stopServer } from "./common.mjs";

const config = settings();
const result = stopServer(config);
if (result.status !== 0) {
  process.stderr.write(`The isolated '${config.session}' session was not running or could not be stopped.\n`);
  process.exitCode = 1;
}
