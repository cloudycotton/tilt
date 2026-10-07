//! Wire protocol v1 (brief section 4): binary layouts, close codes and JSON messages.
//! All binary integers are little-endian.

use std::fmt;

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

use crate::auth::Role;
use crate::x11::cursor::CursorShape;

pub const VERSION: u32 = 1;
/// The `server` field of `welcome`.
pub const SERVER_ID: &str = concat!("tilt/", env!("CARGO_PKG_VERSION"));

// Server -> client binary message types.
pub const MSG_VIDEO: u8 = 0x01;
pub const MSG_CURSOR_SHAPE: u8 = 0x02;
pub const MSG_CURSOR_POS: u8 = 0x03;
pub const MSG_PONG: u8 = 0x04;

// Client -> server binary message types.
pub const MSG_ACK: u8 = 0x10;
pub const MSG_PING: u8 = 0x11;
pub const MSG_MOVE: u8 = 0x20;
pub const MSG_BUTTON: u8 = 0x21;
pub const MSG_WHEEL: u8 = 0x22;
pub const MSG_KEY: u8 = 0x30;
pub const MSG_TEXT: u8 = 0x31;
pub const MSG_RELEASE_ALL: u8 = 0x32;

/// VIDEO flags bit0: an IDR with SPS+PPS in the payload.
pub const FLAG_KEY: u8 = 1 << 0;
/// VIDEO flags bit1: another fragment of the same seq follows.
pub const FLAG_MORE: u8 = 1 << 1;
/// VIDEO header length, type byte included.
pub const VIDEO_HEADER_LEN: usize = 18;
/// The most UTF-8 one TEXT message types; the rest is dropped. Typing is slow (a character
/// the keymap lacks costs a keyboard remap), so this bounds the time one message holds the
/// input thread. Clients send longer text as several messages. (Addition to the brief.)
pub const MAX_TEXT_BYTES: usize = 4096;

// WebSocket close codes.
pub const CLOSE_NORMAL: u16 = 1000;
pub const CLOSE_AUTH: u16 = 4001;
pub const CLOSE_BUSY: u16 = 4002;
pub const CLOSE_VERSION: u16 = 4003;
pub const CLOSE_NOT_READY: u16 = 4004;
pub const CLOSE_HELLO_TIMEOUT: u16 = 4005;
// Standard codes for closes the brief leaves open (all mirrored in docs/protocol.md).
/// The server is shutting down.
pub const CLOSE_GOING_AWAY: u16 = 1001;
/// More than 100 malformed messages.
pub const CLOSE_POLICY: u16 = 1008;
/// The session's encoder stopped.
pub const CLOSE_INTERNAL: u16 = 1011;

/// Header shared by every fragment of one VIDEO access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub flags: u8,
    /// Per-session access-unit counter, starting at 1.
    pub seq: u32,
    /// Server monotonic microseconds at capture (informational).
    pub capture_us: u64,
    /// Coded (even-cropped) size.
    pub width: u16,
    pub height: u16,
}

impl VideoHeader {
    /// The 18-byte wire form, type byte included.
    #[allow(clippy::wrong_self_convention)] // the skeleton's signature
    pub fn to_bytes(&self) -> [u8; VIDEO_HEADER_LEN] {
        let mut b = [0u8; VIDEO_HEADER_LEN];
        b[0] = MSG_VIDEO;
        b[1] = self.flags;
        b[2..6].copy_from_slice(&self.seq.to_le_bytes());
        b[6..14].copy_from_slice(&self.capture_us.to_le_bytes());
        b[14..16].copy_from_slice(&self.width.to_le_bytes());
        b[16..18].copy_from_slice(&self.height.to_le_bytes());
        b
    }

    /// Splits a VIDEO message into its header and payload; None if it is not a well-formed VIDEO message.
    /// The reference parser for tests; the server itself never parses VIDEO.
    #[cfg(test)]
    pub fn parse(msg: &[u8]) -> Option<(VideoHeader, &[u8])> {
        if msg.len() < VIDEO_HEADER_LEN || msg[0] != MSG_VIDEO {
            return None;
        }
        let header = VideoHeader {
            flags: msg[1],
            seq: u32::from_le_bytes(msg[2..6].try_into().ok()?),
            capture_us: u64::from_le_bytes(msg[6..14].try_into().ok()?),
            width: u16::from_le_bytes(msg[14..16].try_into().ok()?),
            height: u16::from_le_bytes(msg[16..18].try_into().ok()?),
        };
        Some((header, &msg[VIDEO_HEADER_LEN..]))
    }
}

