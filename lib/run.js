const { spawnSync } = require("node:child_process");

const { resolveBinary } = require("./platform");

function runBinary(binaryName) {
  let binaryPath;
  try {
    binaryPath = resolveBinary(binaryName);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    console.error(`mcpstead: ${message}`);
    process.exit(1);
  }

  const result = spawnSync(binaryPath, process.argv.slice(2), {
    stdio: "inherit",
  });

  if (result.error) {
    console.error(`mcpstead: failed to launch ${binaryName}: ${result.error.message}`);
    process.exit(1);
  }

  if (result.signal) {
    process.kill(process.pid, result.signal);
    return;
  }

  process.exit(result.status ?? 1);
}

module.exports = {
  runBinary,
};
