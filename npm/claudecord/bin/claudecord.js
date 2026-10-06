#!/usr/bin/env node
// Starts the claudecord program for this machine. The program itself is a single file that lives in a small package for each
// platform (claudecord-linux-x64 and so on); npm installs only the one that matches this machine, because each lists the
// operating system and CPU it is for. Nothing is downloaded when this runs, and nothing runs when it is installed.
"use strict";
const { spawnSync } = require("child_process");
const path = require("path");

// The program for each machine lives next to this file, in bin/<platform>-<cpu>/. Linux builds are static, so they run on any distribution.
const KEYS = ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64", "win32-x64"];

/** The path of the program to run, or an Error saying what is wrong. `dir` is where the programs are, `exists` says whether a file is there. */
function find(platform, arch, env, dir, exists) {
  if (env.CLAUDECORD_BINARY) return env.CLAUDECORD_BINARY;
  const key = platform + "-" + arch;
  if (!KEYS.includes(key)) return new Error("claudecord has no build for " + platform + " " + arch + ". Build it with: cargo install --git https://github.com/kanavdhanda/claudeCord");
  const file = path.join(dir, key, platform === "win32" ? "claudecord.exe" : "claudecord");
  if (!exists(file)) return new Error("the program for " + key + " is missing from this install (" + file + "). Reinstall claudecord, or set CLAUDECORD_BINARY to the program's path.");
  return file;
}

module.exports = { find, KEYS };

if (require.main === module) {
  const bin = find(process.platform, process.arch, process.env, __dirname, require("fs").existsSync);
  if (bin instanceof Error) {
    console.error(bin.message);
    process.exit(1);
  }
  // npm keeps the executable bit, but a file that lost it is made runnable again rather than failing.
  if (process.platform !== "win32") { try { require("fs").chmodSync(bin, 0o755); } catch (e) { /* read-only install: it may already be runnable */ } }
  const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
  if (r.error) {
    console.error("could not start " + bin + ": " + r.error.message);
    process.exit(1);
  }
  // Pass on the program's exit code, or the same signal it was stopped by.
  if (r.signal) process.kill(process.pid, r.signal);
  process.exit(r.status === null ? 1 : r.status);
}
