// A reverse proxy for latency tests: serves a tilt under a path prefix, as a sandbox's HTTPS
// edge or a path-routing gateway would, and delays every chunk by a fixed time each way, so
// the browser sees a link with that round trip.
//
//   node e2e/proxy.mjs [--listen 6190] [--upstream 127.0.0.1:6090] [--prefix /desk/] [--delay-ms 25]
//
// Then open http://localhost:6190/desk/#token=...; TILT_URL=http://localhost:6190/desk/ runs
// e2e/tests through it. Requests outside the prefix get 404, as on a gateway; the prefix
// without its slash (/desk) reaches the upstream's root too, as many gateways route it.
// The mock tests import startProxy.
import http from 'node:http';
import net from 'node:net';
import { Transform } from 'node:stream';
import { fileURLToPath } from 'node:url';

/** Starts the proxy; resolves to { url, close() } once it listens (`listen` 0: any free port). */
export function startProxy({ listen = 0, upstream = '127.0.0.1:6090', prefix = '/desk/', delayMs = 0 } = {}) {
  const [upHost, upPort] = upstream.split(':');
  const server = createProxy(upHost, Number(upPort), prefix, delayMs);
  return new Promise((resolve) => {
    server.listen(listen, '127.0.0.1', () => {
      resolve({
        url: `http://127.0.0.1:${server.address().port}${prefix}`,
        close: () => new Promise((r) => {
          server.close(r);
          server.closeAllConnections();
          // Upgraded (WebSocket) connections are no longer the HTTP server's to close.
          for (const s of server.tunnels) s.destroy();
        }),
        server,
      });
    });
  });
}

function createProxy(upHost, upPort, prefix, delayMs) {
  /** Passes chunks through `delayMs` later, in order. */
  const delayed = () => new Transform({
    transform(chunk, _enc, done) {
      setTimeout(() => this.push(chunk), delayMs);
      done();
    },
    flush(done) {
      setTimeout(done, delayMs);
    },
  });

  /** The upstream path for `url`, or null when it is outside the prefix. */
  function strip(url) {
    const bare = prefix.replace(/\/$/, '');
    if (url === bare || url.startsWith(`${bare}?`)) return '/' + url.slice(bare.length);
    if (!url.startsWith(prefix)) return null;
    return '/' + url.slice(prefix.length);
  }

  const tunnels = new Set();
  const server = http.createServer((req, res) => {
    const path = strip(req.url);
    if (path === null) {
      res.writeHead(404).end();
      return;
    }
    const up = http.request({ host: upHost, port: upPort, method: req.method, path, headers: req.headers }, (upRes) => {
      setTimeout(() => {
        res.writeHead(upRes.statusCode, upRes.headers);
        upRes.pipe(delayed()).pipe(res);
      }, delayMs);
    });
    up.on('error', () => res.writeHead(502).end());
    req.pipe(delayed()).pipe(up);
  });

  server.on('upgrade', (req, socket, head) => {
    const path = strip(req.url);
    if (path === null) {
      socket.end('HTTP/1.1 404 Not Found\r\n\r\n');
      return;
    }
    const up = net.connect(Number(upPort), upHost, () => {
      up.setNoDelay(true);
      const lines = [`${req.method} ${path} HTTP/1.1`];
      for (let i = 0; i < req.rawHeaders.length; i += 2) lines.push(`${req.rawHeaders[i]}: ${req.rawHeaders[i + 1]}`);
      const toUp = delayed();
      toUp.pipe(up);
      toUp.write(lines.join('\r\n') + '\r\n\r\n');
      if (head.length) toUp.write(head);
      socket.pipe(toUp);
      up.pipe(delayed()).pipe(socket);
    });
    socket.setNoDelay(true);
    tunnels.add(socket);
    tunnels.add(up);
    const close = () => {
      socket.destroy();
      up.destroy();
      tunnels.delete(socket);
      tunnels.delete(up);
    };
    up.on('error', close);
    socket.on('error', close);
    up.on('close', close);
    socket.on('close', close);
  });
  server.tunnels = tunnels;
  return server;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const arg = (name, fallback) => {
    const i = process.argv.indexOf(`--${name}`);
    return i > 0 ? process.argv[i + 1] : fallback;
  };
  const p = await startProxy({
    listen: Number(arg('listen', 6190)),
    upstream: arg('upstream', '127.0.0.1:6090'),
    prefix: arg('prefix', '/desk/'),
    delayMs: Number(arg('delay-ms', 25)),
  });
  process.stdout.write(`proxy: ${p.url} -> ${arg('upstream', '127.0.0.1:6090')}\n`);
}
