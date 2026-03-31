#!/usr/bin/env node
// Thin launcher: finds the bundled oly binary and exec's it.

const path = require("path");
const fs = require("fs");
const { spawnSync } = require("child_process");

const binaryNames = {
  "linux:x64": "oly-linux-x64",
  "darwin:arm64": "oly-darwin-arm64",
  "win32:x64": "oly-win32-x64.exe",
};
const binaryName = binaryNames[`${process.platform}:${process.arch}`];

if (!binaryName) {
  console.error(
    `oly: unsupported platform/arch: ${process.platform}/${process.arch}. ` +
      "Please download a release from https://github.com/slaveOftime/open-relay/releases"
  );
  process.exit(1);
}

const binaryPath = path.join(__dirname, binaryName);

if (!fs.existsSync(binaryPath)) {
  console.error(
    `oly: bundled binary not found at ${binaryPath}. Try reinstalling: npm install -g @slaveoftime/oly`
  );
  process.exit(1);
}

const result = spawnSync(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  env: process.env,
});

process.exit(result.status ?? 1);
