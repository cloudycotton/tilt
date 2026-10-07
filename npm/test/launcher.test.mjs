// Tests of both launchers, npm/bin/tilt-live.js and its sh twin scripts/tilt-live (installed by
// install.sh), against a fake release server: `node --test npm/test/*.test.mjs`.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { after, before, beforeEach, describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';

const LAUNCHERS = {
  node: [process.execPath, fileURLToPath(new URL('../bin/tilt-live.js', import.meta.url))],
  sh: ['/bin/sh', fileURLToPath(new URL('../../scripts/tilt-live', import.meta.url))],
};
const TRIPLE = { x64: 'x86_64-unknown-linux-musl', arm64: 'aarch64-unknown-linux-musl' }[process.arch];

// Each release's "tilt" prints its version and arguments, and exits with code 7 (0 for
// --version, like tilt).
const fake = (version) => `#!/bin/sh\necho "tilt ${version} $*"\n[ "$1" = --version ] && exit 0\nexit 7\n`;

let server;
let base;
let latest = '0.1.0';
let corrupt = false;
let hits = [];

before(async () => {
  server = http.createServer((req, res) => {
    hits.push(req.url);
    if (req.url === '/releases/latest') {
      res.writeHead(302, { location: `${base}/tag/v${latest}` }).end();
      return;
    }
    if (req.url === '/releases/latest/download/tilt-live') {
      res.end(fs.readFileSync(LAUNCHERS.sh[1]));
      return;
    }
    const m = /^\/releases\/download\/v([\d.]+)\/(.+)$/.exec(req.url);
    if (!m) {
      res.writeHead(404).end();
      return;
    }
    const bin = fake(m[1]);
    const sum = crypto.createHash('sha256').update(corrupt ? 'other' : bin).digest('hex');
    if (m[2] === `tilt-${TRIPLE}`) res.end(bin);
    else if (m[2] === 'SHA256SUMS') res.end(`${sum}  tilt-${TRIPLE}\n${'0'.repeat(64)}  tilt-other\n`);
    else res.writeHead(404).end();
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${server.address().port}/releases`;
});

after(() => server.close());

let home;
beforeEach(() => {
  home = fs.mkdtempSync(path.join(os.tmpdir(), 'tilt-live-'));
  latest = '0.1.0';
  corrupt = false;
  hits = [];
});

for (const [name, [interpreter, launcher]] of Object.entries(LAUNCHERS)) describe(name, () => {
  /** Runs the launcher with `args` and extra environment; resolves to { code, out, err }. */
  function run(args = [], env = {}) {
    const child = spawn(interpreter, [launcher, ...args], {
      env: {
        PATH: process.env.PATH,
        HOME: home,
        XDG_CACHE_HOME: path.join(home, 'cache'),
        XDG_CONFIG_HOME: path.join(home, 'config'),
        TILT_RELEASES: base,
        ...env,
      },
    });
    const r = { out: '', err: '' };
    child.stdout.on('data', (d) => { r.out += d; });
    child.stderr.on('data', (d) => { r.err += d; });
    return new Promise((resolve) => child.on('close', (code) => resolve({ ...r, code })));
  }

  const versions = () => fs.readdirSync(path.join(home, 'cache', 'tilt-live')).sort();

  test('downloads the latest release, runs it with the arguments and its exit code', async () => {
    const r = await run(['--display', ':5']);
    assert.equal(r.code, 7);
    assert.match(r.out, /^tilt 0\.1\.0 --display :5 --token-file \S+\/config\/tilt-live\/token$/m);
    assert.deepEqual(versions(), ['0.1.0']);
  });

  test('makes a private token once, keeps it, and prints the link', async () => {
    const first = await run();
    const file = path.join(home, 'config', 'tilt-live', 'token');
    const token = fs.readFileSync(file, 'utf8').trim();
    assert.match(token, /^[0-9a-f]{32}$/);
    assert.equal(fs.statSync(file).mode & 0o777, 0o600);
    assert.match(first.err, new RegExp(`open http://localhost:6090/#token=${token}`));
    await run();
    assert.equal(fs.readFileSync(file, 'utf8').trim(), token);
    // A bind address of its own is the one in the link.
    assert.match((await run(['--bind', '127.0.0.1:7000'])).err, new RegExp(`open http://127\\.0\\.0\\.1:7000/#token=${token}\\n`));
  });

  test('leaves access control alone when it is given', async () => {
    for (const [args, env] of [[['--no-auth'], {}], [['--token=x'], {}], [['--token-file', '/t'], {}], [[], { TILT_TOKEN: 'x' }], [['--version'], {}]]) {
      const r = await run(args, env);
      assert.doesNotMatch(r.out, /--token-file \S+\/config\//, JSON.stringify(args));
      assert.doesNotMatch(r.err, /open http/);
    }
  });

  test('updates to a newer release on start and keeps two', async () => {
    await run();
    latest = '0.2.0';
    assert.match((await run()).out, /^tilt 0\.2\.0 /);
    latest = '0.10.0';
    assert.match((await run()).out, /^tilt 0\.10\.0 /);
    assert.deepEqual(versions(), ['0.10.0', '0.2.0']);
    // Up to date: no download.
    hits = [];
    await run();
    assert.deepEqual(hits, ['/releases/latest']);
  });

  test('runs the newest download when offline, or when told not to update', async () => {
    await run();
    latest = '0.2.0';
    await run();
    assert.match((await run([], { TILT_RELEASES: 'http://127.0.0.1:9/releases' })).out, /^tilt 0\.2\.0 /);
    latest = '0.3.0';
    hits = [];
    assert.match((await run([], { TILT_NO_UPDATE: '1' })).out, /^tilt 0\.2\.0 /);
    assert.deepEqual(hits, []);
  });

  test('pins a release with TILT_VERSION', async () => {
    latest = '0.3.0';
    assert.match((await run([], { TILT_VERSION: '0.1.5' })).out, /^tilt 0\.1\.5 /);
  });

  test('refuses a download whose checksum does not match', async () => {
    corrupt = true;
    const r = await run();
    assert.equal(r.code, 1);
    assert.match(r.err, /checksum mismatch/);
    corrupt = false;
    await run();
    corrupt = true;
    latest = '0.2.0';
    const fallback = await run();
    assert.match(fallback.err, /checksum mismatch\); running 0\.1\.0/);
    assert.match(fallback.out, /^tilt 0\.1\.0 /);
  });

  test('says why when there is nothing to run', async () => {
    const r = await run([], { TILT_RELEASES: 'http://127.0.0.1:9/releases' });
    assert.equal(r.code, 1);
    assert.match(r.err, /cannot find the latest tilt release/);
  });
});

