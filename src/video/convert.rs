//! BGRX -> I420 (BT.709, limited range) and a small pool of frame buffers (brief section 5.8).

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{ensure, Context};
use yuv::{BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix};

/// Idle buffers a pool keeps. Capture fills one frame at a time and encoders let go of theirs
/// within a frame interval, so more would only hold memory.
const MAX_IDLE: usize = 4;

/// Planar 4:2:0 with tight strides: `width` for Y, `width / 2` for U and V. Both dims are even.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420Frame {
    /// Copies `other`'s pixels, which must have the same size.
    pub fn copy_from(&mut self, other: &I420Frame) {
        debug_assert_eq!((self.width, self.height), (other.width, other.height));
        self.y.copy_from_slice(&other.y);
        self.u.copy_from_slice(&other.u);
        self.v.copy_from_slice(&other.v);
    }

    /// Sizes the planes for `width` x `height`, reusing their allocations.
    fn reshape(&mut self, width: u32, height: u32) {
        let chroma = (width / 2) as usize * (height / 2) as usize;
        self.width = width;
        self.height = height;
        self.y.resize(width as usize * height as usize, 0);
        self.u.resize(chroma, 0);
        self.v.resize(chroma, 0);
    }
}

/// Recycles I420 buffers between capture and the encoders. Clones share one pool.
#[derive(Clone, Default)]
pub struct FramePool {
    idle: Arc<Mutex<Vec<I420Frame>>>,
}

impl FramePool {
    pub fn new() -> FramePool {
        FramePool::default()
    }

    /// Frees the idle buffers, for when nobody watches.
    pub fn trim(&self) {
        let idle = std::mem::take(&mut *self.idle.lock().unwrap_or_else(PoisonError::into_inner));
        drop(idle);
    }

    /// A `width`x`height` frame with unspecified contents, reusing an idle buffer if there is one.
    pub fn get(&self, width: u32, height: u32) -> PooledFrame {
        // The list stays consistent even if a holder of the lock panicked, so poison is ignored.
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let mut frame = idle.unwrap_or_default();
        frame.reshape(width, height);
        PooledFrame {
            frame,
            pool: self.clone(),
        }
    }
}

/// An I420Frame that goes back to its pool, which keeps at most 4 idle, when dropped.
pub struct PooledFrame {
    frame: I420Frame,
    pool: FramePool,
}

impl Deref for PooledFrame {
    type Target = I420Frame;

    fn deref(&self) -> &I420Frame {
        &self.frame
    }
}

impl DerefMut for PooledFrame {
    fn deref_mut(&mut self) -> &mut I420Frame {
        &mut self.frame
    }
}

impl Drop for PooledFrame {
    fn drop(&mut self) {
        let frame = std::mem::take(&mut self.frame);
        let mut idle = self
            .pool
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if idle.len() < MAX_IDLE {
            idle.push(frame);
        }
        // Otherwise `frame` is freed after the lock is released.
    }
}

/// Converts the top-left `dst.width` x `dst.height` of `src` (32-bpp B,G,R,X rows `src_stride`
/// bytes apart) into `dst`, cropping the odd column and row of odd screen sizes.
pub fn bgrx_to_i420(src: &[u8], src_stride: usize, dst: &mut I420Frame) -> anyhow::Result<()> {
    let height = dst.height;
    bgrx_rows_to_i420(src, src_stride, dst, 0, height)
}

