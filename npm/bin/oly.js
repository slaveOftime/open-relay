#!/usr/bin/env node
// Thin launcher: finds the downloaded oly binary and exec's it.

const path = require("path");
const fs = require("fs");
const { spawnSync } = require("child_process");

const binaryName = process.platform === "win32" ? "oly.exe" : "oly";
const binaryPath = path.join(__dirname, binaryName);

if (!fs.existsSync(binaryPath)) {
  console.error(
    "oly: binary not found. Try reinstalling: npm install -g oly"
  );
  process.exit(1);
}

const result = spawnSync(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  env: process.env,
});

process.exit(result.status ?? 1);
