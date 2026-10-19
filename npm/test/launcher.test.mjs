// Tests of npm/bin/tilt-live.js and install.sh against a fake release server:
// `node --test npm/test/*.test.mjs`.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { after, before, beforeEach, test } from 'node:test';
import { fileURLToPath } from 'node:url';

const LAUNCHER = fileURLToPath(new URL('../bin/tilt-live.js', import.meta.url));
const INSTALL = fileURLToPath(new URL('../../install.sh', import.meta.url));
const TRIPLE = { x64: 'x86_64-unknown-linux-musl', arm64: 'aarch64-unknown-linux-musl' }[process.arch];

// The released "tilt" prints its arguments and exits with code 7 (0 for --version, like tilt).
const FAKE = '#!/bin/sh\necho "tilt $*"\n[ "$1" = --version ] && exit 0\nexit 7\n';

let server;
let base;
let corrupt = false;
let hits = [];

before(async () => {
  server = http.createServer((req, res) => {
    hits.push(req.url);
    const sum = crypto.createHash('sha256').update(corrupt ? 'other' : FAKE).digest('hex');
    if (req.url === `/releases/latest/download/tilt-${TRIPLE}`) res.end(FAKE);
    else if (req.url === '/releases/latest/download/SHA256SUMS') res.end(`${sum}  tilt-${TRIPLE}\n${'0'.repeat(64)}  tilt-other\n`);
    else res.writeHead(404).end();
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${server.address().port}/releases`;
});

after(() => server.close());

let home;
beforeEach(() => {
  home = fs.mkdtempSync(path.join(os.tmpdir(), 'tilt-live-'));
  corrupt = false;
  hits = [];
});

/** Runs `cmd args` with a fresh HOME; resolves to { code, out, err }. */
function run(cmd, args, env = {}) {
  const child = spawn(cmd, args, {
    env: { PATH: process.env.PATH, HOME: home, XDG_CACHE_HOME: path.join(home, 'cache'), TILT_RELEASES: base, ...env },
  });
  const r = { out: '', err: '' };
  child.stdout.on('data', (d) => { r.out += d; });
  child.stderr.on('data', (d) => { r.err += d; });
  return new Promise((resolve) => child.on('exit', (code) => resolve({ ...r, code })));
}
const launch = (args, env) => run(process.execPath, [LAUNCHER, ...args], env);

test('downloads tilt once and passes arguments and exit code through', async () => {
  let r = await launch(['--display', ':1']);
  assert.equal(r.code, 7, r.err);
  assert.equal(r.out, 'tilt --display :1\n');
  assert.match(r.err, /downloading tilt/);
  hits = [];
  // tilt keeps itself up to date from then on: the launcher does not download again.
  r = await launch(['--version']);
  assert.equal(r.code, 0);
  assert.equal(r.out, 'tilt --version\n');
  assert.deepEqual(hits, []);
});

test('refuses a binary whose checksum does not match', async () => {
  corrupt = true;
  const r = await launch(['--version']);
  assert.equal(r.code, 1);
  assert.match(r.err, /checksum mismatch/);
  assert.ok(!fs.existsSync(path.join(home, 'cache', 'tilt-live', 'tilt')));
});

test('install.sh installs a verified tilt', async () => {
  const dir = path.join(home, 'bin');
  const r = await run('/bin/sh', [INSTALL], { TILT_INSTALL_DIR: dir });
  assert.equal(r.code, 0, r.err);
  assert.match(r.out, new RegExp(`installed ${dir}/tilt`));
  const v = await run(path.join(dir, 'tilt'), ['--version']);
  assert.equal(v.out, 'tilt --version\n');
  corrupt = true;
  const bad = await run('/bin/sh', [INSTALL], { TILT_INSTALL_DIR: path.join(home, 'bad') });
  assert.notEqual(bad.code, 0);
  assert.match(bad.err, /checksum/);
  assert.ok(!fs.existsSync(path.join(home, 'bad', 'tilt')));
});