test('install.sh installs tilt-live, which then runs the latest tilt', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'tilt-install-'));
  const env = {
    PATH: process.env.PATH,
    HOME: home,
    XDG_CACHE_HOME: path.join(home, 'cache'),
    XDG_CONFIG_HOME: path.join(home, 'config'),
    TILT_RELEASES: base,
    TILT_INSTALL_DIR: path.join(home, 'bin'),
  };
  const sh = (cmd, args) => new Promise((resolve) => {
    const child = spawn(cmd, args, { env });
    let out = '';
    child.stdout.on('data', (d) => { out += d; });
    child.stderr.on('data', (d) => { out += d; });
    child.on('close', (code) => resolve({ code, out }));
  });
  latest = '0.4.0';
  const installed = await sh('/bin/sh', [fileURLToPath(new URL('../../install.sh', import.meta.url))]);
  assert.equal(installed.code, 0, installed.out);
  assert.match(installed.out, /tilt 0\.4\.0 --version/);
  assert.match(installed.out, /installed \S+\/bin\/tilt-live/);
  const r = await sh(path.join(home, 'bin', 'tilt-live'), ['--display', ':3']);
  assert.equal(r.code, 7);
  assert.match(r.out, /tilt 0\.4\.0 --display :3 --token-file/);
});
