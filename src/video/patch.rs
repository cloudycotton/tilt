//! Patches: small screen changes sent as the changed pixels themselves instead of an H.264
//! frame. OpenH264 works through every macroblock of the screen whatever changed, about 3 ms
//! for a 1080p frame, so a typed character cost as much as a full-screen scroll. A patch costs
//! next to nothing to make.
//!
//! The pixels are the I420 of the captured frame, the same the encoder gets, so a patch and
//! the video around it match: the client draws it as a VideoFrame with the stream's colour
//! space. The encoder never sees a patch. Its next frame codes everything that changed since
//! its own last frame, patched areas included, so the stream stays consistent.
//!
//! Wire format (the payload of a VIDEO message with FLAG_PATCH; docs/protocol.md):
//!
//! ```text
//! u16 count, then per rectangle:
//!   u16 x, u16 y, u16 w, u16 h   (even, inside the frame)
//!   u32 len, then len bytes      (Y rows, then U rows, then V rows, run-length coded)
//! ```
//!
//! The run-length code: a byte c < 128 is followed by c + 1 literal bytes; c >= 128 is
//! followed by one byte repeated c - 125 times (3 to 130). Runs may cross rows and planes.

use super::convert::I420Frame;

/// A screen rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// More rectangles than this become their bounding box.
const MAX_RECTS: usize = 32;
const LONGEST_RUN: usize = 130;
const LONGEST_LITERAL: usize = 128;

/// `rects` clipped to a `width` x `height` frame and widened to even edges (4:2:0 chroma
/// covers 2x2 pixels), with empty ones dropped and too many merged into one.
pub fn even_rects(rects: &[Rect], width: u32, height: u32) -> Vec<Rect> {
    let mut out: Vec<Rect> = rects
        .iter()
        .filter_map(|r| {
            let (x0, y0) = (r.x.min(width) & !1, r.y.min(height) & !1);
            let x1 = (r.x.saturating_add(r.w).min(width) + 1) & !1;
            let y1 = (r.y.saturating_add(r.h).min(height) + 1) & !1;
            (x1 > x0 && y1 > y0).then_some(Rect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            })
        })
        .collect();
    if out.len() > MAX_RECTS {
        let x0 = out.iter().map(|r| r.x).min().unwrap_or(0);
        let y0 = out.iter().map(|r| r.y).min().unwrap_or(0);
        let x1 = out.iter().map(|r| r.x + r.w).max().unwrap_or(0);
        let y1 = out.iter().map(|r| r.y + r.h).max().unwrap_or(0);
        out = vec![Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }];
    }
    out
}

/// The patch for `rects` (from even_rects) of `image`, or None when they cover more than
/// `max_area` pixels or the patch would be bigger than `max_bytes`: then a video frame is
/// cheaper to send.
pub fn encode(
    image: &I420Frame,
    rects: &[Rect],
    max_area: u64,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    let area: u64 = rects.iter().map(|r| u64::from(r.w) * u64::from(r.h)).sum();
    if rects.is_empty() || area > max_area {
        return None;
    }
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(&(rects.len() as u16).to_le_bytes());
    let (w, cw) = (image.width as usize, image.width as usize / 2);
    for r in rects {
        debug_assert!((r.x | r.y | r.w | r.h) & 1 == 0 && r.x + r.w <= image.width);
        for v in [r.x, r.y, r.w, r.h] {
            out.extend_from_slice(&(v as u16).to_le_bytes());
        }
        let len_at = out.len();
        out.extend_from_slice(&[0; 4]);
        let mut rle = Rle::new(&mut out);
        let (x, y, rw, rh) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        for row in y..y + rh {
            rle.push(&image.y[row * w + x..][..rw]);
        }
        for plane in [&image.u, &image.v] {
            for row in y / 2..(y + rh) / 2 {
                rle.push(&plane[row * cw + x / 2..][..rw / 2]);
            }
        }
        rle.finish();
        let len = (out.len() - len_at - 4) as u32;
        out[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        if out.len() > max_bytes {
            return None;
        }
    }
    Some(out)
}

/// The run-length coder: bytes go in with `push`, possibly over several calls, and come out
/// in `out`.
struct Rle<'a> {
    out: &'a mut Vec<u8>,
    /// Bytes not yet written: a literal, and a run of `run` repeats of `last` after it.
    literal: Vec<u8>,
    last: u8,
    run: usize,
}

