//! Runs the tilt-probe binary against an in-process mock of tilt's /stream (brief section 4).
//! The mock encodes the test pattern with OpenH264, fragments every access unit, and advances
//! the marker on KEY and button-1 input the way tilt plus tilt-testpattern do. Its framing is
//! written independently of the probe's, straight from the brief.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{Sink, SinkExt, StreamExt};
use openh264::encoder::{Encoder, EncoderConfig, FrameRate, FrameType, QpRange, RateControlMode};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;
use serde_json::{json, Value};
use tilt_e2e::{BACKGROUND, COLORS, MARKER_ORIGIN, MARKER_SIZE};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

const W: usize = 640;
const H: usize = 480;
/// Small, so that keyframes arrive in several fragments.
const MAX_MSG: usize = 1500;
const TAIL_FRAMES: u32 = 5;
const TOKEN: &str = "secret";

#[derive(Clone, Copy, Default)]
struct Mock {
    /// Move a bar every tick, like `tilt-testpattern --animate`.
    animate: bool,
    /// Open with a delta frame that has no keyframe before it.
    delta_first: bool,
    /// Replace this seq's payload with garbage.
    corrupt_seq: Option<u32>,
}

/// What the mock received.
#[derive(Debug, Default)]
struct Seen {
    hello: Value,
    took_control: bool,
    acks: Vec<(u32, u16)>,
    pings: u32,
    idr_requests: u32,
    keys: Vec<(bool, u32)>,
    moves: Vec<(u32, u32)>,
    buttons: Vec<(u8, bool, u32, u32)>,
    /// Every seq sent, in order.
    sent: Vec<u32>,
}

/// Limited-range BT.709, as tilt's converter produces it.
fn yuv_of((r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
    let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    (
        q(16.0 + y * 219.0 / 255.0),
        q(128.0 + (b - y) / 1.8556 * 224.0 / 255.0),
        q(128.0 + (r - y) / 1.5748 * 224.0 / 255.0),
    )
}

/// The pattern as I420: background, marker, and the animate bar in the band y 400..460.
fn render(marker: (u8, u8, u8), bar_x: Option<usize>) -> YUVBuffer {
    let mut data = vec![0; W * H * 3 / 2];
    let origin = MARKER_ORIGIN as usize;
    let size = usize::from(MARKER_SIZE);
    fill(&mut data, (0, 0, W, H), BACKGROUND);
    fill(&mut data, (origin, origin, size, size), marker);
    if let Some(x) = bar_x {
        fill(&mut data, (x, 400, 64.min(W - x), 60), (255, 255, 255));
    }
    YUVBuffer::from_vec(data, W, H)
}

/// Fills an (x, y, w, h) rectangle with even edges in an I420 buffer.
fn fill(i420: &mut [u8], (x, y, w, h): (usize, usize, usize, usize), rgb: (u8, u8, u8)) {
    let (luma, cb, cr) = yuv_of(rgb);
    let (y_plane, chroma) = i420.split_at_mut(W * H);
    let (u_plane, v_plane) = chroma.split_at_mut(W * H / 4);
    for row in y..y + h {
        y_plane[row * W + x..row * W + x + w].fill(luma);
    }
    for row in y / 2..(y + h) / 2 {
        let span = row * W / 2 + x / 2..row * W / 2 + (x + w) / 2;
        u_plane[span.clone()].fill(cb);
        v_plane[span].fill(cr);
    }
}

fn encoder() -> Encoder {
    // Fixed QP and no adaptive features: every frame comes out, and OpenH264 has nothing to
    // warn about on stderr.
    let config = EncoderConfig::new()
        .rate_control_mode(RateControlMode::Off)
        .qp(QpRange::new(24, 24))
        .max_frame_rate(FrameRate::from_hz(60.0))
        .skip_frames(false)
        .scene_change_detect(false)
        .adaptive_quantization(false)
        .background_detection(false);
    Encoder::with_api_config(OpenH264API::from_source(), config).expect("encoder")
}

/// VIDEO messages for one access unit: 18-byte header, MORE on all but the last fragment.
fn video_messages(seq: u32, key: bool, au: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = au.chunks(MAX_MSG - 18).collect();
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, part)| {
            let flags = u8::from(key) | if i < last { 2 } else { 0 };
            let mut m = vec![0x01, flags];
            m.extend_from_slice(&seq.to_le_bytes());
            m.extend_from_slice(&123_456u64.to_le_bytes());
            m.extend_from_slice(&(W as u16).to_le_bytes());
            m.extend_from_slice(&(H as u16).to_le_bytes());
            m.extend_from_slice(part);
            m
        })
        .collect()
}

