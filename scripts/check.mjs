#!/usr/bin/env node
// The CI gate, runnable locally: ci.yml calls exactly this script, so a green
// `node scripts/check.mjs` means a green CI by construction. Run it before
// every push.
import { execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "..");

function step(name, cmd) {
  console.log(`\n=== ${name}: ${cmd}`);
  execSync(cmd, { cwd: root, stdio: "inherit" });
}

// tauri-build validates externalBin (binaries/node-<triple>) at COMPILE time —
// clippy included — so the sidecar binary must exist before any cargo step.
if (!fs.readdirSync(path.join(root, "apps")).every((app) =>
  fs.existsSync(path.join(root, "apps", app, "binaries")) &&
  fs.readdirSync(path.join(root, "apps", app, "binaries")).some((f) => f.startsWith("node-")),
)) {
  step("fetch sidecar node", "node scripts/fetch-node.mjs apps/powerhouse-desktop");
} else {
  console.log("=== sidecar node: present");
}

step("rustfmt", "cargo fmt --all --check");
step("clippy", "cargo clippy --workspace --locked -- -D warnings");
step("build", "cargo build --workspace --locked");
step("test", "cargo test --workspace --locked");

console.log("\nAll checks passed — CI will agree.");
