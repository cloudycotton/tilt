//! A session's connection: a WebTransport session (src/wt.rs, docs/protocol.md).
//!
//! One client-opened bidirectional stream (the control stream) carries every message but VIDEO,
//! each framed as `u32 length (LE) | u8 kind | payload`, where `length` counts the kind byte and
//! the payload, and kind is KIND_TEXT (a JSON message), KIND_BINARY (a binary message) or
//! KIND_CLOSE (`u16 code (LE) | utf8 reason`). Each VIDEO access unit goes on a unidirectional
//! stream of its own: its 18-byte header, then the whole access unit, so a lost packet of one
//! frame holds up neither the next frames' transfer nor cursor, PONG or control messages.

use std::time::Instant;

use bytes::{Buf, Bytes, BytesMut};
use wtransport::error::StreamReadError;
use wtransport::{Connection, RecvStream, SendStream, VarInt};

use crate::protocol::MSG_VIDEO;
use crate::recovery::{self, Recovery};

pub const KIND_TEXT: u8 = 0;
pub const KIND_BINARY: u8 = 1;
pub const KIND_CLOSE: u8 = 2;

/// Close code sent in a KIND_CLOSE without one.
const NO_STATUS: u16 = 1005;
/// Bytes read from the control stream at a time.
const READ_CHUNK: usize = 16 * 1024;

/// One message of the protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Bytes),
    Close(Option<CloseFrame>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: u16,
    pub reason: String,
}

/// Writes a session's messages.
pub struct Sink {
    conn: Connection,
    control: SendStream,
    /// Each video stream gets a lower priority than the one before, so QUIC sends (and resends)
    /// the oldest frame first, as the decoder needs them, rather than interleaving them.
    video_priority: i32,
    recovery: Option<Recovery>,
}

impl Sink {
    /// `control` is the session's control stream, which goes ahead of all video.
    pub fn new(conn: Connection, control: SendStream) -> Sink {
        control.set_priority(i32::MAX);
        Sink {
            conn,
            control,
            video_priority: 0,
            recovery: None,
        }
    }

    pub fn enable_recovery(&mut self) {
        self.recovery = Some(Recovery::new(Instant::now()));
    }

    pub async fn send(&mut self, msg: Message) -> anyhow::Result<()> {
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
        }
    }

    async fn frame(&mut self, kind: u8, payload: &[u8]) -> anyhow::Result<()> {
        self.control.write_all(&frame(kind, payload)).await?;
        Ok(())
    }

    /// Ordinary VIDEO or negotiated recovery shards. Every shard keeps its frame's priority;
    /// control stays ahead of both. Finishing never waits for the peer's acknowledgement.
    async fn video(&mut self, msg: &[u8]) -> anyhow::Result<()> {
        let protect = self.recovery.as_mut().is_some_and(|r| {
            r.protect(
                msg.len(),
                &self.conn.quic_connection().stats().path,
                Instant::now(),
            )
        });
        if protect {
            for shard in recovery::shards(msg) {
                self.video_stream(&shard).await?;
            }
        } else {
            self.video_stream(msg).await?;
        }
        self.video_priority = self.video_priority.checked_sub(1).unwrap_or(0);
        Ok(())
    }

    async fn video_stream(&mut self, msg: &[u8]) -> anyhow::Result<()> {
        let mut stream = self.conn.open_uni().await?.await?;
        stream.set_priority(self.video_priority);
        stream.write_all(msg).await?;
        // Errs only on a stream already reset; the data is queued either way.
        let _ = stream.quic_stream_mut().finish();
        Ok(())
    }

    /// Flushes the control stream, waiting until the peer has all of it (a close message
    /// included), then ends the connection: closing it at once would drop unacknowledged data.
    pub async fn close(&mut self) -> anyhow::Result<()> {
        let flushed = self.control.finish().await;
        self.conn.close(VarInt::from_u32(0), b"");
        Ok(flushed?)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // A session that ends without `close` (a write timeout) still ends its connection.
        self.conn.close(VarInt::from_u32(0), b"");
    }
}

/// Reads a session's messages from its control stream.
pub struct Stream {
    recv: RecvStream,
    buf: BytesMut,
    max_msg: usize,
}

impl Stream {
    /// Messages longer than `max_msg` bytes end the session.
    pub fn new(recv: RecvStream, max_msg: usize) -> Stream {
        Stream {
            recv,
            buf: BytesMut::new(),
            max_msg,
        }
    }

    /// The next message; None once the peer has finished. Cancel-safe: a message is never
    /// half-consumed by a dropped call.
    pub async fn next(&mut self) -> Option<anyhow::Result<Message>> {
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
        KIND_TEXT => Message::Text(String::from_utf8(payload.to_vec())?),
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
        reason: reason.to_owned(),
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
        // A close without a code is a close without a frame.
        let r = parse_all(&frame(KIND_CLOSE, &[]), 64);
        assert!(matches!(r[0], Ok(Some(Message::Close(None)))));
    }
}
