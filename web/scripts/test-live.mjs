import { buildLuvus, ensureDependencies, npmCommand, run, settings, webRoot } from "./common.mjs";

const config = settings();
ensureDependencies();
buildLuvus(config);
process.stdout.write("\nRunning isolated browser/UHP integration test...\n");
run(npmCommand, ["run", "test:integration"], {
  cwd: webRoot,
  env: { ...process.env, LUVUS_BIN: config.binary },
  timeout: 10 * 60 * 1000,
});
