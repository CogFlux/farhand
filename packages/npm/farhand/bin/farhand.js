#!/usr/bin/env node
// Runs the prebuilt `farhand` binary, fetching it from the GitHub release
// that matches this package's version on first use. No dependencies.
"use strict";
const { spawnSync, execFileSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const crypto = require("node:crypto");
const https = require("node:https");

const pkg = require("../package.json");
const REPO = "CogFlux/farhand";
const version = pkg.version;

function target() {
  const os_t = { darwin: "apple-darwin", linux: "unknown-linux-gnu" }[process.platform];
  const arch_t = { arm64: "aarch64", x64: "x86_64" }[process.arch];
  if (!os_t || !arch_t) {
    console.error(`farhand: unsupported platform ${process.platform}/${process.arch} (macOS and Linux, arm64 and x86_64)`);
    process.exit(1);
  }
  return `${arch_t}-${os_t}`;
}

function fetch(url) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "user-agent": `farhand-npm/${version}` } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          return fetch(res.headers.location).then(resolve, reject);
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`${url}: HTTP ${res.statusCode}`));
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

async function ensureBinary() {
  const name = `farhand-${version}-${target()}`;
  const vendor = path.join(__dirname, "..", "vendor");
  const bin = path.join(vendor, name, "farhand");
  if (fs.existsSync(bin)) return bin;

  const base = `https://github.com/${REPO}/releases/download/v${version}`;
  console.error(`farhand: downloading ${name}.tar.gz`);
  const [tarball, sums] = await Promise.all([fetch(`${base}/${name}.tar.gz`), fetch(`${base}/SHA256SUMS`)]);
  const line = sums.toString().split("\n").find((l) => l.endsWith(` ${name}.tar.gz`));
  if (!line) throw new Error(`no checksum for ${name}.tar.gz in SHA256SUMS`);
  const expected = line.split(" ")[0];
  const actual = crypto.createHash("sha256").update(tarball).digest("hex");
  if (expected !== actual) throw new Error("checksum mismatch; refusing to install");

  fs.mkdirSync(vendor, { recursive: true });
  const tmp = path.join(os.tmpdir(), `${name}-${process.pid}.tar.gz`);
  fs.writeFileSync(tmp, tarball);
  try {
    execFileSync("tar", ["-C", vendor, "-xzf", tmp]);
  } finally {
    fs.rmSync(tmp, { force: true });
  }
  fs.chmodSync(bin, 0o755);
  return bin;
}

ensureBinary()
  .then((bin) => {
    const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
    if (r.error) throw r.error;
    process.exit(r.status === null ? 1 : r.status);
  })
  .catch((e) => {
    console.error(`farhand: ${e.message}`);
    process.exit(1);
  });
