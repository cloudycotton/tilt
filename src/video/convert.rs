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
    let (width, height) = (dst.width, dst.height);
    ensure!(
        width % 2 == 0 && height % 2 == 0,
        "I420 frame {width}x{height} has an odd dimension"
    );
    let src_stride = u32::try_from(src_stride).context("source stride out of range")?;
    let mut image = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(&mut dst.y),
        y_stride: width,
        u_plane: BufferStoreMut::Borrowed(&mut dst.u),
        u_stride: width / 2,
        v_plane: BufferStoreMut::Borrowed(&mut dst.v),
        v_stride: width / 2,
        width,
        height,
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
    .with_context(|| format!("BGRX to I420 at {width}x{height}, stride {src_stride}"))
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
    }

    #[test]
    fn pooled_frames_cross_threads() {
        fn shareable<T: Send + Sync>() {}
        shareable::<PooledFrame>();
        shareable::<FramePool>();
    }
}
