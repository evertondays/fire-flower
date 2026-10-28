// Compila o addon (cargo, release) e copia a biblioteca para fire-flower.node.
"use strict";

const { execFileSync } = require("node:child_process");
const { copyFileSync } = require("node:fs");
const path = require("node:path");

const root = path.resolve(__dirname, "..", "..", "..");
const profile = process.argv.includes("--debug") ? "debug" : "release";
const args = ["build", "-p", "fire-flower-node"];
if (profile === "release") args.push("--release");
execFileSync("cargo", args, { cwd: root, stdio: "inherit" });

const file = {
  linux: "libfire_flower_node.so",
  darwin: "libfire_flower_node.dylib",
  win32: "fire_flower_node.dll",
}[process.platform];
const target = process.env.CARGO_TARGET_DIR || path.join(root, "target");
copyFileSync(path.join(target, profile, file), path.join(__dirname, "..", "fire-flower.node"));
