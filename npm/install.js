#!/usr/bin/env node
// Downloads the correct oly binary for the current platform from GitHub Releases.

const https = require("https");
const fs = require("fs");
const path = require("path");
const { execSync } = require("child_process");
const os = require("os");

const pkg = require("./package.json");
const version = pkg.version;
const repo = "slaveOftime/open-relay";

function getPlatformAsset() {
  const platform = process.platform;
  const arch = process.arch;

  if (platform === "linux" && arch === "x64") return "oly-linux-amd64.zip";
  if (platform === "darwin" && arch === "arm64") return "oly-macos-arm64.zip";
  if (platform === "win32" && arch === "x64") return "oly-windows-amd64.zip";

  throw new Error(
    `Unsupported platform/arch: ${platform}/${arch}. ` +
      "Please download the binary manually from https://github.com/" +
      repo +
      "/releases"
  );
}

function getBinaryName() {
  return process.platform === "win32" ? "oly.exe" : "oly";
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest);
    const request = (u) => {
      https.get(u, (res) => {
        if (res.statusCode === 301 || res.statusCode === 302) {
          return request(res.headers.location);
        }
        if (res.statusCode !== 200) {
          reject(new Error(`Failed to download ${u}: HTTP ${res.statusCode}`));
          return;
        }
        res.pipe(file);
        file.on("finish", () => file.close(resolve));
      }).on("error", reject);
    };
    request(url);
  });
}

async function install() {
  const asset = getPlatformAsset();
  const url = `https://github.com/${repo}/releases/download/v${version}/${asset}`;
  const binDir = path.join(__dirname, "bin");
  const zipPath = path.join(os.tmpdir(), asset);
  const binaryName = getBinaryName();
  const binaryDest = path.join(binDir, binaryName);

  if (!fs.existsSync(binDir)) fs.mkdirSync(binDir, { recursive: true });

  console.log(`oly: downloading ${url}`);
  await download(url, zipPath);

  // Unzip
  if (process.platform === "win32") {
    execSync(
      `powershell -Command "Expand-Archive -Path '${zipPath}' -DestinationPath '${binDir}' -Force"`,
      { stdio: "inherit" }
    );
  } else {
    execSync(`unzip -o "${zipPath}" -d "${binDir}"`, { stdio: "inherit" });
    fs.chmodSync(binaryDest, 0o755);
  }

  fs.unlinkSync(zipPath);
  console.log(`oly: installed to ${binaryDest}`);
}

install().catch((err) => {
  console.error("oly install failed:", err.message);
  process.exit(1);
});