/// Like [`bgrx_to_i420`] for rows `[top, bottom)` only, which must be even, leaving the rest of
/// `dst` as it was. `src` is the whole image, as for `bgrx_to_i420`.
pub fn bgrx_rows_to_i420(
    src: &[u8],
    src_stride: usize,
    dst: &mut I420Frame,
    top: u32,
    bottom: u32,
) -> anyhow::Result<()> {
    let (width, height) = (dst.width, dst.height);
    ensure!(
        width % 2 == 0 && height % 2 == 0,
        "I420 frame {width}x{height} has an odd dimension"
    );
    ensure!(
        (top | bottom) & 1 == 0 && top < bottom && bottom <= height,
        "rows {top}..{bottom} are not an even span of {height}"
    );
    let rows = bottom - top;
    let (w, cw) = (width as usize, width as usize / 2);
    let (top, bottom) = (top as usize, bottom as usize);
    let src = src
        .get(top * src_stride..)
        .context("source image ends before the rows to convert")?;
    let src_stride = u32::try_from(src_stride).context("source stride out of range")?;
    let short = || anyhow::anyhow!("I420 planes are too short for {width}x{height}");
    let y_plane = dst.y.get_mut(top * w..bottom * w).ok_or_else(short)?;
    let u_plane = dst
        .u
        .get_mut(top / 2 * cw..bottom / 2 * cw)
        .ok_or_else(short)?;
    let v_plane = dst
        .v
        .get_mut(top / 2 * cw..bottom / 2 * cw)
        .ok_or_else(short)?;
    let mut image = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y_plane),
        y_stride: width,
        u_plane: BufferStoreMut::Borrowed(u_plane),
        u_stride: width / 2,
        v_plane: BufferStoreMut::Borrowed(v_plane),
        v_stride: width / 2,
        width,
        height: rows,
    };
    // yuv checks the source length and stride and the plane sizes, and reports them as errors.
    // Its alpha input is X here, which a YUV conversion never reads.
    yuv::bgra_to_yuv420(
        &mut image,
        src,
        src_stride,
        YuvRange::Limited,
        YuvStandardMatrix::Bt709,
        YuvConversionMode::Balanced,
    )
    .with_context(|| format!("BGRX to I420 at {width}x{rows}, stride {src_stride}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w`x`h` BGRX image, `stride` bytes per row, filled by `px(x, y) -> [b, g, r]`. The
    /// padding and the X byte hold junk that must not reach the output.
    fn bgrx(w: usize, h: usize, stride: usize, px: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut buf = vec![0xa5; stride * h];
        for y in 0..h {
            for x in 0..w {
                let [b, g, r] = px(x, y);
                buf[y * stride + x * 4..][..4].copy_from_slice(&[b, g, r, 0x5a]);
            }
        }
        buf
    }

    fn frame(width: u32, height: u32) -> I420Frame {
        let mut f = I420Frame::default();
        f.reshape(width, height);
        f
    }

    fn assert_near(plane: &[u8], want: u8, what: &str) {
        for &got in plane {
            assert!(
                got.abs_diff(want) <= 2,
                "{what}: got {got}, want {want} +-2"
            );
        }
    }

    #[test]
    fn primaries_match_bt709_limited_range() {
        // Y = 16 + 219 E'Y, Cb = 128 + 224 (B' - E'Y) / 1.8556, Cr = 128 + 224 (R' - E'Y) / 1.5748.
        let cases = [
            ("white", [255, 255, 255], [235, 128, 128]),
            ("black", [0, 0, 0], [16, 128, 128]),
            ("red", [0, 0, 255], [63, 102, 240]),
            ("green", [0, 255, 0], [173, 42, 26]),
            ("blue", [255, 0, 0], [32, 240, 118]),
        ];
        for (name, bgr, [y, u, v]) in cases {
            let src = bgrx(16, 8, 16 * 4, |_, _| bgr);
            let mut dst = frame(16, 8);
            bgrx_to_i420(&src, 16 * 4, &mut dst).unwrap();
            assert_near(&dst.y, y, &format!("{name} Y"));
            assert_near(&dst.u, u, &format!("{name} U"));
            assert_near(&dst.v, v, &format!("{name} V"));
        }
    }

    #[test]
    fn crops_odd_sizes_and_honours_stride() {
        // A 7x5 grey ramp with padded rows, converted into its even 6x4 top-left corner.
        let grey = |x: usize, y: usize| (x * 9 + y * 50) as u8;
        let src = bgrx(7, 5, 7 * 4 + 12, |x, y| [grey(x, y); 3]);
        let mut dst = frame(6, 4);
        bgrx_to_i420(&src, 7 * 4 + 12, &mut dst).unwrap();
        for y in 0..4 {
            for x in 0..6 {
                let want = 16.0 + 219.0 * f64::from(grey(x, y)) / 255.0;
                let got = f64::from(dst.y[y * 6 + x]);
                assert!(
                    (got - want).abs() <= 1.5,
                    "Y({x},{y}) = {got}, want {want:.1}"
                );
            }
        }
        assert_near(&dst.u, 128, "grey U");
        assert_near(&dst.v, 128, "grey V");

        // A source ending right after the last pixel it needs is enough.
        let exact = &src[..(7 * 4 + 12) * 3 + 6 * 4];
        bgrx_to_i420(exact, 7 * 4 + 12, &mut dst).unwrap();
    }

    #[test]
    fn converting_rows_matches_a_full_conversion_and_leaves_the_rest() {
        let (w, h, stride) = (32, 24, 32 * 4 + 8);
        let before = bgrx(w, h, stride, |x, y| [(x * 7) as u8, (y * 9) as u8, 40]);
        let after = bgrx(w, h, stride, |x, y| {
            if (6..14).contains(&y) {
                [200, (x * 3) as u8, (y * 11) as u8]
            } else {
                [(x * 7) as u8, (y * 9) as u8, 40]
            }
        });
        let mut want = frame(w as u32, h as u32);
        bgrx_to_i420(&after, stride, &mut want).unwrap();
        let mut got = frame(w as u32, h as u32);
        bgrx_to_i420(&before, stride, &mut got).unwrap();
        bgrx_rows_to_i420(&after, stride, &mut got, 6, 14).unwrap();
        assert_eq!(got, want);

        let mut copy = frame(w as u32, h as u32);
        copy.copy_from(&got);
        assert_eq!(copy, got);

        for (top, bottom) in [(5, 14), (6, 13), (6, 6), (8, 6), (0, 26)] {
            assert!(
                bgrx_rows_to_i420(&after, stride, &mut got, top, bottom).is_err(),
                "rows {top}..{bottom}"
            );
        }
        let mut short = frame(w as u32, h as u32);
        short.u.truncate(10);
        assert!(bgrx_rows_to_i420(&after, stride, &mut short, 20, 24).is_err());
    }

    #[test]
    fn rejects_bad_geometry_without_panicking() {
        let src = bgrx(8, 8, 32, |_, _| [1, 2, 3]);
        assert!(
            bgrx_to_i420(&src, 32, &mut frame(7, 8)).is_err(),
            "odd width"
        );
        assert!(
            bgrx_to_i420(&src, 32, &mut frame(8, 7)).is_err(),
            "odd height"
        );
        assert!(bgrx_to_i420(&src, 32, &mut frame(0, 0)).is_err(), "empty");
        assert!(
            bgrx_to_i420(&src, 28, &mut frame(8, 8)).is_err(),
            "stride shorter than a row"
        );
        assert!(
            bgrx_to_i420(&src[..200], 32, &mut frame(8, 8)).is_err(),
            "short source"
        );
        assert!(
            bgrx_to_i420(&src, 32, &mut frame(10, 8)).is_err(),
            "wider than the source"
        );
        assert!(
            bgrx_to_i420(&src, 1 << 33, &mut frame(8, 8)).is_err(),
            "stride beyond u32"
        );
        let mut short = frame(8, 8);
        short.y.truncate(60);
        assert!(bgrx_to_i420(&src, 32, &mut short).is_err(), "short Y plane");
        let mut short = frame(8, 8);
        short.v.clear();
        assert!(
            bgrx_to_i420(&src, 32, &mut short).is_err(),
            "missing V plane"
        );
    }

    #[test]
    fn pool_reuses_buffers_and_keeps_four_idle() {
        let pool = FramePool::new();
        let first = pool.get(64, 32);
        assert_eq!((first.width, first.height), (64, 32));
        assert_eq!(
            (first.y.len(), first.u.len(), first.v.len()),
            (2048, 512, 512)
        );
        let y_ptr = first.y.as_ptr();
        drop(first);
        let again = pool.clone().get(64, 32);
        assert_eq!(again.y.as_ptr(), y_ptr, "the idle buffer is reused");
        drop(again);

        let frames: Vec<PooledFrame> = (0..6).map(|_| pool.get(64, 32)).collect();
        assert_eq!(pool.idle.lock().unwrap().len(), 0);
        drop(frames);
        assert_eq!(pool.idle.lock().unwrap().len(), MAX_IDLE);

        // After a resize, old buffers come back reshaped.
        let mut small = pool.get(16, 8);
        assert_eq!((small.y.len(), small.u.len(), small.v.len()), (128, 32, 32));
        let src = bgrx(16, 8, 64, |_, _| [255, 255, 255]);
        bgrx_to_i420(&src, 64, &mut small).unwrap();
        assert_near(&small.y, 235, "pooled Y");
        drop(small);
        assert_eq!(pool.idle.lock().unwrap().len(), MAX_IDLE);
        pool.trim();
        assert_eq!(pool.idle.lock().unwrap().len(), 0);
    }

    #[test]
    fn pooled_frames_cross_threads() {
        fn shareable<T: Send + Sync>() {}
        shareable::<PooledFrame>();
        shareable::<FramePool>();
    }
}
