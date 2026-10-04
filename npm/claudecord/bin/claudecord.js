#!/usr/bin/env node
// Starts the claudecord program for this machine. The program itself is a single file that lives in a small package for each
// platform (claudecord-linux-x64 and so on); npm installs only the one that matches this machine, because each lists the
// operating system and CPU it is for. Nothing is downloaded when this runs, and nothing runs when it is installed.
"use strict";
const { spawnSync } = require("child_process");
const path = require("path");

// Which package holds the program for which machine. Linux builds are static, so they run on any distribution.
const PACKAGES = {
  "linux-x64": "claudecord-linux-x64",
  "linux-arm64": "claudecord-linux-arm64",
  "darwin-x64": "claudecord-darwin-x64",
  "darwin-arm64": "claudecord-darwin-arm64",
  "win32-x64": "claudecord-win32-x64",
};

/** The path of the program to run, or an Error saying what is wrong. */
function find(platform, arch, env, resolve) {
  if (env.CLAUDECORD_BINARY) return env.CLAUDECORD_BINARY;
  const pkg = PACKAGES[platform + "-" + arch];
  if (!pkg) return new Error("claudecord has no build for " + platform + " " + arch + ". Build it with: cargo install --git https://github.com/kanavdhanda/claudeCord");
  try {
    return path.join(path.dirname(resolve(pkg + "/package.json")), "bin", platform === "win32" ? "claudecord.exe" : "claudecord");
  } catch (e) {
    return new Error("the package " + pkg + " is not installed. Reinstall without --omit=optional (or --no-optional), or set CLAUDECORD_BINARY to the program's path.");
  }
}

module.exports = { find, PACKAGES };

if (require.main === module) {
  const bin = find(process.platform, process.arch, process.env, require.resolve);
  if (bin instanceof Error) {
    console.error(bin.message);
    process.exit(1);
  }
  const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
  if (r.error) {
    console.error("could not start " + bin + ": " + r.error.message);
    process.exit(1);
  }
  // Pass on the program's exit code, or the same signal it was stopped by.
  if (r.signal) process.kill(process.pid, r.signal);
  process.exit(r.status === null ? 1 : r.status);
}
