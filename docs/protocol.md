# tilt wire protocol, version 1

This is the contract between the tilt server and any client: the bundled web page, a native
WebView or a test probe. The server's implementation is `src/protocol.rs` and `src/session.rs`.

**In short:** fetch `transport` next to the page, open the WebTransport session it names, send
`{"t":"hello","v":1,"token":"…"}` on a bidirectional stream, then receive H.264 frames (one per
unidirectional stream; decode them with WebCodecs) and send ACKs, a PING every second, and mouse
and key events back. The rest of this page is the exact contract.

All binary integers are little-endian. Every message is either:
- binary: the first byte is the message type;
- text: a JSON object with a string field `t`.

## 1. Connecting

1. **Discovery.** `GET transport` next to the page (`/transport`, or `/<prefix>/transport`)
   returns `{"wt":{"host":"…","port":N,"hashes":["<sha-256 hex>"],"ticket":"…"}}`: where the
   stream's UDP goes (`host` only when it isn't the page's own host; `port` by default the HTTP
   port), the certificate's hash, and a ticket.
2. **Session.** Open `https://<host>:<port>/stream?ticket=<ticket>` with WebTransport, where
   `host` is the advert's `host`, else the page's host (`127.0.0.1` for `localhost`: Chrome dials
   `localhost` over QUIC on `::1` only), with `serverCertificateHashes: [{algorithm: 'sha-256', value: <hash bytes>}]`
   for each of `hashes`. The certificate is self-signed ECDSA P-256, valid for 14 days (from a
   day before it was made) and replaced every 6 days, so fetch `transport` before each
   connection. A request without the right ticket gets HTTP 403: other sites' pages cannot read
   `transport`, so a session with the ticket comes from a page this server served. Unknown
   paths get 404.
3. **Control stream.** The client opens one bidirectional stream. Every message except VIDEO
   travels on it, in both directions, as `u32 length (LE) | u8 kind | payload`, `length`
   counting the kind byte and the payload:

   | kind | payload |
   |---|---|
   | 0 | a JSON (text) message, UTF-8 |
   | 1 | a binary message (type byte first) |
   | 2 | close: `u16 code (LE)`, then the UTF-8 reason; the codes of 1.10 |

   The server sends a kind-2 close before it ends a session, then finishes the stream and closes
   the QUIC connection once the client has acknowledged it. A client that reads the end of the
   stream or loses the connection without a close treats it as an abnormal close (1006) and
   reconnects. Messages longer than 64 KiB + 1 end the session.
4. **Video.** Each access unit arrives on its own server-opened unidirectional stream: one VIDEO
   message (section 2), then the end of the stream. Streams may complete out of order: decode in
   `seq` order, starting at 1. If a stream fails before its header was read, skip to the oldest
   seq that did arrive, ACK the last skipped seq (ACKs are cumulative) and request an IDR. VIDEO
   may arrive before the `welcome` on the control stream: hold it until then. A lost packet
   delays only the frame it belongs to, never cursor updates, PONGs, input or the frames after
   it. The server uses BBR congestion control; the ACK credit of section 3 decides how much
   video is in flight.
5. Within 5 s the client sends, on the control stream, the text message
   `{"t":"hello","v":1,"token":"<token>","client":"tilt-web/0.1"}`.
   The `token` field may be omitted only in no-auth mode. Tokens never go in a URL; the web
   client reads them from the page's URL fragment (`#token=`).
6. If `v != 1`, the server replies `{"t":"error","code":"version","msg":...}` and closes.
7. If the token is wrong, the server sleeps 500 ms, then sends `{"t":"error","code":"auth"}` and
   closes.
8. There is no limit on viewers: any number of sessions may watch, and control (section 4)
   is what only one holds. If the token file is missing or holds no token yet, the server
   sends `{"t":"error","code":"not_ready"}` and closes. (Servers before 0.4 had a viewer limit
   and answered `{"t":"error","code":"busy"}`, closing with 4002; clients should retry then.)
9. On success the server sends, in this order:
   - `{"t":"welcome","v":1,"session":"s7","role":"control"|"view","screen":{"w":W,"h":H},"server":"tilt/0.1.0","resize":bool}`
   - `{"t":"control","you":bool,"held":bool,"holder":"s3"}` (see section 4)
   - CURSOR_SHAPE and CURSOR_POS, if known
   - then video, starting with a KEY frame.
10. Close codes:

| code | meaning |
|---|---|
| 1000 | normal; also sent after 30 s without any client message (reason `idle`) |
| 1001 | the server is shutting down |
| 1008 | more than 100 malformed messages |
| 1011 | server-side failure (the session's encoder thread could not start or stopped) |
| 4001 | auth: wrong or missing token, or the first message was not `hello` |
| 4002 | busy: only from servers before 0.4, which had a viewer limit |
| 4003 | version: `v` is not 1 |
| 4004 | not_ready: the token file is missing or holds no token yet |
| 4005 | hello timeout: no `hello` within 5 s |

Codes 1000, 1001, 1008 and 1011 follow their WebSocket meanings; clients should treat any close as "reconnect with backoff", except 4001 and 4003, which will
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
  - bit2 PATCH: the payload is a patch (below), not H.264.
- **seq:** a per-session counter, starting at 1 and incremented once per access unit. It is a
  u32 and wraps (skipping 0).
- **capture_us:** the server's monotonic µs at capture. It is informational.
- **width, height:** the coded size, which is the even-cropped screen size.
- **payload:** Annex B (00 00 00 01 start codes), one access unit per seq; or, with PATCH, a
  patch.
- **Codec:** H.264 High profile with CABAC (`--profile high`, the default) or Constrained
  Baseline (`--profile baseline`). The SPS signals a level that covers the screen size at
  `--max-fps` and `--max-bitrate-kbps`, so it holds however the bitrate adapts: with the
  defaults, 1920×1080 is level 4.2 and 1280×720 or 1024×768 level 3.2. A WebCodecs client
  builds its codec string from the three bytes after the SPS NAL header (profile_idc,
  constraint flags, level_idc): `avc1.640C2A` for 1920×1080 with High, `avc1.42C02A` with
  Baseline. The level follows the screen size, so take it from every KEY frame's SPS rather
  than from the first.

Patches: a small change (a typed character, a blinking caret, a hover) is sent as the changed
pixels instead of an H.264 frame, which would cost the server a full-screen encode. A patch is
`u16 count`, then per rectangle `u16 x, u16 y, u16 w, u16 h, u32 len` and `len` bytes: the
rectangle's I420 pixels (Y rows, then U rows, then V rows; x, y, w and h are even),
run-length coded. In the code, a byte c < 128 is followed by c + 1 literal bytes, and c ≥ 128
by one byte that repeats c − 125 times. Runs can cross rows and planes. The pixels use the
stream's colours (BT.709, limited range), so draw them as an I420 VideoFrame with that colour
space. The decoder never sees a patch, and the next H.264 frame codes everything since the one
before, patched areas included. A patch never comes before the session's first KEY frame.
- **Order:** draw updates in seq order. A patch goes over the frame before it, so a patch that
  arrives while an earlier frame is still in the decoder waits for that frame. Once a frame is
  drawn, patches with a lower seq are covered by it and can be skipped.
- **ACK:** a patch is acked once it is drawn (or skipped), in seq order like frames.
  decode_ms is the time from receipt to drawing.

Cursor messages: CURSOR_SHAPE is sent when the cursor image changes (new serial), CURSOR_POS
when the pointer moves. Captured frames never contain the cursor; draw it from these.
The shape is at most 128×128.

## 3. Client to server, binary

| type | name | layout after the type byte |
|---|---|---|
| 0x10 | ACK | `u32 seq, u16 decode_ms` — sent once per VIDEO seq when the decoder outputs it (or discards it), or for a patch when it is drawn; decode_ms = ms from receipt to output, saturating. Acks are cumulative: ACK(n) acks every seq ≤ n. |
| 0x11 | PING | `u32 t` (client ms clock, wraps) — every 1 s while connected; doubles as the heartbeat |
| 0x20 | MOVE | `u16 x, u16 y` — normalized: 0 = left/top edge pixel, 65535 = right/bottom edge pixel |
| 0x21 | BUTTON | `u8 button, u8 down, u16 x, u16 y` — X button numbers 1 left, 2 middle, 3 right, 8 back, 9 forward |
| 0x22 | WHEEL | `i16 dx, i16 dy, u16 x, u16 y` — discrete notches; +dy = down (button 5), −dy = up (4), +dx = right (7), −dx = left (6) |
| 0x30 | KEY | `u8 down, u32 keysym` — a `down` for an already-down keysym means autorepeat |
| 0x31 | TEXT | `utf8 bytes…` (rest of message) — type this text: '\n' → Return, '\t' → Tab, otherwise per-codepoint keysym. At most 4096 bytes are typed: the server cuts longer text at the last character boundary before that and drops the rest (with a warning in its log). Send longer text as several messages. It is typed literally: the Control, Alt, Meta, Super or Hyper keys this session holds are released for the whole text and pressed again after it, and its own Shift or AltGr is lifted for characters that must not have it. Losing control, disconnecting or RELEASE_ALL stops the rest. |
| 0x32 | RELEASE_ALL | (no payload) — release everything this session holds, and stop the TEXT it is still typing along with everything the session sent after that TEXT |
| 0x33 | CLIPBOARD | `utf8 bytes…` (rest of message, at most 65535 bytes) — make this text the X CLIPBOARD selection's content, tilt owning it, before any input sent after it runs: send it just before a paste key (Ctrl+V), so the application pastes it. Offered as UTF8_STRING, `text/plain;charset=utf-8`, TEXT and STRING (Latin-1, `?` for other characters). |

Every fixed-size message must have exactly the listed length; TEXT and CLIPBOARD must be valid UTF-8. Anything
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
VIDEO was written, the server forgets the frames in flight and goes on; after each such reset
with no ACK since, the next wait doubles, up to 20 s. It sends no KEY frame for this (streams
lose nothing): a client that needs one asks with `{"t":"idr"}`. A late ACK for a
forgotten frame is harmless. Never drop a VIDEO seq without acking it. An ACK for a seq the
server has not sent yet counts as malformed.

## 4. JSON (text) messages

Client to server:
- `{"t":"control","take":true|false}`: take or release control. A view-role session gets
  `{"t":"error","code":"forbidden"}` and is not closed.
- `{"t":"idr"}`: request a keyframe, for example after a decoder error. The server honours it
  subject to the 500 ms rate limit; a request that arrives during the limit is deferred, not
  dropped.
- `{"t":"resize","w":W,"h":H}`: with `--resize MIN`, the viewer's window in CSS pixels. The
  screen takes that shape, scaled up to at least MIN on each side and down into the largest
  screen the X server allows, with even sides; `screen` and a KEY frame follow. Allowed when the
  `welcome` says `"resize":true` (control role, server started with `--resize`) and nobody else
  holds control; otherwise ignored, or `forbidden` for a session that may not send it. The web
  client sends it after its welcome and whenever its window has kept a new size for 300 ms.
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
  lost one.
- `{"t":"screen","w":W,"h":H}`: sent when the screen size changed. A KEY frame at the new size
  follows. VIDEO width/height are the even-cropped values of W and H.
- `{"t":"stats","fps":f,"kbps":f,"bitrate_kbps":n,"rtt_ms":f,"min_rtt_ms":f,"queue_ms":f,"enc_ms":f,"inflight":n,"skipped":n,"viewers":n,"gov_fps":n,"qp":f,"patches":n}`:
  sent every 1 s while video is on. `fps`, `kbps` and `enc_ms` cover the last second; `skipped`
  counts captures this session never encoded, over the whole session. `gov_fps` is the frame
  rate cap in force (`--max-fps`); `qp` is the mean quantizer of the last second's frames, 0
  when there were none. `patches` is how many of the last second's `fps` were patches; `enc_ms`
  and `qp` cover the H.264 frames only.
- `{"t":"clipboard","text":"…"}`: an application on the display copied this text (the CLIPBOARD
  selection changed hands), sent only to the session holding control, as it happens. Only text
  that converts to UTF8_STRING in one go, at most 65535 bytes, is sent; other content (images,
  larger text) is not. Text CLIPBOARD set is not sent back.
- `{"t":"error","code":"auth"|"version"|"not_ready"|"forbidden"|"hello_timeout"|"unsupported_size","msg":"…"}`.
  `unsupported_size` means the encoder cannot take the screen's size (smaller than 16×16, or
  more than 36864 macroblocks, about 4096×2304). The session stays open without video; when the
  screen changes to a size that works, `screen` and a KEY frame follow.

