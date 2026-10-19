//! Shared by the tilt end-to-end tools: the test pattern that tilt-testpattern draws and the
//! colour maths tilt-probe uses to recognise it in decoded video.

/// Marker colours, indexed by the event counter modulo 4: red, green, blue, white.
pub const COLORS: [(u8, u8, u8); 4] =
    [(230, 40, 40), (40, 200, 60), (40, 80, 230), (240, 240, 240)];
pub const BACKGROUND: (u8, u8, u8) = (0x20, 0x20, 0x20);
/// The marker covers x and y 64..320.
pub const MARKER_ORIGIN: i16 = 64;
pub const MARKER_SIZE: u16 = 256;

/// A colour further than this (Euclidean, 0..255 per channel) from every marker colour is
/// "no marker": codec noise on a flat block stays far below it, while the background, the
/// desktop or a half-drawn frame land above it.
const MAX_MATCH_DISTANCE: f32 = 60.0;

/// The index into [`COLORS`] nearest to `rgb`, or None when nothing is close enough.
pub fn nearest_color(rgb: (u8, u8, u8)) -> Option<usize> {
    let dist = |c: (u8, u8, u8)| {
        let d = |a: u8, b: u8| f32::from(a) - f32::from(b);
        (d(rgb.0, c.0).powi(2) + d(rgb.1, c.1).powi(2) + d(rgb.2, c.2).powi(2)).sqrt()
    };
    let (index, best) = COLORS
        .iter()
        .map(|&c| dist(c))
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(&b.1))?;
    (best <= MAX_MATCH_DISTANCE).then_some(index)
}

/// One limited-range BT.709 Y'CbCr sample to 8-bit RGB, the inverse of what tilt encodes.
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let y = (f32::from(y) - 16.0) * (255.0 / 219.0);
    let cb = (f32::from(u) - 128.0) * (255.0 / 224.0);
    let cr = (f32::from(v) - 128.0) * (255.0 / 224.0);
    let clamp = |x: f32| x.round().clamp(0.0, 255.0) as u8;
    (
        clamp(y + 1.5748 * cr),
        clamp(y - 0.1873 * cb - 0.4681 * cr),
        clamp(y + 1.8556 * cb),
    )
}

/// The Y'CbCr of pixel (`x`, `y`) in a tilt patch (the payload of a VIDEO message with
/// FLAG_PATCH; tilt's src/video/patch.rs), or None when no rectangle of it covers the pixel.
/// Malformed patches cover nothing.
pub fn patch_pixel(p: &[u8], x: u32, y: u32) -> Option<(u8, u8, u8)> {
    let u16_at = |i: usize| {
        Some(u32::from(u16::from_le_bytes(
            p.get(i..i + 2)?.try_into().ok()?,
        )))
    };
    let mut at = 2;
    let mut found = None;
    for _ in 0..u16_at(0)? {
        let (rx, ry, w, h) = (
            u16_at(at)?,
            u16_at(at + 2)?,
            u16_at(at + 4)?,
            u16_at(at + 6)?,
        );
        let len = u32::from_le_bytes(p.get(at + 8..at + 12)?.try_into().ok()?) as usize;
        let data = p.get(at + 12..at + 12 + len)?;
        at += 12 + len;
        if !(rx..rx + w).contains(&x) || !(ry..ry + h).contains(&y) {
            continue;
        }
        let pixels = unrle(data);
        let (lx, ly) = ((x - rx) as usize, (y - ry) as usize);
        let (w, h) = (w as usize, h as usize);
        let chroma = (ly / 2) * (w / 2) + lx / 2;
        // Later rectangles are drawn over earlier ones.
        found = Some((
            *pixels.get(ly * w + lx)?,
            *pixels.get(w * h + chroma)?,
            *pixels.get(w * h + w * h / 4 + chroma)?,
        ));
    }
    found
}

