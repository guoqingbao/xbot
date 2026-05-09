#!/usr/bin/env node
"use strict";

const fs = require("fs");
const https = require("https");
const os = require("os");
const path = require("path");
const { createHash } = require("crypto");
const { spawnSync } = require("child_process");

const pkg = require("../package.json");

function targetTriple() {
  const platform = process.platform;
  const arch = process.arch;
  if ((platform !== "linux" && platform !== "darwin") || (arch !== "x64" && arch !== "arm64")) {
    throw new Error(`Unsupported platform: ${platform}-${arch}. Supported: linux/darwin x64/arm64.`);
  }
  return `${platform}-${arch}`;
}

function download(url, destination, redirects = 0) {
  if (redirects > 5) {
    return Promise.reject(new Error(`Too many redirects while downloading ${url}`));
  }
  return new Promise((resolve, reject) => {
    const request = https.get(url, (response) => {
      if (
        response.statusCode >= 300 &&
        response.statusCode < 400 &&
        response.headers.location
      ) {
        response.resume();
        const next = new URL(response.headers.location, url).toString();
        download(next, destination, redirects + 1).then(resolve, reject);
        return;
      }
      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`Download failed (${response.statusCode}) for ${url}`));
        return;
      }
      const file = fs.createWriteStream(destination);
      response.pipe(file);
      file.on("finish", () => file.close(resolve));
      file.on("error", reject);
    });
    request.on("error", reject);
  });
}

function sha256(file) {
  const hash = createHash("sha256");
  hash.update(fs.readFileSync(file));
  return hash.digest("hex");
}

function expectedSha(manifest, artifactName) {
  for (const line of manifest.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    const parts = trimmed.split(/\s+/);
    const checksum = parts[0];
    const name = parts[parts.length - 1].replace(/^\*/, "");
    if (name === artifactName) {
      return checksum;
    }
  }
  return null;
}

async function main() {
  const target = targetTriple();
  const version = process.env.XBOT_INSTALL_VERSION || pkg.version;
  const tag = process.env.XBOT_INSTALL_TAG || `v${version}`;
  const base =
    process.env.XBOT_INSTALL_BASE_URL ||
    `https://github.com/guoqingbao/xbot/releases/download/${tag}`;
  const artifactName = `xbot-${version}-${target}.tar.gz`;
  const artifactUrl = `${base}/${artifactName}`;
  const sumsUrl = `${base}/SHA256SUMS`;
  const root = path.resolve(__dirname, "..");
  const installDir = path.join(root, "vendor", target);
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "xbot-install-"));
  const archivePath = path.join(tmpDir, artifactName);
  const sumsPath = path.join(tmpDir, "SHA256SUMS");

  try {
    await download(sumsUrl, sumsPath);
    await download(artifactUrl, archivePath);
    const expected = expectedSha(fs.readFileSync(sumsPath, "utf8"), artifactName);
    if (!expected) {
      throw new Error(`No checksum entry found for ${artifactName}`);
    }
    const actual = sha256(archivePath);
    if (actual !== expected) {
      throw new Error(`Checksum mismatch for ${artifactName}: expected ${expected}, got ${actual}`);
    }

    fs.rmSync(installDir, { recursive: true, force: true });
    fs.mkdirSync(installDir, { recursive: true });
    const tar = spawnSync("tar", ["-xzf", archivePath, "-C", installDir], {
      stdio: "inherit"
    });
    if (tar.error) {
      throw tar.error;
    }
    if (tar.status !== 0) {
      throw new Error(`tar exited with status ${tar.status}`);
    }
    const binary = path.join(installDir, "xbot");
    fs.chmodSync(binary, 0o755);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(`xbot install failed: ${err.message}`);
  process.exit(1);
});
