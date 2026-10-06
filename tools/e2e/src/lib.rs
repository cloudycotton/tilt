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

#[cfg(test)]
mod tests {
    use super::*;

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