async fn send_unit<S>(tx: &mut S, seq: u32, key: bool, au: &[u8]) -> Result<(), S::Error>
where
    S: Sink<Message> + Unpin,
{
    for m in video_messages(seq, key, au) {
        tx.send(Message::binary(m)).await?;
    }
    Ok(())
}

fn denormalize(v: u16, extent: usize) -> u32 {
    (f64::from(v) * (extent - 1) as f64 / 65535.0).round() as u32
}

async fn serve(listener: TcpListener, mock: Mock, seen: Arc<Mutex<Seen>>) {
    let (tcp, _) = listener.accept().await.expect("accept");
    tcp.set_nodelay(true).expect("nodelay");
    let ws = tokio_tungstenite::accept_async(tcp)
        .await
        .expect("ws accept");
    let (mut tx, mut rx) = ws.split();
    let text = |v: Value| Message::text(v.to_string());

    let hello: Value = match rx.next().await {
        Some(Ok(Message::Text(t))) => serde_json::from_str(&t).expect("hello json"),
        other => panic!("expected hello, got {other:?}"),
    };
    seen.lock().unwrap().hello = hello.clone();
    if hello["token"] != TOKEN {
        tx.send(text(json!({"t": "error", "code": "auth"})))
            .await
            .unwrap();
        let close = CloseFrame {
            code: CloseCode::from(4001),
            reason: "auth".into(),
        };
        let _ = tx.send(Message::Close(Some(close))).await;
        return;
    }
    let welcome = json!({"t": "welcome", "v": 1, "session": "s1", "role": "control",
                         "screen": {"w": W, "h": H}, "server": "mock"});
    tx.send(text(welcome)).await.unwrap();
    tx.send(text(json!({"t": "control", "you": false, "held": false})))
        .await
        .unwrap();

    let mut enc = encoder();
    let mut state = State {
        force_idr: true,
        dirty: true,
        ..State::default()
    };
    let (mut seq, mut bar, mut tail) = (0u32, 0usize, 0u32);
    if mock.delta_first {
        // A real delta frame from another encoder, so only its missing reference is wrong.
        let delta = {
            let mut other = encoder();
            other.encode(&render(COLORS[3], None)).unwrap();
            let delta = other.encode(&render(COLORS[2], None)).unwrap();
            assert_eq!(delta.frame_type(), FrameType::P);
            delta.to_vec()
        };
        seq += 1;
        send_unit(&mut tx, seq, false, &delta).await.unwrap();
        seen.lock().unwrap().sent.push(seq);
    }

    // A failed send ends the session like a closed read does: the probe hangs up when it is
    // done, and a frame or a pong can race with its close.
    let mut tick = tokio::time::interval(Duration::from_micros(16_667));
    loop {
        tokio::select! {
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let reply = state.handle(msg, &mut seen.lock().unwrap());
                if let Some(reply) = reply {
                    if tx.send(reply).await.is_err() {
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                if !(state.force_idr || state.dirty || mock.animate || tail > 0) {
                    continue;
                }
                if mock.animate {
                    bar = (bar + 8) % W;
                }
                if state.force_idr {
                    enc.force_intra_frame();
                }
                let color = COLORS[state.counter % COLORS.len()];
                let frame = render(color, mock.animate.then_some(bar));
                let (key, mut au) = {
                    let bitstream = enc.encode(&frame).expect("encode");
                    (bitstream.frame_type() == FrameType::IDR, bitstream.to_vec())
                };
                seq += 1;
                if mock.corrupt_seq == Some(seq) {
                    // A non-IDR slice header followed by noise.
                    au = [&[0, 0, 0, 1, 0x41][..], &[0x9a; 600]].concat();
                }
                if send_unit(&mut tx, seq, key, &au).await.is_err() {
                    break;
                }
                seen.lock().unwrap().sent.push(seq);
                state.force_idr &= !key;
                tail = if state.dirty || mock.animate {
                    TAIL_FRAMES
                } else {
                    tail.saturating_sub(1)
                };
                state.dirty = false;
            }
        }
    }
}

/// The mock's reaction to client input: tilt's input path plus the pattern's marker.
#[derive(Default)]
struct State {
    counter: usize,
    dirty: bool,
    force_idr: bool,
}

impl State {
    /// Records one client message; returns the reply, if any.
    fn handle(&mut self, msg: Message, seen: &mut Seen) -> Option<Message> {
        match msg {
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).expect("client json");
                match v["t"].as_str() {
                    Some("control") if v["take"] == true => {
                        seen.took_control = true;
                        return Some(Message::text(
                            json!({"t": "control", "you": true, "held": true}).to_string(),
                        ));
                    }
                    Some("idr") => {
                        seen.idr_requests += 1;
                        self.force_idr = true;
                    }
                    _ => panic!("unexpected client message {v}"),
                }
            }
            Message::Binary(b) => {
                let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
                let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
                match (b[0], b.len()) {
                    (0x10, 7) => seen.acks.push((u32_at(1), u16_at(5))),
                    (0x11, 5) => {
                        seen.pings += 1;
                        return Some(Message::binary([&[0x04][..], &b[1..5]].concat()));
                    }
                    (0x20, 5) => seen
                        .moves
                        .push((denormalize(u16_at(1), W), denormalize(u16_at(3), H))),
                    (0x21, 7) => {
                        let (button, down) = (b[1], b[2] == 1);
                        seen.buttons.push((
                            button,
                            down,
                            denormalize(u16_at(3), W),
                            denormalize(u16_at(5), H),
                        ));
                        self.advance(button == 1 && down);
                    }
                    (0x30, 6) => {
                        let down = b[1] == 1;
                        seen.keys.push((down, u32_at(2)));
                        self.advance(down);
                    }
                    _ => panic!("unexpected client message {b:?}"),
                }
            }
            // Close: keep reading, the next poll sends tungstenite's close reply.
            _ => {}
        }
        None
    }

    fn advance(&mut self, pressed: bool) {
        if pressed {
            self.counter += 1;
            self.dirty = true;
        }
    }
}

