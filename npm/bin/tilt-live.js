#!/usr/bin/env node
// tilt-live: runs the latest tilt release (https://github.com/cloudycotton/tilt), downloading or
// updating it first when a newer one is out, and passes every argument through to it.
//
//   TILT_VERSION=0.2.0   run that release instead of the latest
//   TILT_NO_UPDATE=1     do not look for updates (run the newest one already downloaded)
//
// With no --token, --token-file or --no-auth (nor their TILT_* variables), it makes a token,
// keeps it in ~/.config/tilt-live/token and prints the link to open.
'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

// TILT_RELEASES points elsewhere for tests: a URL with GitHub's /latest and /download layout.
const RELEASES = process.env.TILT_RELEASES || 'https://github.com/cloudycotton/tilt/releases';
const CHECK_TIMEOUT_MS = 2000;
const DOWNLOAD_TIMEOUT_MS = 120_000;
const KEEP_VERSIONS = 2;
const TRIPLES = { x64: 'x86_64-unknown-linux-musl', arm64: 'aarch64-unknown-linux-musl' };

const home = os.homedir();
const cacheDir = path.join(process.env.XDG_CACHE_HOME || path.join(home, '.cache'), 'tilt-live');
const configDir = path.join(process.env.XDG_CONFIG_HOME || path.join(home, '.config'), 'tilt-live');

function fail(msg) {
  process.stderr.write(`tilt-live: ${msg}\n`);
  process.exit(1);
}

const cmp = (a, b) => {
  const pa = a.split(/[.-]/).map(Number);
  const pb = b.split(/[.-]/).map(Number);
  for (let i = 0; i < 3; i++) if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) - (pb[i] || 0);
  return 0;
};

/** Versions already downloaded, newest first. */
function cached() {
  try {
    return fs.readdirSync(cacheDir)
      .filter((v) => /^\d+\.\d+\.\d+$/.test(v) && fs.existsSync(path.join(cacheDir, v, 'tilt')))
      .sort(cmp)
      .reverse();
  } catch {
    return [];
  }
}

/** The newest release's version, from the redirect of /releases/latest; null when unknown. */
async function latest() {
  try {
    const res = await fetch(`${RELEASES}/latest`, {
      redirect: 'manual',
      signal: AbortSignal.timeout(CHECK_TIMEOUT_MS),
    });
    const m = /\/tag\/v?(\d+\.\d+\.\d+)$/.exec(res.headers.get('location') || '');
    return m ? m[1] : null;
  } catch {
    return null;
  }
}

async function download(version, triple) {
  const base = `${RELEASES}/download/v${version}`;
  const get = async (name) => {
    const res = await fetch(`${base}/${name}`, { signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS) });
    if (!res.ok) throw new Error(`${name}: HTTP ${res.status}`);
    return Buffer.from(await res.arrayBuffer());
  };
  const name = `tilt-${triple}`;
  const [bin, sums] = await Promise.all([get(name), get('SHA256SUMS')]);
  const want = sums.toString().split('\n').map((l) => l.trim().split(/\s+/)).find((f) => f[1] === name)?.[0];
  const got = crypto.createHash('sha256').update(bin).digest('hex');
  if (want !== got) throw new Error(`${name}: checksum mismatch`);
  const dir = path.join(cacheDir, version);
  fs.mkdirSync(dir, { recursive: true });
  const tmp = path.join(dir, `tilt.${process.pid}`);
  fs.writeFileSync(tmp, bin, { mode: 0o755 });
  fs.renameSync(tmp, path.join(dir, 'tilt'));
  for (const old of cached().slice(KEEP_VERSIONS)) fs.rmSync(path.join(cacheDir, old), { recursive: true, force: true });
}

/** The binary to run: the release asked for, else the latest, downloaded if need be. */
async function binary() {
  const triple = TRIPLES[process.arch];
  if (process.platform !== 'linux' || !triple) fail('tilt runs on Linux, on x86_64 or arm64');
  const have = cached();
  let version = process.env.TILT_VERSION;
  if (!version && !process.env.TILT_NO_UPDATE) version = await latest();
  version ||= have[0];
  if (!version) fail('cannot find the latest tilt release (check the network, or set TILT_VERSION)');
  if (!have.includes(version)) {
    process.stderr.write(`tilt-live: downloading tilt ${version}...\n`);
    try {
      await download(version, triple);
    } catch (e) {
      if (!have[0]) fail(`download failed: ${e.message}`);
      process.stderr.write(`tilt-live: download failed (${e.message}); running ${have[0]}\n`);
      version = have[0];
    }
  }
  return path.join(cacheDir, version, 'tilt');
}

/** Adds --token-file with a kept token when no access control was given, and says where to go. */
function withAuth(args) {
  const given = args.some((a) => /^--(token|token-file|no-auth)(=|$)/.test(a))
    || ['TILT_TOKEN', 'TILT_TOKEN_FILE', 'TILT_NO_AUTH'].some((v) => process.env[v]);
  const info = ['-h', '--help', '-V', '--version'].some((a) => args.includes(a));
  if (given || info) return args;
  const file = path.join(configDir, 'token');
  let token = '';
  try { token = fs.readFileSync(file, 'utf8').trim(); } catch { /* none yet */ }
  if (!token) {
    token = crypto.randomBytes(16).toString('hex');
    fs.mkdirSync(configDir, { recursive: true, mode: 0o700 });
    fs.writeFileSync(file, `${token}\n`, { mode: 0o600 });
  }
  const bind = (args.find((a) => a.startsWith('--bind='))?.slice(7))
    || (args.includes('--bind') ? args[args.indexOf('--bind') + 1] : null)
    || process.env.TILT_BIND || '0.0.0.0:6090';
  const at = bind.lastIndexOf(':');
  const [host, port] = [bind.slice(0, at).replace(/^\[|\]$/g, ''), bind.slice(at + 1)];
  // Bound to every interface: the machine's own addresses; else the one it is bound to.
  const hosts = host === '0.0.0.0' || host === '::'
    ? ['localhost', ...Object.values(os.networkInterfaces()).flat()
      .filter((i) => i && i.family === 'IPv4' && !i.internal).map((i) => i.address)]
    : [host.includes(':') ? `[${host}]` : host];
  process.stderr.write(`tilt-live: open ${hosts.map((h) => `http://${h}:${port}/#token=${token}`).join('\n           or ')}\n`);
  return [...args, '--token-file', file];
}

(async () => {
  const bin = await binary();
  const args = withAuth(process.argv.slice(2));
  const child = spawn(bin, args, { stdio: 'inherit' });
  for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT']) process.on(sig, () => child.kill(sig));
  child.on('error', (e) => fail(e.message));
  child.on('exit', (code, sig) => process.exit(code ?? (sig ? 128 + os.constants.signals[sig] : 1)));
})();
