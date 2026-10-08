#!/usr/bin/env node
// tilt-live: runs the latest tilt release (https://github.com/cloudycotton/tilt), downloading or
// updating it first when a newer one is out, and passes every argument through to it.
//
// On Linux it streams this machine's X display. Anywhere else (macOS, Windows), or with
// TILT_DOCKER=1, it runs the release's demo desktop image (Xvfb + xfce + tilt) in Docker and
// streams that, publishing tilt's port on this machine.
//
//   TILT_VERSION=0.2.0   run that release instead of the latest
//   TILT_NO_UPDATE=1     do not look for updates (run the newest one already downloaded)
//   TILT_DOCKER=1        run the demo desktop in Docker, even on Linux
//   SCREEN, DESKTOP      the Docker desktop's screen (1920x1080x24) and session (xfce or none)
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
// The demo desktop image, published with each release as :<version> and :latest.
const IMAGE = process.env.TILT_IMAGE || 'ghcr.io/cloudycotton/tilt-desktop';
const DOCKER = !!process.env.TILT_DOCKER || process.platform !== 'linux';
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
  if (!triple) fail('tilt runs on x86_64 or arm64 (set TILT_DOCKER=1 for the demo desktop in Docker)');
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

/** The bind address from the arguments or TILT_BIND, as [host, port]. */
function bindAddress(args) {
  const bind = (args.find((a) => a.startsWith('--bind='))?.slice(7))
    || (args.includes('--bind') ? args[args.indexOf('--bind') + 1] : null)
    || process.env.TILT_BIND || '0.0.0.0:6090';
  const at = bind.lastIndexOf(':');
  return [bind.slice(0, at).replace(/^\[|\]$/g, ''), bind.slice(at + 1)];
}

/**
 * Access control: when none was given, a kept token, and the link to open. Returns the token
 * file to pass on (null when the caller chose its own access control).
 */
function access(args) {
  const given = args.some((a) => /^--(token|token-file|no-auth)(=|$)/.test(a))
    || ['TILT_TOKEN', 'TILT_TOKEN_FILE', 'TILT_NO_AUTH'].some((v) => process.env[v]);
  const info = ['-h', '--help', '-V', '--version'].some((a) => args.includes(a));
  if (given || info) return null;
  const file = path.join(configDir, 'token');
  let token = '';
  try { token = fs.readFileSync(file, 'utf8').trim(); } catch { /* none yet */ }
  if (!token) {
    token = crypto.randomBytes(16).toString('hex');
    fs.mkdirSync(configDir, { recursive: true, mode: 0o700 });
    fs.writeFileSync(file, `${token}\n`, { mode: 0o600 });
  }
  const [host, port] = bindAddress(args);
  // Bound to every interface: the machine's own addresses; else the one it is bound to.
  const hosts = host === '0.0.0.0' || host === '::'
    ? ['localhost', ...Object.values(os.networkInterfaces()).flat()
      .filter((i) => i && i.family === 'IPv4' && !i.internal).map((i) => i.address)]
    : [host.includes(':') ? `[${host}]` : host];
  process.stderr.write(`tilt-live: open ${hosts.map((h) => `http://${h}:${port}/#token=${token}`).join('\n           or ')}\n`);
  return file;
}

/** Runs `cmd` and resolves to its exit code (null when it could not start). */
const exitCode = (cmd, args) => new Promise((resolve) => {
  const child = spawn(cmd, args, { stdio: 'inherit' });
  for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT']) process.on(sig, () => child.kill(sig));
  child.on('error', (e) => { process.stderr.write(`tilt-live: ${e.message}\n`); resolve(null); });
  child.on('exit', (code, sig) => resolve(code ?? (sig ? 128 + os.constants.signals[sig] : 1)));
});

/** This machine's own X display: the release binary, with the kept token file. */
async function native(args) {
  const bin = await binary();
  const file = access(args);
  const code = await exitCode(bin, file ? [...args, '--token-file', file] : args);
  process.exit(code ?? 1);
}

/**
 * The demo desktop in Docker: the release's image, with tilt's port published at the bind
 * address (default 0.0.0.0:6090). The kept token goes in through the environment as TILT_TOKEN
 * (the file is private to this user, so the container's user could not read it, and the
 * environment keeps it out of the process list); token files the caller named are mounted
 * read-only.
 */
async function docker(args) {
  const ok = await new Promise((resolve) => {
    const child = spawn('docker', ['version', '--format', '{{.Server.Version}}'], { stdio: 'ignore' });
    child.on('error', () => resolve(false));
    child.on('exit', (code) => resolve(code === 0));
  });
  if (!ok) {
    fail(`${process.platform === 'linux' ? 'TILT_DOCKER is set, but' : `tilt streams a Linux desktop; on ${process.platform} it runs one in Docker, but`} docker is not running.
           Install Docker Desktop (https://docs.docker.com/desktop/) and start it, then run this again.`);
  }
  let version = process.env.TILT_VERSION;
  if (!version && !process.env.TILT_NO_UPDATE) version = await latest();
  const image = `${IMAGE}:${version || 'latest'}`;
  const [host, port] = bindAddress(args);
  const everywhere = host === '0.0.0.0' || host === '::';
  const publish = `${everywhere ? '' : `${host.includes(':') ? `[${host}]` : host}:`}${port}:6090`;
  const run = ['run', '--rm', '--init', '--shm-size', '1g', '-p', publish];
  if (process.stdin.isTTY && process.stdout.isTTY) run.push('-it');
  // The desktop's settings and tilt's own TILT_* variables go in, except the launcher's and
  // those that mean something else inside: the bind address, the display, the token file.
  const own = ['TILT_VERSION', 'TILT_NO_UPDATE', 'TILT_RELEASES', 'TILT_DOCKER', 'TILT_IMAGE',
    'TILT_BIND', 'TILT_DISPLAY', 'TILT_TOKEN_FILE'];
  for (const v of ['SCREEN', 'DESKTOP', ...Object.keys(process.env).filter((k) => k.startsWith('TILT_') && !own.includes(k))]) {
    if (process.env[v] !== undefined) run.push('-e', v);
  }
  run.push('-e', 'TESTPATTERN=0');
  // Just the binary for --help and --version: no desktop to bring up first.
  if (['-h', '--help', '-V', '--version'].some((a) => args.includes(a))) run.push('--entrypoint', 'tilt');
  // Inside the container tilt listens on 0.0.0.0:6090; --bind only chooses the published port.
  const passed = [];
  let mounts = 0;
  const mount = (file) => {
    const inside = `/run/tilt-live/token${mounts++ || ''}`;
    run.push('-v', `${path.resolve(file)}:${inside}:ro`);
    return inside;
  };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === '--bind') { i++; continue; }
    if (a.startsWith('--bind=')) continue;
    if (a === '--token-file') { passed.push(a, mount(args[++i])); continue; }
    if (a.startsWith('--token-file=')) { passed.push(`--token-file=${mount(a.slice(13))}`); continue; }
    passed.push(a);
  }
  if (process.env.TILT_TOKEN_FILE) run.push('-e', `TILT_TOKEN_FILE=${mount(process.env.TILT_TOKEN_FILE)}`);
  const file = access(args);
  if (file) {
    process.env.TILT_TOKEN = fs.readFileSync(file, 'utf8').trim();
    run.push('-e', 'TILT_TOKEN');
  }
  process.stderr.write(`tilt-live: starting the demo desktop ${image} in Docker...\n`);
  const code = await exitCode('docker', [...run, image, ...passed]);
  process.exit(code ?? 1);
}

(DOCKER ? docker : native)(process.argv.slice(2));