/// Frames one Annex B access unit as VIDEO messages of at most `max_msg_bytes` each, header
/// included. Every fragment repeats `header`; FLAG_MORE is set on all but the last.
pub fn video_messages(header: VideoHeader, access_unit: &[u8], max_msg_bytes: usize) -> Vec<Bytes> {
    let room = max_msg_bytes.saturating_sub(VIDEO_HEADER_LEN).max(1);
    let base = header.flags & !FLAG_MORE;
    // An empty access unit still yields one (header-only) message, so every seq reaches the client.
    let count = access_unit.len().div_ceil(room).max(1);
    (0..count)
        .map(|i| {
            let chunk = &access_unit
                [(i * room).min(access_unit.len())..((i + 1) * room).min(access_unit.len())];
            let flags = if i + 1 < count {
                base | FLAG_MORE
            } else {
                base
            };
            let mut msg = BytesMut::with_capacity(VIDEO_HEADER_LEN + chunk.len());
            msg.put_slice(&VideoHeader { flags, ..header }.to_bytes());
            msg.put_slice(chunk);
            msg.freeze()
        })
        .collect()
}

/// Whether `msg` is a VIDEO fragment that another fragment of the same seq follows.
pub fn video_has_more(msg: &[u8]) -> bool {
    matches!(msg, [MSG_VIDEO, flags, ..] if flags & FLAG_MORE != 0)
}

/// CURSOR_SHAPE: serial, size, hotspot, then w*h*4 bytes of straight-alpha RGBA (w=h=0 means hidden).
pub fn cursor_shape_msg(shape: &CursorShape) -> Bytes {
    let image_len = usize::from(shape.width) * usize::from(shape.height) * 4;
    let mut msg = BytesMut::with_capacity(13 + image_len);
    msg.put_u8(MSG_CURSOR_SHAPE);
    msg.put_u32_le(shape.serial);
    msg.put_u16_le(shape.width);
    msg.put_u16_le(shape.height);
    msg.put_u16_le(shape.xhot);
    msg.put_u16_le(shape.yhot);
    // The layout promises exactly w*h*4 bytes; pad (transparent) or cut if the image disagrees.
    let image = &shape.rgba[..image_len.min(shape.rgba.len())];
    msg.put_slice(image);
    msg.put_bytes(0, image_len - image.len());
    msg.freeze()
}

/// CURSOR_POS in screen pixels, clamped to i16.
pub fn cursor_pos_msg(x: i32, y: i32) -> Bytes {
    let clamp = |v: i32| v.clamp(i16::MIN.into(), i16::MAX.into()) as i16;
    let mut msg = BytesMut::with_capacity(5);
    msg.put_u8(MSG_CURSOR_POS);
    msg.put_i16_le(clamp(x));
    msg.put_i16_le(clamp(y));
    msg.freeze()
}

/// PONG echoing PING.t.
pub fn pong_msg(t: u32) -> Bytes {
    let mut msg = BytesMut::with_capacity(5);
    msg.put_u8(MSG_PONG);
    msg.put_u32_le(t);
    msg.freeze()
}

/// Maps a normalized coordinate (0 = first pixel, 65535 = last) onto `extent` pixels:
/// `round(v * (extent - 1) / 65535)`. For the input thread, which owns the root size.
pub fn denormalize(v: u16, extent: u32) -> u32 {
    let last = u64::from(extent.saturating_sub(1));
    ((u64::from(v) * last + 32_767) / 65_535) as u32
}

