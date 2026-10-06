# tilt wire protocol, version 1

This is the contract between the tilt server and any client: the bundled web page, a native
WebView or a test probe. The server's implementation is `src/protocol.rs` and `src/session.rs`.

All binary integers are little-endian. Every WebSocket message is either:
- binary: the first byte is the message type;
- text: a JSON object with a string field `t`.

## 1. Handshake

1. The client opens `ws(s)://<host>/<base>/stream` with no subprotocol: `stream` resolved
   against the page's URL (a native client: against the base URL it was given), with the
   scheme switched, as the web client does:
   ```js
   const url = new URL('stream', location.href);
   url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
   url.hash = '';
   ```
   This keeps any path prefix (`https://host/desktop/` → `wss://host/desktop/stream`) and uses
   `ws:` for a plain-HTTP server such as `http://localhost:6090`.

   With `--no-auth` the upgrade is refused (HTTP 403) when the request has an `Origin` header
   whose host and port are not those of its `Host` header (or of an `X-Forwarded-Host` or
   `Forwarded: host=` added by a proxy that rewrites `Host`) and that is not listed in
   `--allow-origin` (`*` allows any): everyone who connects gets control, and browsers do not
   stop other sites' pages from opening WebSockets. Clients that send no `Origin` (probes,
   native apps) are unaffected, and token modes do not check `Origin`.
2. Within 5 s the client sends the text message
   `{"t":"hello","v":1,"token":"<token>","client":"tilt-web/0.1"}`.
   The `token` field may be omitted only in no-auth mode. Tokens never go in the query string;
   the web client reads them from the URL fragment (`#token=`).
3. If `v != 1`, the server replies `{"t":"error","code":"version","msg":...}` and closes.
4. If the token is wrong, the server sleeps 500 ms, then sends `{"t":"error","code":"auth"}` and
   closes.
5. If viewers already number `--max-viewers`, the server sends `{"t":"error","code":"busy"}` and
   closes, unless one of those sessions has sent nothing for over 5 s: then the longest-silent
   one is closed (1000) and the new one admitted. If the token file is missing or holds no
   token yet, it sends `{"t":"error","code":"not_ready"}` and closes.
6. On success the server sends, in this order:
   - `{"t":"welcome","v":1,"session":"s7","role":"control"|"view","screen":{"w":W,"h":H},"server":"tilt/0.1.0"}`
   - `{"t":"control","you":bool,"held":bool,"holder":"s3"}` (see section 4)
   - CURSOR_SHAPE and CURSOR_POS, if known
   - then video, starting with a KEY frame.
7. Close codes:

| code | meaning |
|---|---|
| 1000 | normal; also sent after 30 s without any client message (reason `idle`), and to a session silent for over 5 s whose place a new client takes (see 1.5) |
| 1001 | the server is shutting down |
| 1008 | more than 100 malformed messages |
| 1011 | server-side failure (the session's encoder thread could not start or stopped) |
| 4001 | auth: wrong or missing token, or the first message was not `hello` |
| 4002 | busy: `--max-viewers` sessions are already connected |
| 4003 | version: `v` is not 1 |
| 4004 | not_ready: the token file is missing or holds no token yet |
| 4005 | hello timeout: no `hello` within 5 s |

Codes 1001, 1008 and 1011 are standard WebSocket codes the server uses beyond the 4xxx set;
clients should treat any close as "reconnect with backoff", except 4001 and 4003, which will
not succeed on retry. A 4004 means the token file has not been written yet: retrying later is
expected to work.

## 2. Server to client, binary

| type | name | layout after the type byte |
|---|---|---|
| 0x01 | VIDEO | `u8 flags, u32 seq, u64 capture_us, u16 width, u16 height, payload…` (18-byte header including the type) |
| 0x02 | CURSOR_SHAPE | `u32 serial, u16 w, u16 h, u16 xhot, u16 yhot, w*h*4 bytes RGBA (straight alpha)`; w=h=0 means hidden |
| 0x03 | CURSOR_POS | `i16 x, i16 y` (screen pixels) |
| 0x04 | PONG | `u32 t` (echo of PING.t) |

VIDEO fields:
- **flags:**
  - bit0 KEY: an IDR, with SPS+PPS present in the payload.
  - bit1 MORE: another fragment of the same seq follows.
- **seq:** a per-session counter, starting at 1 and incremented once per access unit, not per
  fragment. It is a u32 and wraps (skipping 0).
- **capture_us:** the server's monotonic µs at capture. It is informational.
- **width, height:** the coded size, which is the even-cropped screen size.
- **payload:** Annex B (00 00 00 01 start codes), one access unit per seq.
- **Codec:** H.264 High profile with CABAC (`--profile high`, the default) or Constrained
  Baseline (`--profile baseline`). The SPS signals a level that covers the screen size at
  `--max-fps` and `--max-bitrate-kbps`, so it holds however the bitrate adapts: with the
  defaults, 1920×1080 is level 4.2 and 1280×720 or 1024×768 level 3.2. A WebCodecs client
  builds its codec string from the three bytes after the SPS NAL header (profile_idc,
  constraint flags, level_idc): `avc1.640C2A` for 1920×1080 with High, `avc1.42C02A` with
  Baseline. The level follows the screen size, so take it from every KEY frame's SPS rather
  than from the first.
- **Fragmentation:** an access unit larger than `--max-msg-bytes` (default 65536, header
  included) is split into several VIDEO messages with identical headers. MORE is set on every
  fragment but the last. Fragments of one seq are always contiguous on the socket: no other
  message (cursor, PONG, JSON) is sent between them. Reassemble by concatenating payloads until
  a fragment without MORE arrives.

Cursor messages: CURSOR_SHAPE is sent when the cursor image changes (new serial), CURSOR_POS
when the pointer moves. Captured frames never contain the cursor; draw it from these.
The shape is at most 128×128.

## 3. Client to server, binary

| type | name | layout after the type byte |
|---|---|---|
| 0x10 | ACK | `u32 seq, u16 decode_ms` — sent once per VIDEO seq when the decoder outputs it (or discards it); decode_ms = ms from last-fragment receipt to output, saturating. Acks are cumulative: ACK(n) acks every seq ≤ n. |
| 0x11 | PING | `u32 t` (client ms clock, wraps) — every 1 s while connected; doubles as the heartbeat |
| 0x20 | MOVE | `u16 x, u16 y` — normalized: 0 = left/top edge pixel, 65535 = right/bottom edge pixel |
| 0x21 | BUTTON | `u8 button, u8 down, u16 x, u16 y` — X button numbers 1 left, 2 middle, 3 right, 8 back, 9 forward |
| 0x22 | WHEEL | `i16 dx, i16 dy, u16 x, u16 y` — discrete notches; +dy = down (button 5), −dy = up (4), +dx = right (7), −dx = left (6) |
| 0x30 | KEY | `u8 down, u32 keysym` — a `down` for an already-down keysym means autorepeat |
| 0x31 | TEXT | `utf8 bytes…` (rest of message) — type this text: '\n' → Return, '\t' → Tab, otherwise per-codepoint keysym. At most 4096 bytes are typed: the server cuts longer text at the last character boundary before that and drops the rest (with a warning in its log). Send longer text as several messages. It is typed literally: the Control, Alt, Meta, Super or Hyper keys this session holds are released for the whole text and pressed again after it, and its own Shift or AltGr is lifted for characters that must not have it. Losing control, disconnecting or RELEASE_ALL stops the rest. |
| 0x32 | RELEASE_ALL | (no payload) — release everything this session holds, and stop the TEXT it is still typing along with everything the session sent after that TEXT |

Every fixed-size message must have exactly the listed length; TEXT must be valid UTF-8. Anything
else counts as malformed (see section 5).

Coordinate mapping, done on the server against the current screen size W×H:
- `px = round(x * (W-1) / 65535)`, and likewise for y.
- The client computes `x = round(fx * 65535)`, where `fx = (clientX - rect.left) / rect.width`,
  clamped to [0,1].

Input from a session that does not hold control is silently dropped, as is input from a
view-role session. ACK and PING are always processed.

Flow control depends on ACKs: the server only has a few frames in flight per session (3 to 32,
scaled with the round trip) and encodes nothing more until they are acknowledged. A client that
stops acking stops receiving video. When nothing has moved for 5 s, neither an ACK arrived nor
VIDEO was written to the socket, the server forgets the frames in flight and goes on; after
each such reset with no ACK since, the next wait doubles, up to 20 s. It sends no KEY frame for
this (TCP lost nothing): a client that needs one asks with `{"t":"idr"}`. A late ACK for a
forgotten frame is harmless. Never drop a VIDEO seq without acking it. An ACK for a seq the
server has not sent yet counts as malformed.

## 4. JSON (text) messages

Client to server:
- `{"t":"control","take":true|false}`: take or release control. A view-role session gets
  `{"t":"error","code":"forbidden"}` and is not closed.
- `{"t":"idr"}`: request a keyframe, for example after a decoder error. The server honours it
  subject to the 500 ms rate limit; a request that arrives during the limit is deferred, not
  dropped.
- `{"t":"video","on":false|true}`: pause or resume video, for example when the page is hidden.
  Pausing forgets the frames in flight (ACKs for them are still accepted, and teach the server
  nothing). Resume forces an IDR.

Server to client:
- `{"t":"control","you":bool,"held":bool,"holder":"s3"}`: sent on any change. `held` is true if
  any session holds control, and `holder` is that session's id as its `welcome` gave it; the
  field is absent when nobody holds control. A client that lost its connection while holding
  control can tell its own lost session (which keeps control until the server closes it, within
  30 s) from another viewer: it may take control back at once from the former, never from the
  latter. A server run never reuses a session id, and each run starts numbering at a random point,
  so a restarted server is unlikely (one chance in a million) to give a new session the id of a
  lost one. Older servers send no `holder`.
- `{"t":"screen","w":W,"h":H}`: sent when the screen size changed. A KEY frame at the new size
  follows. VIDEO width/height are the even-cropped values of W and H.
- `{"t":"stats","fps":f,"kbps":f,"bitrate_kbps":n,"rtt_ms":f,"min_rtt_ms":f,"queue_ms":f,"enc_ms":f,"inflight":n,"skipped":n,"viewers":n,"gov_fps":n,"qp":f}`:
  sent every 1 s while video is on. `fps`, `kbps` and `enc_ms` cover the last second; `skipped`
  counts captures this session never encoded, over the whole session. `gov_fps` is the frame
  rate cap in force (`--max-fps` unless the server's CPU governor lowered it); `qp` is the mean
  quantizer of the last second's frames, 0 when the server does not report it. Older servers
  send neither.
- `{"t":"error","code":"auth"|"busy"|"version"|"not_ready"|"forbidden"|"hello_timeout"|"unsupported_size","msg":"…"}`.
  `unsupported_size` means the encoder cannot take the screen's size (smaller than 16×16, or
  more than 36864 macroblocks, about 4096×2304). The session stays open without video; when the
  screen changes to a size that works, `screen` and a KEY frame follow.

Unknown fields in client JSON are ignored. A repeated `hello` after the handshake is ignored.

## 5. Liveness

- **Client:**
  - Send PING every 1 s.
  - Declare the link dead after 5 s without any server message, provided a PONG was expected.
  - Reconnect with jittered backoff of 250 ms, 500 ms, 1 s, 2 s, then capped at 5 s.
  - On reconnect, show the last frame under a "Reconnecting…" overlay.
- **Server:**
  - If a controlling session sends nothing for 3 s while it holds keys or buttons, release them.
    The session stays open.
  - If a session sends nothing for 30 s, close it (1000, `idle`).
  - When `--max-viewers` sessions are open and a new client's `hello` succeeds, close the one
    that has sent nothing for longest, if that is over 5 s (1000), and admit the new client.
  - Close a session whose WebSocket write blocks for more than 10 s.
  - TCP keepalive: first probe after 10 s without traffic, then 3 probes 5 s apart, so a peer
    that vanished without closing is dropped after about 25 s.
  - More than 100 malformed messages in one session close it (1008).

## 6. HTTP endpoints on the same port

| path | |
|---|---|
| `GET /` | the web client (`index.html`); `/app.js`, `/input.js`, `/keysyms.js` alongside. Strong ETag, `Cache-Control: no-cache`, `If-None-Match` answered with 304. |
| `GET /healthz` | `ok`, no auth |
| `GET /api/status` | `{"version","screen":{"w","h"},"viewers","controller":bool,"sessions":[{"id","role","control","fps","kbps","rtt_ms"}]}`; needs `Authorization: Bearer <control token>` unless `--no-auth` (401 wrong token, 403 view token, 503 token file missing or empty) |
| `GET /stream` | the WebSocket above; incoming messages are limited to 64 KiB; with `--no-auth`, 403 for another site's `Origin` (see 1.1) |
