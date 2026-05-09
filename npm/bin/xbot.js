#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

function targetTriple() {
  const platform = process.platform;
  const arch = process.arch;
  if ((platform !== "linux" && platform !== "darwin") || (arch !== "x64" && arch !== "arm64")) {
    throw new Error(`Unsupported platform: ${platform}-${arch}. Supported: linux/darwin x64/arm64.`);
  }
  return `${platform}-${arch}`;
}

function main() {
  const target = targetTriple();
  const root = path.resolve(__dirname, "..");
  const binary = path.join(root, "vendor", target, "xbot");
  if (!fs.existsSync(binary)) {
    throw new Error(
      `xbot native binary is missing for ${target}. Reinstall the package or set XBOT_INSTALL_BASE_URL for a custom release mirror.`
    );
  }

  const env = { ...process.env };
  if (!env.XBOT_BUILTIN_SKILLS_DIR) {
    env.XBOT_BUILTIN_SKILLS_DIR = path.join(root, "skills");
  }

  const result = spawnSync(binary, process.argv.slice(2), {
    stdio: "inherit",
    env
  });
  if (result.error) {
    throw result.error;
  }
  if (result.signal) {
    process.kill(process.pid, result.signal);
  }
  process.exit(result.status === null ? 1 : result.status);
}

try {
  main();
} catch (err) {
  console.error(`xbot: ${err.message}`);
  process.exit(1);
}