/// A client -> server binary message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    /// Cumulative: acknowledges every seq <= `seq`.
    Ack {
        seq: u32,
        decode_ms: u16,
    },
    Ping {
        t: u32,
    },
    Move {
        x: u16,
        y: u16,
    },
    /// X button numbers: 1 left, 2 middle, 3 right, 8 back, 9 forward.
    Button {
        button: u8,
        down: bool,
        x: u16,
        y: u16,
    },
    /// Discrete notches: +dy down (button 5), -dy up (4), +dx right (7), -dx left (6).
    Wheel {
        dx: i16,
        dy: i16,
        x: u16,
        y: u16,
    },
    /// A down for an already-down keysym means autorepeat.
    Key {
        down: bool,
        keysym: u32,
    },
    /// At most MAX_TEXT_BYTES of it; `dropped` bytes were cut off the end, at a character
    /// boundary.
    Text {
        text: String,
        dropped: usize,
    },
    ReleaseAll,
}

/// Why a client binary message was rejected. The reader counts these; it never panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    UnknownType(u8),
    /// Payload too short (or too long) for the message type.
    Length {
        kind: u8,
        len: usize,
    },
    Utf8,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => f.write_str("empty message"),
            ParseError::UnknownType(t) => write!(f, "unknown message type {t:#04x}"),
            ParseError::Length { kind, len } => {
                write!(f, "bad length {len} for message type {kind:#04x}")
            }
            ParseError::Utf8 => f.write_str("TEXT payload is not UTF-8"),
        }
    }
}

impl std::error::Error for ParseError {}

impl ClientMsg {
    /// Parses one binary message. Fixed-size messages must have exactly their size.
    pub fn parse(msg: &[u8]) -> Result<ClientMsg, ParseError> {
        let (&kind, p) = msg.split_first().ok_or(ParseError::Empty)?;
        let fixed = |len: usize| {
            if p.len() == len {
                Ok(p)
            } else {
                Err(ParseError::Length { kind, len: p.len() })
            }
        };
        Ok(match kind {
            MSG_ACK => {
                let p = fixed(6)?;
                ClientMsg::Ack {
                    seq: u32_le(p, 0),
                    decode_ms: u16_le(p, 4),
                }
            }
            MSG_PING => ClientMsg::Ping {
                t: u32_le(fixed(4)?, 0),
            },
            MSG_MOVE => {
                let p = fixed(4)?;
                ClientMsg::Move {
                    x: u16_le(p, 0),
                    y: u16_le(p, 2),
                }
            }
            MSG_BUTTON => {
                let p = fixed(6)?;
                ClientMsg::Button {
                    button: p[0],
                    down: p[1] != 0,
                    x: u16_le(p, 2),
                    y: u16_le(p, 4),
                }
            }
            MSG_WHEEL => {
                let p = fixed(8)?;
                ClientMsg::Wheel {
                    dx: u16_le(p, 0) as i16,
                    dy: u16_le(p, 2) as i16,
                    x: u16_le(p, 4),
                    y: u16_le(p, 6),
                }
            }
            MSG_KEY => {
                let p = fixed(5)?;
                ClientMsg::Key {
                    down: p[0] != 0,
                    keysym: u32_le(p, 1),
                }
            }
            MSG_TEXT => {
                let text = std::str::from_utf8(p).map_err(|_| ParseError::Utf8)?;
                let mut end = text.len().min(MAX_TEXT_BYTES);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                ClientMsg::Text {
                    text: text[..end].to_owned(),
                    dropped: text.len() - end,
                }
            }
            MSG_RELEASE_ALL => {
                fixed(0)?;
                ClientMsg::ReleaseAll
            }
            other => return Err(ParseError::UnknownType(other)),
        })
    }
}

// Callers check the length first.
fn u16_le(p: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([p[at], p[at + 1]])
}

fn u32_le(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]])
}

/// A client -> server JSON message, tagged by `t`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientText {
    /// `token` may be absent only in no-auth mode.
    Hello {
        v: u32,
        token: Option<String>,
        client: Option<String>,
    },
    Control {
        take: bool,
    },
    Idr,
    Video {
        on: bool,
    },
}

/// A server -> client JSON message, tagged by `t`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerText {
    Welcome {
        v: u32,
        session: String,
        role: Role,
        screen: ScreenSize,
        server: &'static str,
    },
    Control {
        you: bool,
        held: bool,
        /// `s<id>` of the session holding control; absent when nobody does. (Addition to the
        /// brief.)
        #[serde(skip_serializing_if = "Option::is_none")]
        holder: Option<String>,
    },
    Screen {
        w: u32,
        h: u32,
    },
    Stats(Stats),
    Error {
        code: ErrorCode,
        #[serde(skip_serializing_if = "Option::is_none")]
        msg: Option<String>,
    },
}

