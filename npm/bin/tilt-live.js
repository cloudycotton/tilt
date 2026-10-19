#!/usr/bin/env node
// tilt-live: runs tilt (https://github.com/cloudycotton/tilt) with every argument passed through.
// The first run downloads the latest release (checksum-verified); after that tilt updates itself
// whenever it starts. Linux on x86_64 or arm64.
'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

// TILT_RELEASES points elsewhere for tests: a URL with GitHub's /latest/download layout.
const RELEASES = process.env.TILT_RELEASES || 'https://github.com/cloudycotton/tilt/releases';
const TRIPLE = { x64: 'x86_64-unknown-linux-musl', arm64: 'aarch64-unknown-linux-musl' }[process.arch];
const cacheDir = path.join(process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache'), 'tilt-live');
const bin = path.join(cacheDir, 'tilt');

function fail(msg) {
  process.stderr.write(`tilt-live: ${msg}\n`);
  process.exit(1);
}

async function download() {
  const get = async (name) => {
    const res = await fetch(`${RELEASES}/latest/download/${name}`, { signal: AbortSignal.timeout(120_000) });
    if (!res.ok) throw new Error(`${name}: HTTP ${res.status}`);
    return Buffer.from(await res.arrayBuffer());
  };
  const name = `tilt-${TRIPLE}`;
  const [data, sums] = await Promise.all([get(name), get('SHA256SUMS')]);
  const want = sums.toString().split('\n').map((l) => l.trim().split(/\s+/)).find((f) => f[1] === name)?.[0];
  if (crypto.createHash('sha256').update(data).digest('hex') !== want) throw new Error(`${name}: checksum mismatch`);
  fs.mkdirSync(cacheDir, { recursive: true });
  const tmp = `${bin}.${process.pid}`;
  fs.writeFileSync(tmp, data, { mode: 0o755 });
  fs.renameSync(tmp, bin);
}

async function main() {
  if (process.platform !== 'linux') fail('tilt streams a Linux desktop: run it on the Linux machine or VM whose screen you want');
  if (!TRIPLE) fail('tilt runs on x86_64 or arm64');
  if (!fs.existsSync(bin)) {
    process.stderr.write('tilt-live: downloading tilt...\n');
    await download().catch((e) => fail(`download failed: ${e.message}`));
  }
  const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' });
  for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT']) process.on(sig, () => child.kill(sig));
  child.on('error', (e) => fail(e.message));
  child.on('exit', (code, sig) => process.exit(code ?? 128 + (os.constants.signals[sig] || 0)));
}

main();
