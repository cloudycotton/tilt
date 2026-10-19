//! What a session reads from and writes to: a WebSocket, or a WebTransport session (src/wt.rs)
//! that carries the same messages (docs/protocol.md section 7).
//!
//! Sessions speak in WebSocket messages either way. Over WebTransport, one client-opened
//! bidirectional stream (the control stream) carries every message but VIDEO, each framed as
//! `u32 length (LE) | u8 kind | payload`, where `length` counts the kind byte and the payload,
//! and kind is KIND_TEXT (a JSON message), KIND_BINARY (a binary message) or KIND_CLOSE
//! (`u16 code (LE) | utf8 reason`, the WebSocket close). Each VIDEO access unit goes on a
//! unidirectional stream of its own: its 18-byte header with MORE clear, then the whole access
//! unit, so a lost packet of one frame holds up neither the next frames' transfer nor cursor,
//! PONG or control messages.

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use bytes::{Buf, Bytes, BytesMut};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use wtransport::error::StreamReadError;
use wtransport::{Connection, RecvStream, SendStream, VarInt};

use crate::protocol::{self, FLAG_MORE, MSG_VIDEO, VIDEO_HEADER_LEN};

pub const KIND_TEXT: u8 = 0;
pub const KIND_BINARY: u8 = 1;
pub const KIND_CLOSE: u8 = 2;

/// Close code sent in a KIND_CLOSE without one, as WebSocket reports a close without a code.
const NO_STATUS: u16 = 1005;
/// Bytes read from the control stream at a time.
const READ_CHUNK: usize = 16 * 1024;

/// The sending half of a session's connection.
pub enum Sink {
    Ws(SplitSink<WebSocket, Message>),
    Wt(WtSink),
}

impl Sink {
    pub async fn send(&mut self, msg: Message) -> anyhow::Result<()> {
        match self {
            Sink::Ws(s) => Ok(s.send(msg).await?),
            Sink::Wt(s) => s.send(msg).await,
        }
    }

    /// Flushes what was sent and ends the connection: for a WebSocket its closing handshake,
    /// for WebTransport the control stream's end, acknowledged, and then the QUIC connection.
    pub async fn close(&mut self) -> anyhow::Result<()> {
        match self {
            Sink::Ws(s) => Ok(s.close().await?),
            Sink::Wt(s) => s.close().await,
        }
    }
}

/// The receiving half of a session's connection.
pub enum Stream {
    Ws(SplitStream<WebSocket>),
    Wt(WtStream),
}

impl Stream {
    /// The next message; None once the peer has finished. Cancel-safe: a message is never
    /// half-consumed by a dropped call.
    pub async fn next(&mut self) -> Option<anyhow::Result<Message>> {
        match self {
            Stream::Ws(s) => s.next().await.map(|r| r.map_err(Into::into)),
            Stream::Wt(s) => s.next().await,
        }
    }
}

/// Writes a session's messages to a WebTransport session.
pub struct WtSink {
    conn: Connection,
    control: SendStream,
    /// The stream of an access unit whose remaining fragments are still to come.
    video: Option<SendStream>,
    /// Each video stream gets a lower priority than the one before, so QUIC sends (and resends)
    /// the oldest frame first, as the decoder needs them, rather than interleaving them.
    video_priority: i32,
}

impl WtSink {
    /// `control` is the session's control stream, which goes ahead of all video.
    pub fn new(conn: Connection, control: SendStream) -> WtSink {
        control.set_priority(i32::MAX);
        WtSink {
            conn,
            control,
            video: None,
            video_priority: 0,
        }
    }

    async fn send(&mut self, msg: Message) -> anyhow::Result<()> {
        match msg {
            Message::Text(t) => self.frame(KIND_TEXT, t.as_bytes()).await,
            Message::Binary(b) if b.first() == Some(&MSG_VIDEO) => self.video(&b).await,
            Message::Binary(b) => self.frame(KIND_BINARY, &b).await,
            Message::Close(frame) => {
                let (code, reason) =
                    frame.map_or((NO_STATUS, Default::default()), |f| (f.code, f.reason));
                let mut payload = Vec::with_capacity(2 + reason.len());
                payload.extend_from_slice(&code.to_le_bytes());
                payload.extend_from_slice(reason.as_bytes());
                self.frame(KIND_CLOSE, &payload).await
            }
            Message::Ping(_) | Message::Pong(_) => Ok(()),
        }
    }

    async fn frame(&mut self, kind: u8, payload: &[u8]) -> anyhow::Result<()> {
        self.control.write_all(&frame(kind, payload)).await?;
        Ok(())
    }

    /// One VIDEO message: the first fragment of an access unit opens its stream, the last one
    /// finishes it. Finishing does not wait for the peer's acknowledgement.
    async fn video(&mut self, msg: &[u8]) -> anyhow::Result<()> {
        let more = protocol::video_has_more(msg);
        let mut stream = match self.video.take() {
            Some(mut stream) => {
                stream
                    .write_all(msg.get(VIDEO_HEADER_LEN..).unwrap_or_default())
                    .await?;
                stream
            }
            None => {
                let mut stream = self.conn.open_uni().await?.await?;
                stream.set_priority(self.video_priority);
                self.video_priority = self.video_priority.checked_sub(1).unwrap_or(0);
                // The stream holds the whole access unit, so the header's MORE is clear.
                let mut first = msg.to_vec();
                if let Some(flags) = first.get_mut(1) {
                    *flags &= !FLAG_MORE;
                }
                stream.write_all(&first).await?;
                stream
            }
        };
        if more {
            self.video = Some(stream);
        } else {
            // Errs only on a stream already finished or reset; the data is queued either way.
            let _ = stream.quic_stream_mut().finish();
        }
        Ok(())
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        // Ends the control stream and waits until the peer has all of it (a close message
        // included): closing the QUIC connection would throw away anything unacknowledged.
        let flushed = self.control.finish().await;
        self.conn.close(VarInt::from_u32(0), b"");
        Ok(flushed?)
    }
}