struct Run {
    code: Option<i32>,
    report: Value,
    stderr: String,
    seen: Seen,
}

async fn probe(mock: Mock, args: &[&str]) -> Run {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/stream", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Seen::default()));
    let server = tokio::spawn(serve(listener, mock, seen.clone()));

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tilt-probe"));
    cmd.args(["--url", &url, "--json"])
        .args(args)
        .env_remove("TILT_TOKEN");
    let out = tokio::task::spawn_blocking(move || cmd.output().expect("run tilt-probe"))
        .await
        .unwrap();
    // The mock ends when the probe disconnects; a panic in it fails the test here.
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("mock did not finish")
        .expect("mock panicked");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let report =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}\n{stderr}"));
    let seen = std::mem::take(&mut *seen.lock().unwrap());
    Run {
        code: out.status.code(),
        report,
        stderr,
        seen,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn measures_an_animated_stream() {
    let mock = Mock {
        animate: true,
        delta_first: true,
        ..Mock::default()
    };
    let args = [
        "--token",
        TOKEN,
        "--duration-s",
        "1",
        "--latency-trials",
        "8",
    ];
    let Run {
        code,
        report: r,
        stderr,
        seen,
    } = probe(mock, &args).await;
    assert_eq!(code, Some(0), "{r:#}\n{stderr}");
    assert_eq!(r["ok"], true);
    assert_eq!(r["screen"], json!({"w": W, "h": H}));
    assert_eq!(r["decode_errors"], 0);
    assert_eq!(r["protocol_errors"], 0);
    assert!(r["keyframes"].as_u64() >= Some(1));
    assert!(r["fps"].as_f64().unwrap() > 30.0, "{r:#}");
    assert!(r["kbps"].as_f64().unwrap() > 0.0);
    assert!(r["gaps_ms"]["p50"].as_f64().unwrap() < 40.0, "{r:#}");
    assert_eq!(r["latency_ms"]["n"], 8);
    assert_eq!(r["latency_ms"]["failed"], 0);
    assert!(r["latency_ms"]["p95"].as_f64().unwrap() < 300.0, "{r:#}");
    assert_eq!(r["click"]["ok"], true, "{r:#}");

    assert_eq!(seen.hello["v"], 1);
    assert!(seen.hello["client"]
        .as_str()
        .unwrap()
        .starts_with("tilt-probe/"));
    assert!(seen.took_control);
    // The click lands on exactly pixel (100,100) after the server's denormalization.
    assert_eq!(seen.moves, [(100, 100)]);
    assert_eq!(seen.buttons, [(1, true, 100, 100), (1, false, 100, 100)]);
    let spaces = seen.keys.iter().filter(|k| k.1 == 0x20).count();
    assert_eq!((seen.keys.len(), spaces), (16, 16));
    assert!(seen.keys.chunks(2).all(|p| p[0].0 && !p[1].0));
    // The leading delta has no reference: dropped (not a decode error), acked with 0 ms as the
    // web client does, and an IDR requested.
    assert_eq!(seen.acks.first(), Some(&(1, 0)));
    assert!(seen.idr_requests >= 1);
    // Every access unit up to the probe's disconnect is acked, in order.
    let acked: Vec<u32> = seen.acks.iter().map(|a| a.0).collect();
    assert_eq!(acked, seen.sent[..acked.len()]);
    assert!(
        acked.len() + 10 >= seen.sent.len(),
        "{} acks for {} units",
        acked.len(),
        seen.sent.len()
    );
    assert!(seen.pings >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missed_thresholds_exit_3_and_idle_is_measured() {
    let args = [
        "--token",
        TOKEN,
        "--duration-s",
        "0.5",
        "--latency-trials",
        "3",
        "--min-fps",
        "1000",
        "--idle-s",
        "1",
        "--max-idle-frames",
        "0",
    ];
    let Run {
        code,
        report: r,
        stderr,
        ..
    } = probe(Mock::default(), &args).await;
    assert_eq!(code, Some(3), "{r:#}\n{stderr}");
    assert_eq!(r["ok"], false);
    let failures = r["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1, "{r:#}");
    assert!(failures[0].as_str().unwrap().starts_with("fps: "), "{r:#}");
    assert_eq!(r["latency_ms"]["failed"], 0);
    assert_eq!(
        r["idle"],
        json!({"seconds": 1.0, "frames": 0, "settled": true})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_errors_are_recovered_and_reported() {
    let mock = Mock {
        corrupt_seq: Some(4),
        ..Mock::default()
    };
    let args = [
        "--token",
        TOKEN,
        "--duration-s",
        "0",
        "--latency-trials",
        "5",
        "--no-click",
    ];
    let Run {
        code,
        report: r,
        stderr,
        seen,
    } = probe(mock, &args).await;
    assert_eq!(code, Some(3), "{r:#}\n{stderr}");
    assert_eq!(r["decode_errors"], 1, "{r:#}");
    assert_eq!(r["failures"], json!(["decode errors: 1"]));
    // After the error the probe asked for a keyframe and the trials still all passed.
    assert!(seen.idr_requests >= 1);
    assert!(r["keyframes"].as_u64() >= Some(2));
    assert_eq!(r["latency_ms"]["n"], 5);
    assert!(r["click"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_failure_is_fatal() {
    let args = ["--token", "wrong", "--latency-trials", "1"];
    let Run {
        code,
        report: r,
        stderr,
        ..
    } = probe(Mock::default(), &args).await;
    assert_eq!(code, Some(1), "{r:#}\n{stderr}");
    assert_eq!(r["ok"], false);
    let error = r["error"].as_str().unwrap();
    assert!(
        error.contains("server refused the connection") && error.contains("auth"),
        "{error}"
    );
}
