#!/usr/bin/env node
"use strict";

const { spawn } = require("child_process");
const path = require("path");
const fs = require("fs");

// Legacy alias notice: `telecli` was the pre-0.10 package name. Both spellings
// run the same binary; the alias will be removed in a future major release.
const invokedAs = path.basename(process.argv[1] || "", ".exe");
if (invokedAs === "telecli") {
  console.error("[telecli] deprecated alias of `tele`; will be removed in a future major release.");
}

pickExeAndSpawn();

function isMusl() {
  // process.report carries the runtime libc identity (node >= 12.17 on Linux);
  // ldd --version is only consulted when the report is unavailable.
  try {
    if (typeof process.report?.getReport === "function") {
      const header = process.report.getReport().header || {};
      if (header.glibcVersionRuntime) return false;
      if (process.report.getReport().header.osName) {
        // report exists but carries no glibc marker: musl builds simply lack it
        return true;
      }
    }
  } catch {
    // fall through to ldd
  }
  try {
    const out = require("child_process").execSync("ldd --version 2>&1 || true", {
      encoding: "utf8",
    });
    return out.includes("musl");
  } catch {
    return false;
  }
}

// Spawn the first existing candidate; on spawn failure (broken/undecodable
// binary despite the existsSync probe) fall through to the remaining
// candidates instead of dying on the first miss. Spawn errors surface
// asynchronously on the child's "error" event, so the retry lives there.
function pickExeAndSpawn() {
  const platform = process.platform;
  const arch = process.arch;
  const ext = platform === "win32" ? ".exe" : "";
  const prefixes = ["tele-", "telecli-"];
  const candidates = [];
  if (platform === "win32" && arch === "x64") candidates.push("x86_64-pc-windows-msvc");
  if (platform === "win32" && arch === "arm64") candidates.push("aarch64-pc-windows-msvc");
  if (platform === "darwin" && arch === "arm64") candidates.push("aarch64-apple-darwin");
  if (platform === "darwin" && arch === "x64") candidates.push("x86_64-apple-darwin");
  if (platform === "linux" && arch === "x64") {
    if (isMusl()) candidates.push("x86_64-unknown-linux-musl");
    candidates.push("x86_64-unknown-linux-gnu");
  }
  if (platform === "linux" && arch === "arm64") {
    if (isMusl()) candidates.push("aarch64-unknown-linux-musl");
    candidates.push("aarch64-unknown-linux-gnu");
  }
  if (platform === "linux" && arch === "arm") {
    if (isMusl()) candidates.push("armv7-unknown-linux-musleabihf");
    candidates.push("armv7-unknown-linux-gnueabihf");
  }
  if (platform === "linux" && arch === "ia32") candidates.push("i686-unknown-linux-musl");
  if (platform === "linux" && arch === "ppc64") candidates.push("powerpc64le-unknown-linux-gnu");
  if (platform === "linux" && arch === "riscv64") candidates.push("riscv64gc-unknown-linux-gnu");

  const attempts = [];
  for (const prefix of prefixes) {
    for (const triple of candidates) {
      attempts.push(path.join(__dirname, `${prefix}${triple}${ext}`));
    }
  }
  const existing = attempts.filter((p) => fs.existsSync(p));
  if (existing.length === 0) {
    console.error(
      `[tele] no runnable binary bundled for ${platform}-${arch}` +
        (candidates.length ? ` (tried: ${candidates.join(", ")})` : "") +
        ". Download from https://github.com/mahyartdb/tele-cli/releases"
    );
    process.exit(1);
  }

  let index = 0;
  function tryNext() {
    if (index >= existing.length) {
      console.error(
        `[tele] no runnable binary for ${platform}-${arch}: every candidate failed to start ` +
          `(tried: ${existing.join(", ")}). ` +
          "Reinstall the package, or download from https://github.com/mahyartdb/tele-cli/releases"
      );
      process.exit(1);
    }
    const p = existing[index++];
    let child;
    try {
      child = spawn(p, process.argv.slice(2), { stdio: "inherit" });
    } catch (err) {
      console.error(`[tele] failed to start ${p}: ${err.message}; trying next candidate`);
      tryNext();
      return;
    }
    child.on("error", (err) => {
      console.error(`[tele] failed to start ${p}: ${err.message}; trying next candidate`);
      tryNext();
    });
    child.on("exit", (code) => {
      process.exit(code ?? 1);
    });
  }
  tryNext();
}