impl Drop for WtSink {
    fn drop(&mut self) {
        // A session that ends without `close` (a write timeout) still ends its connection.
        self.conn.close(VarInt::from_u32(0), b"");
    }
}

/// Reads a session's messages from its WebTransport control stream.
pub struct WtStream {
    recv: RecvStream,
    buf: BytesMut,
    max_msg: usize,
}

impl WtStream {
    /// Messages longer than `max_msg` bytes end the session, as they do on a WebSocket.
    pub fn new(recv: RecvStream, max_msg: usize) -> WtStream {
        WtStream {
            recv,
            buf: BytesMut::new(),
            max_msg,
        }
    }

    async fn next(&mut self) -> Option<anyhow::Result<Message>> {
        loop {
            match parse_frame(&mut self.buf, self.max_msg) {
                Ok(Some(msg)) => return Some(Ok(msg)),
                Ok(None) => {}
                Err(e) => return Some(Err(e)),
            }
            let mut chunk = [0u8; READ_CHUNK];
            // RecvStream::read is cancel-safe: no data is consumed by a call that is dropped.
            match self.recv.read(&mut chunk).await {
                Ok(Some(n)) => self.buf.extend_from_slice(&chunk[..n]),
                Ok(None) if self.buf.is_empty() => return None,
                Ok(None) => {
                    return Some(Err(anyhow::anyhow!(
                        "the control stream ended inside a message"
                    )))
                }
                Err(StreamReadError::Reset(_)) if self.buf.is_empty() => return None,
                Err(e) => return Some(Err(e.into())),
            }
        }
    }
}

/// One control-stream frame.
pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len() + 1).expect("messages are far below 4 GiB");
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

/// Takes the first whole frame off `buf`; None until one has arrived.
fn parse_frame(buf: &mut BytesMut, max_msg: usize) -> anyhow::Result<Option<Message>> {
    let Some(len) = buf.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(len.try_into().expect("4 bytes")) as usize;
    anyhow::ensure!(
        (1..=max_msg + 1).contains(&len),
        "control-stream message of {len} bytes is outside 1..={}",
        max_msg + 1
    );
    if buf.len() < 4 + len {
        return Ok(None);
    }
    buf.advance(4);
    let kind = buf.get_u8();
    let payload = buf.split_to(len - 1).freeze();
    Ok(Some(match kind {
        KIND_TEXT => Message::Text(String::from_utf8(payload.to_vec())?.into()),
        KIND_BINARY => Message::Binary(payload),
        KIND_CLOSE => Message::Close(close_frame(&payload)?),
        k => anyhow::bail!("unknown control-stream message kind {k}"),
    }))
}

fn close_frame(payload: &Bytes) -> anyhow::Result<Option<CloseFrame>> {
    let Some(code) = payload.get(..2) else {
        return Ok(None);
    };
    let code = u16::from_le_bytes(code.try_into().expect("2 bytes"));
    let reason = std::str::from_utf8(&payload[2..])?;
    Ok(Some(CloseFrame {
        code,
        reason: reason.to_owned().into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(bytes: &[u8], max: usize) -> Vec<anyhow::Result<Option<Message>>> {
        let mut buf = BytesMut::from(bytes);
        let mut out = Vec::new();
        loop {
            let r = parse_frame(&mut buf, max);
            let stop = !matches!(r, Ok(Some(_)));
            out.push(r);
            if stop {
                return out;
            }
        }
    }

    #[test]
    fn frames_round_trip_and_wait_for_whole_messages() {
        let mut wire = frame(KIND_TEXT, br#"{"t":"hello","v":1}"#);
        wire.extend(frame(KIND_BINARY, &[0x11, 1, 2, 3, 4]));
        wire.extend(frame(KIND_CLOSE, &[0xe9, 0x03, b'o', b'k']));
        // Byte by byte: nothing is returned until a frame is whole, and nothing is lost.
        let mut buf = BytesMut::new();
        let mut got = Vec::new();
        for b in &wire {
            buf.extend_from_slice(&[*b]);
            if let Some(m) = parse_frame(&mut buf, 64).unwrap() {
                got.push(m);
            }
        }
        assert!(buf.is_empty());
        assert_eq!(
            got,
            [
                Message::Text(r#"{"t":"hello","v":1}"#.into()),
                Message::Binary(Bytes::from_static(&[0x11, 1, 2, 3, 4])),
                Message::Close(Some(CloseFrame {
                    code: 1001,
                    reason: "ok".into()
                })),
            ]
        );
    }

    #[test]
    fn bad_frames_are_errors() {
        // Too long, empty, unknown kind, text that is not UTF-8.
        assert!(parse_all(&frame(KIND_BINARY, &[0; 65]), 64)[0].is_err());
        assert!(parse_all(&frame(KIND_BINARY, &[0; 64]), 64)[0].is_ok());
        assert!(parse_all(&[0, 0, 0, 0], 64)[0].is_err());
        assert!(parse_all(&frame(9, b"x"), 64)[0].is_err());
        assert!(parse_all(&frame(KIND_TEXT, &[0xff]), 64)[0].is_err());
        // A close without a code is a close without a frame, as on a WebSocket.
        let r = parse_all(&frame(KIND_CLOSE, &[]), 64);
        assert!(matches!(r[0], Ok(Some(Message::Close(None)))));
    }
}
