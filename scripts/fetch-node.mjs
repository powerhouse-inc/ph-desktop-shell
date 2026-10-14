#!/usr/bin/env node
// Fetch the stock Node binary Tauri ships as the sidecar (externalBin
// "binaries/node"). Tauri resolves it per target triple, so the file must be
// named node-<triple>.exe. ~88 MB — gitignored, fetched on demand.
//
// Usage: node scripts/fetch-node.mjs <app-dir> [node-version]
//   e.g. node scripts/fetch-node.mjs apps/powerhouse-desktop
import { execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const appDir = process.argv[2];
if (!appDir) {
  console.error("usage: node scripts/fetch-node.mjs <app-dir> [node-version]");
  process.exit(1);
}
const version = process.argv[3] ?? process.versions.node;

const triples = {
  "win32 x64": { triple: "x86_64-pc-windows-msvc", dist: `win-x64`, bin: "node.exe", ext: ".exe" },
  "darwin arm64": { triple: "aarch64-apple-darwin", dist: "darwin-arm64", bin: "bin/node", ext: "" },
  "darwin x64": { triple: "x86_64-apple-darwin", dist: "darwin-x64", bin: "bin/node", ext: "" },
  "linux x64": { triple: "x86_64-unknown-linux-gnu", dist: "linux-x64", bin: "bin/node", ext: "" },
};
const key = `${process.platform} ${process.arch}`;
const target = triples[key];
if (!target) {
  console.error(`unsupported platform: ${key}`);
  process.exit(1);
}

const binariesDir = path.resolve(appDir, "binaries");
fs.mkdirSync(binariesDir, { recursive: true });
const dest = path.join(binariesDir, `node-${target.triple}${target.ext}`);
if (fs.existsSync(dest)) {
  console.log(`already present: ${dest}`);
  process.exit(0);
}

if (process.platform === "win32") {
  // Windows ships a bare node.exe — no archive to unpack.
  const url = `https://nodejs.org/dist/v${version}/win-x64/node.exe`;
  console.log(`fetching ${url}`);
  execSync(`curl -fsSL -o "${dest}" "${url}"`, { stdio: "inherit" });
} else {
  const name = `node-v${version}-${target.dist}`;
  const url = `https://nodejs.org/dist/v${version}/${name}.tar.gz`;
  console.log(`fetching ${url}`);
  execSync(
    `curl -fsSL "${url}" | tar -xz -C "${binariesDir}" --strip-components=2 ${name}/${target.bin}`,
    { stdio: "inherit", shell: "/bin/sh" },
  );
  fs.renameSync(path.join(binariesDir, "node"), dest);
  fs.chmodSync(dest, 0o755);
}
console.log(`sidecar node ready: ${dest}`);