/// Undoes a patch's run-length code: c < 128, c + 1 literal bytes; c >= 128, a byte c - 125
/// times.
fn unrle(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let [c, rest @ ..] = b {
        if *c < 128 {
            let n = (usize::from(*c) + 1).min(rest.len());
            out.extend_from_slice(&rest[..n]);
            b = &rest[n..];
        } else {
            let Some(&v) = rest.first() else { break };
            out.extend(std::iter::repeat_n(v, usize::from(*c) - 125));
            b = &rest[1..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_pixels_come_from_the_rectangle_that_covers_them() {
        // Two 2x2 rectangles: (4, 2) solid Y 50 / U 60 / V 70 (run-length coded), then (4, 2)
        // again over it with literal bytes.
        let mut p = vec![2, 0];
        for (rect, data) in [
            ([4u16, 2, 2, 2], vec![129u8, 50, 0, 60, 0, 70]),
            ([4, 2, 2, 2], vec![5, 1, 2, 3, 4, 9, 8]),
        ] {
            for v in rect {
                p.extend_from_slice(&v.to_le_bytes());
            }
            p.extend_from_slice(&(data.len() as u32).to_le_bytes());
            p.extend_from_slice(&data);
        }
        assert_eq!(patch_pixel(&p, 5, 3), Some((4, 9, 8)));
        assert_eq!(patch_pixel(&p[..2 + 12 + 6], 5, 3), None, "truncated");
        p[0] = 1;
        assert_eq!(patch_pixel(&p, 5, 3), Some((50, 60, 70)));
        assert_eq!(patch_pixel(&p, 6, 3), None);
        assert_eq!(patch_pixel(&[], 0, 0), None);
    }

    /// Limited-range BT.709 forward transform, as tilt's converter does it.
    fn rgb_to_yuv((r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
        let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let q = |x: f32| x.round().clamp(0.0, 255.0) as u8;
        (
            q(16.0 + y * 219.0 / 255.0),
            q(128.0 + (b - y) / 1.8556 * 224.0 / 255.0),
            q(128.0 + (r - y) / 1.5748 * 224.0 / 255.0),
        )
    }

    #[test]
    fn yuv_round_trips_within_two_levels() {
        for rgb in COLORS
            .into_iter()
            .chain([BACKGROUND, (0, 0, 0), (255, 255, 255)])
        {
            let (y, u, v) = rgb_to_yuv(rgb);
            let back = yuv_to_rgb(y, u, v);
            for (a, b) in [(rgb.0, back.0), (rgb.1, back.1), (rgb.2, back.2)] {
                assert!(a.abs_diff(b) <= 2, "{rgb:?} -> {:?} -> {back:?}", (y, u, v));
            }
        }
    }

    #[test]
    fn known_limited_range_values() {
        assert_eq!(yuv_to_rgb(16, 128, 128), (0, 0, 0));
        assert_eq!(yuv_to_rgb(235, 128, 128), (255, 255, 255));
        // Pure red in BT.709 limited range is Y=63, Cb=102, Cr=240.
        let (r, g, b) = yuv_to_rgb(63, 102, 240);
        assert!(r >= 253 && g <= 2 && b <= 2, "{:?}", (r, g, b));
    }

    #[test]
    fn decoded_marker_colours_classify() {
        for (i, &c) in COLORS.iter().enumerate() {
            let (y, u, v) = rgb_to_yuv(c);
            assert_eq!(nearest_color(yuv_to_rgb(y, u, v)), Some(i));
            // Codec noise of a few levels on every plane must not change the answer.
            let noisy = yuv_to_rgb(
                y.saturating_add(4),
                u.saturating_sub(3),
                v.saturating_add(3),
            );
            assert_eq!(nearest_color(noisy), Some(i), "{c:?} noisy {noisy:?}");
        }
    }

    #[test]
    fn non_marker_colours_do_not_classify() {
        assert_eq!(nearest_color(BACKGROUND), None);
        assert_eq!(nearest_color((0, 0, 0)), None);
        assert_eq!(nearest_color((128, 128, 128)), None);
        assert_eq!(nearest_color((230, 200, 40)), None);
    }
}