Unknown fields in client JSON are ignored. A repeated `hello` after the handshake is ignored.

## 5. Liveness

- **Client:**
  - Send PING every 1 s.
  - Declare the link dead after 5 s without any server message, provided a PONG was expected.
  - Reconnect with jittered backoff of 250 ms, 500 ms, 1 s, 2 s, then capped at 5 s.
  - On reconnect, keep showing the last frame until the new session's first one.
- **Server:**
  - If a controlling session sends nothing for 3 s while it holds keys or buttons, release them.
    The session stays open.
  - If a session sends nothing for 30 s, close it (1000, `idle`).
  - Close a session whose write blocks for more than 10 s.
  - QUIC keep-alive every 5 s; a peer silent for 25 s (vanished without closing) is dropped.
  - More than 100 malformed messages in one session close it (1008).

## 6. HTTP endpoints (TCP, same port)

| path | |
|---|---|
| `GET /` | the web client (`index.html`); `/app.js`, `/input.js`, `/transport.js`, `/keysyms.js` alongside. Strong ETag, `Cache-Control: no-cache`, `If-None-Match` answered with 304. |
| `GET /healthz` | `ok`, no auth |
| `GET /api/status` | `{"version","screen":{"w","h"},"viewers","controller":bool,"sessions":[{"id","role","control","fps","kbps","rtt_ms"}]}`; needs `Authorization: Bearer <control token>` unless `--no-auth` (401 wrong token, 403 view token, 503 token file missing or empty) |
| `GET /transport` | `{"wt":{"host":"…","port":N,"hashes":["<sha-256 hex>"],"ticket":"…"}}` (1.1; `host` only when set by `--udp-public`); no auth, `Cache-Control: no-store` |