impl ServerText {
    pub fn to_json(&self) -> String {
        // Infallible: string keys only, and serde_json writes non-finite floats as null.
        serde_json::to_string(self).expect("ServerText serializes")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ScreenSize {
    pub w: u32,
    pub h: u32,
}

/// The once-per-second `stats` message.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Stats {
    pub fps: f32,
    pub kbps: f32,
    pub bitrate_kbps: u32,
    pub rtt_ms: f32,
    pub min_rtt_ms: f32,
    pub queue_ms: f32,
    pub enc_ms: f32,
    pub inflight: u32,
    pub skipped: u64,
    pub viewers: u32,
    /// The frame rate cap in force: `--max-fps`.
    pub gov_fps: u32,
    /// Mean quantizer of the frames in the window; 0 when there were none.
    pub qp: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Auth,
    Busy,
    Version,
    NotReady,
    Forbidden,
    HelloTimeout,
    /// The encoder cannot take the screen's size; the session stays open and video resumes
    /// when the size changes. (Addition to the brief.)
    UnsupportedSize,
}

impl ErrorCode {
    /// The close code that follows this error; None when the session stays open.
    pub fn close_code(self) -> Option<u16> {
        match self {
            ErrorCode::Auth => Some(CLOSE_AUTH),
            ErrorCode::Busy => Some(CLOSE_BUSY),
            ErrorCode::Version => Some(CLOSE_VERSION),
            ErrorCode::NotReady => Some(CLOSE_NOT_READY),
            ErrorCode::HelloTimeout => Some(CLOSE_HELLO_TIMEOUT),
            ErrorCode::Forbidden | ErrorCode::UnsupportedSize => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    // The JSON shapes are normative (brief section 4.4); these pin the serde attributes to them.
    #[test]
    fn server_json_matches_the_wire_format() {
        let welcome = ServerText::Welcome {
            v: VERSION,
            session: "s7".into(),
            role: Role::Control,
            screen: ScreenSize { w: 1920, h: 1080 },
            server: SERVER_ID,
        };
        assert_eq!(
            welcome.to_json(),
            r#"{"t":"welcome","v":1,"session":"s7","role":"control","screen":{"w":1920,"h":1080},"server":"tilt/0.1.0"}"#
        );
        assert_eq!(
            ServerText::Control {
                you: false,
                held: true,
                holder: Some("s7".into())
            }
            .to_json(),
            r#"{"t":"control","you":false,"held":true,"holder":"s7"}"#
        );
        assert_eq!(
            ServerText::Control {
                you: false,
                held: false,
                holder: None
            }
            .to_json(),
            r#"{"t":"control","you":false,"held":false}"#
        );
        assert_eq!(
            ServerText::Screen { w: 1024, h: 768 }.to_json(),
            r#"{"t":"screen","w":1024,"h":768}"#
        );
        assert_eq!(
            ServerText::Error {
                code: ErrorCode::NotReady,
                msg: None
            }
            .to_json(),
            r#"{"t":"error","code":"not_ready"}"#
        );
        assert_eq!(
            ServerText::Error {
                code: ErrorCode::HelloTimeout,
                msg: Some("late".into())
            }
            .to_json(),
            r#"{"t":"error","code":"hello_timeout","msg":"late"}"#
        );
        let stats = ServerText::Stats(Stats {
            fps: 60.0,
            kbps: 1500.5,
            bitrate_kbps: 8000,
            viewers: 1,
            gov_fps: 45,
            ..Stats::default()
        });
        assert_eq!(
            stats.to_json(),
            r#"{"t":"stats","fps":60.0,"kbps":1500.5,"bitrate_kbps":8000,"rtt_ms":0.0,"min_rtt_ms":0.0,"queue_ms":0.0,"enc_ms":0.0,"inflight":0,"skipped":0,"viewers":1,"gov_fps":45,"qp":0.0}"#
        );
    }

    #[test]
    fn client_json_parses() {
        let parse = |s: &str| serde_json::from_str::<ClientText>(s).unwrap();
        assert_eq!(
            parse(r#"{"t":"hello","v":1,"token":"tok","client":"tilt-web/0.1"}"#),
            ClientText::Hello {
                v: 1,
                token: Some("tok".into()),
                client: Some("tilt-web/0.1".into())
            }
        );
        assert_eq!(
            parse(r#"{"t":"hello","v":1}"#),
            ClientText::Hello {
                v: 1,
                token: None,
                client: None
            }
        );
        assert_eq!(
            parse(r#"{"t":"control","take":true}"#),
            ClientText::Control { take: true }
        );
        assert_eq!(parse(r#"{"t":"idr"}"#), ClientText::Idr);
        assert_eq!(
            parse(r#"{"t":"video","on":false,"extra":1}"#),
            ClientText::Video { on: false }
        );
        assert!(serde_json::from_str::<ClientText>(r#"{"t":"resize","w":1}"#).is_err());
        assert!(serde_json::from_str::<ClientText>(r#"{"v":1}"#).is_err());
    }

    #[test]
    fn video_header_roundtrips() {
        let h = VideoHeader {
            flags: FLAG_KEY,
            seq: 0xA1B2_C3D4,
            capture_us: 0x0102_0304_0506_0708,
            width: 1920,
            height: 1080,
        };
        let b = h.to_bytes();
        assert_eq!(
            b,
            [
                0x01, 0x01, 0xD4, 0xC3, 0xB2, 0xA1, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
                0x80, 0x07, 0x38, 0x04
            ]
        );
        let mut msg = b.to_vec();
        msg.extend_from_slice(b"payload");
        assert_eq!(VideoHeader::parse(&msg), Some((h, &b"payload"[..])));
        assert_eq!(VideoHeader::parse(&b), Some((h, &[][..])));
        assert_eq!(VideoHeader::parse(&b[..17]), None);
        let mut wrong_type = b;
        wrong_type[0] = MSG_PONG;
        assert_eq!(VideoHeader::parse(&wrong_type), None);
    }

    fn fragments(len: usize, max: usize) -> Vec<Bytes> {
        let header = VideoHeader {
            flags: FLAG_KEY,
            seq: 7,
            capture_us: 99,
            width: 64,
            height: 48,
        };
        let au: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let msgs = video_messages(header, &au, max);
        // Every fragment fits, repeats the header, and only the last lacks MORE.
        let mut joined = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            assert!(m.len() <= max, "fragment {i} is {} > {max}", m.len());
            let (h, payload) = VideoHeader::parse(m).unwrap();
            let more = i + 1 < msgs.len();
            assert_eq!(
                h,
                VideoHeader {
                    flags: if more { FLAG_KEY | FLAG_MORE } else { FLAG_KEY },
                    ..header
                }
            );
            joined.extend_from_slice(payload);
        }
        assert_eq!(joined, au);
        msgs
    }

    #[test]
    fn fragmentation_boundaries() {
        let max = 1024;
        let room = max - VIDEO_HEADER_LEN;
        assert_eq!(fragments(0, max).len(), 1);
        assert_eq!(fragments(1, max).len(), 1);
        assert_eq!(fragments(room - 1, max).len(), 1);
        let exact = fragments(room, max);
        assert_eq!((exact.len(), exact[0].len()), (1, max));
        let over = fragments(room + 1, max);
        assert_eq!(over.len(), 2);
        assert_eq!((over[0].len(), over[1].len()), (max, VIDEO_HEADER_LEN + 1));
        let two = fragments(2 * room, max);
        assert_eq!((two.len(), two[1].len()), (2, max));
        // The writer keeps fragments together by this flag.
        assert!(video_has_more(&two[0]) && !video_has_more(&two[1]));
        assert!(!video_has_more(&[]) && !video_has_more(&[MSG_VIDEO]));
        assert!(!video_has_more(&pong_msg(0x0202_0202)));
        assert_eq!(fragments(2 * room + 1, max).len(), 3);
        assert_eq!(fragments(262_144 * 3, 262_144).len(), 4);
        // A caller's stray MORE bit never leaks onto the last fragment.
        let h = VideoHeader {
            flags: FLAG_MORE,
            seq: 1,
            capture_us: 0,
            width: 2,
            height: 2,
        };
        let msgs = video_messages(h, &[1, 2, 3], max);
        assert_eq!(VideoHeader::parse(&msgs[0]).unwrap().0.flags, 0);
        // Degenerate limits still make progress.
        assert_eq!(video_messages(h, &[1, 2, 3], 0).len(), 3);
    }

    #[test]
    fn small_server_messages() {
        assert_eq!(&pong_msg(0x0403_0201)[..], &[0x04, 1, 2, 3, 4]);
        assert_eq!(
            &cursor_pos_msg(-2, 300)[..],
            &[0x03, 0xFE, 0xFF, 0x2C, 0x01]
        );
        assert_eq!(
            &cursor_pos_msg(100_000, -100_000)[..],
            &[0x03, 0xFF, 0x7F, 0x00, 0x80]
        );
        let shape = CursorShape {
            serial: 9,
            width: 2,
            height: 1,
            xhot: 1,
            yhot: 0,
            rgba: Arc::new(vec![1, 2, 3, 4, 5, 6, 7, 8]),
        };
        assert_eq!(
            &cursor_shape_msg(&shape)[..],
            &[0x02, 9, 0, 0, 0, 2, 0, 1, 0, 1, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        let hidden = CursorShape {
            serial: 10,
            width: 0,
            height: 0,
            xhot: 0,
            yhot: 0,
            rgba: Arc::new(Vec::new()),
        };
        assert_eq!(cursor_shape_msg(&hidden).len(), 13);
        // An image shorter than w*h*4 is padded, a longer one cut, so the layout always holds.
        let short = CursorShape {
            rgba: Arc::new(vec![1, 2, 3]),
            ..shape.clone()
        };
        assert_eq!(&cursor_shape_msg(&short)[13..], &[1, 2, 3, 0, 0, 0, 0, 0]);
        let long = CursorShape {
            rgba: Arc::new(vec![1; 12]),
            ..shape
        };
        assert_eq!(cursor_shape_msg(&long).len(), 13 + 8);
    }

    #[test]
    fn denormalize_maps_edges_and_rounds() {
        assert_eq!(denormalize(0, 1920), 0);
        assert_eq!(denormalize(65535, 1920), 1919);
        assert_eq!(denormalize(32768, 1921), 960);
        assert_eq!(denormalize(65535, 1), 0);
        assert_eq!(denormalize(65535, 0), 0);
        // The client sends round(fx * 65535); every pixel centre must map back to its pixel.
        for w in [2u32, 3, 1024, 1366, 1920, 3840] {
            for px in 0..w {
                let fx = px as f64 / (w - 1) as f64;
                assert_eq!(denormalize((fx * 65535.0).round() as u16, w), px, "w={w}");
            }
        }
    }

    #[test]
    fn client_binary_messages_parse() {
        let ok = |m: &[u8]| ClientMsg::parse(m).unwrap();
        assert_eq!(
            ok(&[0x10, 5, 0, 0, 0, 0x2C, 0x01]),
            ClientMsg::Ack {
                seq: 5,
                decode_ms: 300
            }
        );
        assert_eq!(
            ok(&[0x11, 1, 0, 0, 0x80]),
            ClientMsg::Ping { t: 0x8000_0001 }
        );
        assert_eq!(
            ok(&[0x20, 0xFF, 0xFF, 0, 0]),
            ClientMsg::Move { x: 65535, y: 0 }
        );
        assert_eq!(
            ok(&[0x21, 3, 1, 1, 0, 2, 0]),
            ClientMsg::Button {
                button: 3,
                down: true,
                x: 1,
                y: 2
            }
        );
        assert_eq!(
            ok(&[0x21, 1, 0, 0, 0, 0, 0]),
            ClientMsg::Button {
                button: 1,
                down: false,
                x: 0,
                y: 0
            }
        );
        assert_eq!(
            ok(&[0x22, 0xFF, 0xFF, 2, 0, 10, 0, 20, 0]),
            ClientMsg::Wheel {
                dx: -1,
                dy: 2,
                x: 10,
                y: 20
            }
        );
        assert_eq!(
            ok(&[0x30, 1, 0x08, 0xFF, 0, 0]),
            ClientMsg::Key {
                down: true,
                keysym: 0xFF08
            }
        );
        assert_eq!(
            ok("\u{31}héllo\n".as_bytes()),
            ClientMsg::Text {
                text: "héllo\n".into(),
                dropped: 0
            }
        );
        assert_eq!(
            ok(&[0x31]),
            ClientMsg::Text {
                text: String::new(),
                dropped: 0
            }
        );
        assert_eq!(ok(&[0x32]), ClientMsg::ReleaseAll);
    }

    #[test]
    fn long_text_is_cut_at_a_character_boundary() {
        let text = |body: &str| {
            let mut m = vec![MSG_TEXT];
            m.extend_from_slice(body.as_bytes());
            match ClientMsg::parse(&m) {
                Ok(ClientMsg::Text { text, dropped }) => (text, dropped),
                other => panic!("{other:?}"),
            }
        };
        let full = "a".repeat(MAX_TEXT_BYTES);
        assert_eq!(text(&full), (full.clone(), 0));
        assert_eq!(text(&format!("{full}b")), (full, 1));
        // 3-byte characters: 4096 = 3 * 1365 + 1, so the 1366th would straddle the limit.
        let cjk = "中".repeat(2000);
        let (kept, dropped) = text(&cjk);
        assert_eq!((kept.len(), dropped), (1365 * 3, 635 * 3));
        assert!(cjk.starts_with(&kept));
        // The whole payload must be UTF-8, not just what is kept.
        let mut m = vec![b'a'; 1 + MAX_TEXT_BYTES];
        m[0] = MSG_TEXT;
        m.push(0xC3);
        assert_eq!(ClientMsg::parse(&m), Err(ParseError::Utf8));
    }

    #[test]
    fn malformed_client_messages_are_errors() {
        use ParseError::*;
        assert_eq!(ClientMsg::parse(&[]), Err(Empty));
        assert_eq!(ClientMsg::parse(&[0x99, 1]), Err(UnknownType(0x99)));
        assert_eq!(ClientMsg::parse(&[0x01]), Err(UnknownType(0x01)));
        assert_eq!(
            ClientMsg::parse(&[0x10, 1, 2]),
            Err(Length { kind: 0x10, len: 2 })
        );
        assert_eq!(
            ClientMsg::parse(&[0x11, 1, 2, 3, 4, 5]),
            Err(Length { kind: 0x11, len: 5 })
        );
        assert_eq!(
            ClientMsg::parse(&[0x32, 0]),
            Err(Length { kind: 0x32, len: 1 })
        );
        assert_eq!(ClientMsg::parse(&[0x31, 0xC3]), Err(Utf8));
        for kind in [0x10u8, 0x11, 0x20, 0x21, 0x22, 0x30] {
            assert!(matches!(ClientMsg::parse(&[kind]), Err(Length { .. })));
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // Every type byte with every short length, then pseudo-random messages.
        for kind in 0..=255u8 {
            for len in 0..=12usize {
                let mut m = vec![kind];
                m.extend((0..len).map(|i| (i as u8).wrapping_mul(37)));
                let _ = ClientMsg::parse(&m);
                let _ = VideoHeader::parse(&m);
            }
        }
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..20_000 {
            let len = (next() % 40) as usize;
            let m: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = ClientMsg::parse(&m);
            let _ = VideoHeader::parse(&m);
            let _ = serde_json::from_slice::<ClientText>(&m);
        }
    }

    #[test]
    fn errors_close_with_their_codes() {
        assert_eq!(ErrorCode::Auth.close_code(), Some(4001));
        assert_eq!(ErrorCode::HelloTimeout.close_code(), Some(4005));
        assert_eq!(ErrorCode::Forbidden.close_code(), None);
        assert_eq!(ErrorCode::UnsupportedSize.close_code(), None);
        let error = ServerText::Error {
            code: ErrorCode::UnsupportedSize,
            msg: None,
        };
        assert!(error.to_json().contains(r#""code":"unsupported_size""#));
    }
}