impl<'a> Rle<'a> {
    fn new(out: &'a mut Vec<u8>) -> Rle<'a> {
        Rle {
            out,
            literal: Vec::with_capacity(LONGEST_LITERAL),
            last: 0,
            run: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.run > 0 && b == self.last && self.run < LONGEST_RUN {
                self.run += 1;
                continue;
            }
            self.end_run();
            self.last = b;
            self.run = 1;
        }
    }

    /// Writes the pending run: as a run if it is long enough, else onto the literal.
    fn end_run(&mut self) {
        if self.run >= 3 {
            self.flush_literal();
            self.out.push((self.run + 125) as u8);
            self.out.push(self.last);
        } else {
            for _ in 0..self.run {
                if self.literal.len() == LONGEST_LITERAL {
                    self.flush_literal();
                }
                self.literal.push(self.last);
            }
        }
        self.run = 0;
    }

    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            self.out.push((self.literal.len() - 1) as u8);
            self.out.extend_from_slice(&self.literal);
            self.literal.clear();
        }
    }

    fn finish(mut self) {
        self.end_run();
        self.flush_literal();
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Decodes a run-length coded stream (the client's job).
    pub fn unrle(mut b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while let [c, rest @ ..] = b {
            if *c < 128 {
                let n = usize::from(*c) + 1;
                out.extend_from_slice(&rest[..n]);
                b = &rest[n..];
            } else {
                out.extend(std::iter::repeat_n(rest[0], usize::from(*c) - 125));
                b = &rest[1..];
            }
        }
        out
    }

    /// The rectangles of a patch, each with its decoded I420 bytes.
    pub fn decode(p: &[u8]) -> Vec<(Rect, Vec<u8>)> {
        let u16_at = |i: usize| u32::from(u16::from_le_bytes([p[i], p[i + 1]]));
        let mut at = 2;
        (0..u16_at(0))
            .map(|_| {
                let r = Rect {
                    x: u16_at(at),
                    y: u16_at(at + 2),
                    w: u16_at(at + 4),
                    h: u16_at(at + 6),
                };
                let len = u32::from_le_bytes(p[at + 8..at + 12].try_into().unwrap()) as usize;
                let pixels = unrle(&p[at + 12..at + 12 + len]);
                at += 12 + len;
                (r, pixels)
            })
            .collect()
    }

    fn image(w: u32, h: u32) -> I420Frame {
        // Flat background with a noisy block: runs and literals both.
        let mut f = I420Frame {
            width: w,
            height: h,
            y: vec![16; (w * h) as usize],
            u: vec![128; (w * h / 4) as usize],
            v: vec![128; (w * h / 4) as usize],
        };
        for (i, p) in f.y.iter_mut().enumerate() {
            if (i as u32 % w) < 40 && (i as u32 / w) < 20 {
                *p = (i * 7919 % 251) as u8;
            }
        }
        f
    }

    #[test]
    fn run_length_code_round_trips() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![5],
            vec![5, 5],
            vec![5, 5, 5],
            vec![9; 1000],
            (0..=255).cycle().take(700).collect(),
            [vec![1, 2], vec![7; 131], vec![3], vec![7; 2], vec![8; 3]].concat(),
        ];
        for bytes in cases {
            let mut out = Vec::new();
            let mut rle = Rle::new(&mut out);
            // In uneven pieces, as rows arrive.
            for chunk in bytes.chunks(7) {
                rle.push(chunk);
            }
            rle.finish();
            assert_eq!(unrle(&out), bytes);
        }
        let mut out = Vec::new();
        let mut rle = Rle::new(&mut out);
        rle.push(&[0; 1300]);
        rle.finish();
        assert_eq!(out.len(), 20, "flat areas cost 2 bytes per 130 pixels");
    }

    #[test]
    fn a_patch_carries_the_pixels_of_each_rectangle() {
        let f = image(64, 32);
        let rects = even_rects(
            &[
                Rect {
                    x: 3,
                    y: 1,
                    w: 10,
                    h: 5,
                },
                Rect {
                    x: 60,
                    y: 30,
                    w: 10,
                    h: 10,
                },
            ],
            64,
            32,
        );
        assert_eq!(
            rects,
            [
                Rect {
                    x: 2,
                    y: 0,
                    w: 12,
                    h: 6
                },
                Rect {
                    x: 60,
                    y: 30,
                    w: 4,
                    h: 2
                },
            ]
        );
        let p = encode(&f, &rects, u64::MAX, usize::MAX).unwrap();
        let got = decode(&p);
        assert_eq!(got.len(), 2);
        for (r, pixels) in got {
            let mut want = Vec::new();
            for row in r.y..r.y + r.h {
                want.extend_from_slice(&f.y[(row * 64 + r.x) as usize..][..r.w as usize]);
            }
            for plane in [&f.u, &f.v] {
                for row in r.y / 2..(r.y + r.h) / 2 {
                    want.extend_from_slice(
                        &plane[(row * 32 + r.x / 2) as usize..][..r.w as usize / 2],
                    );
                }
            }
            assert_eq!(pixels, want, "{r:?}");
        }
    }

    #[test]
    fn too_big_a_change_is_left_to_the_encoder() {
        let f = image(64, 32);
        let all = [Rect {
            x: 0,
            y: 0,
            w: 64,
            h: 32,
        }];
        assert!(encode(&f, &all, 64 * 32 - 1, usize::MAX).is_none(), "area");
        let p = encode(&f, &all, u64::MAX, usize::MAX).unwrap();
        assert!(encode(&f, &all, u64::MAX, p.len() - 1).is_none(), "bytes");
        assert!(encode(&f, &[], u64::MAX, usize::MAX).is_none());
        // Off-screen or empty damage is nothing to send.
        assert!(even_rects(
            &[
                Rect {
                    x: 70,
                    y: 0,
                    w: 5,
                    h: 5
                },
                Rect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 9
                }
            ],
            64,
            32
        )
        .is_empty());
        // Many rectangles: one bounding box.
        let many: Vec<Rect> = (0..40)
            .map(|i| Rect {
                x: i,
                y: i % 9,
                w: 1,
                h: 1,
            })
            .collect();
        assert_eq!(
            even_rects(&many, 64, 32),
            [Rect {
                x: 0,
                y: 0,
                w: 40,
                h: 10
            }]
        );
    }
}
