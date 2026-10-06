//! Writes the mock server's H.264 fixtures: OpenH264 2.6.0 streams encoded with tilt's encoder
//! settings (brief 5.7) from a tilt-testpattern look-alike. Each stream has 4 segments, one per
//! marker colour, each starting with an IDR, so the mock can "advance the marker" on input.
//!
//! Usage: cargo run --release -- <out_dir>
//! Output per stream: <name>.h264 (Annex B) and <name>.idx ("frame offset size frame_type segment").

use std::io::Write;
use std::path::Path;

use openh264_sys2::*;

const COLORS: [(u8, u8, u8); 4] = [(230, 40, 40), (40, 200, 60), (40, 80, 230), (240, 240, 240)];
const FRAMES_PER_SEGMENT: usize = 90;

fn yuv709(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    (
        (16.0 + 219.0 * y).round() as u8,
        (128.0 + 224.0 * cb).round() as u8,
        (128.0 + 224.0 * cr).round() as u8,
    )
}

struct I420 {
    w: usize,
    h: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420 {
    fn new(w: usize, h: usize) -> Self {
        I420 { w, h, y: vec![0; w * h], u: vec![0; w * h / 4], v: vec![0; w * h / 4] }
    }

    fn rect(&mut self, x0: usize, y0: usize, w: usize, h: usize, rgb: (u8, u8, u8)) {
        let (yy, uu, vv) = yuv709(rgb.0, rgb.1, rgb.2);
        for j in y0..(y0 + h).min(self.h) {
            for i in x0..(x0 + w).min(self.w) {
                self.y[j * self.w + i] = yy;
                if i % 2 == 0 && j % 2 == 0 {
                    self.u[(j / 2) * (self.w / 2) + i / 2] = uu;
                    self.v[(j / 2) * (self.w / 2) + i / 2] = vv;
                }
            }
        }
    }
}

/// The testpattern scene: #202020 background, the 256x256 marker at 64..320, a static band of
/// glyph-like blocks (so IDRs have a realistic size) and the --animate bar at y=400..560.
fn draw(f: &mut I420, color: usize, t: usize) {
    f.rect(0, 0, f.w, f.h, (0x20, 0x20, 0x20));
    f.rect(64, 64, 256, 256, COLORS[color]);
    let mut seed = 0x9e37_79b9u32;
    let mut rnd = |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed % n
    };
    let band = f.h - 140;
    for _ in 0..900 {
        let x = rnd(f.w as u32 - 12) as usize;
        let y = band + rnd(100) as usize;
        let shade = 120 + rnd(120) as u8;
        f.rect(x, y, 2 + rnd(7) as usize, 9, (shade, shade, shade));
    }
    let x = (t * 8) % f.w;
    f.rect(x, 400, 64, 160, (255, 255, 255));
}

struct Encoder {
    api: DynamicAPI,
    raw: *mut ISVCEncoder,
}

impl Encoder {
    fn new(w: usize, h: usize, cabac: bool) -> Encoder {
        // SAFETY: plain FFI calls on an encoder we create and own; parameters mirror brief 5.7.
        unsafe {
            let api = DynamicAPI::from_source();
            let mut raw: *mut ISVCEncoder = std::ptr::null_mut();
            assert_eq!(api.WelsCreateSVCEncoder(&mut raw), 0);
            let vt = &**raw;
            let mut p: SEncParamExt = std::mem::zeroed();
            (vt.GetDefaultParams.unwrap())(raw, &mut p);
            p.iUsageType = CAMERA_VIDEO_REAL_TIME;
            p.iComplexityMode = LOW_COMPLEXITY;
            p.iPicWidth = w as i32;
            p.iPicHeight = h as i32;
            p.fMaxFrameRate = 60.0;
            p.iRCMode = RC_BITRATE_MODE;
            p.bEnableFrameSkip = false;
            p.iTargetBitrate = 8_000_000;
            p.iMaxBitrate = 0;
            p.iMinQp = 20;
            p.iMaxQp = 28;
            p.iEntropyCodingModeFlag = cabac as i32;
            p.uiIntraPeriod = 0;
            p.eSpsPpsIdStrategy = CONSTANT_ID;
            p.iSpatialLayerNum = 1;
            p.iTemporalLayerNum = 1;
            p.iNumRefFrame = 1;
            p.bEnableLongTermReference = false;
            p.bEnableSceneChangeDetect = false;
            p.bEnableBackgroundDetection = true;
            p.bEnableAdaptiveQuant = true;
            p.bEnableDenoise = false;
            p.iLoopFilterDisableIdc = 0;
            p.iMultipleThreadIdc = 1;
            let l = &mut p.sSpatialLayers[0];
            l.iVideoWidth = w as i32;
            l.iVideoHeight = h as i32;
            l.fFrameRate = 60.0;
            l.iSpatialBitrate = 8_000_000;
            l.iMaxSpatialBitrate = 0;
            l.uiProfileIdc = if cabac { PRO_HIGH } else { PRO_BASELINE };
            l.sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
            l.bVideoSignalTypePresent = true;
            l.uiVideoFormat = 5;
            l.bFullRange = false;
            l.bColorDescriptionPresent = true;
            l.uiColorPrimaries = 1;
            l.uiTransferCharacteristics = 1;
            l.uiColorMatrix = 1;
            assert_eq!((vt.InitializeExt.unwrap())(raw, &p), 0, "InitializeExt");
            let mut fmt: i32 = videoFormatI420;
            (vt.SetOption.unwrap())(raw, ENCODER_OPTION_DATAFORMAT, &mut fmt as *mut i32 as *mut _);
            Encoder { api, raw }
        }
    }

    /// One access unit and its frame type; None when the encoder skipped the frame.
    fn encode(&mut self, f: &I420, ts_ms: i64, idr: bool) -> Option<(Vec<u8>, i32)> {
        // SAFETY: the picture borrows `f` only for the duration of EncodeFrame; NAL slices are
        // read from the encoder-owned buffer before the next call.
        unsafe {
            let vt = &**self.raw;
            if idr {
                (vt.ForceIntraFrame.unwrap())(self.raw, true);
            }
            let pic = SSourcePicture {
                iColorFormat: videoFormatI420,
                iStride: [f.w as i32, f.w as i32 / 2, f.w as i32 / 2, 0],
                pData: [f.y.as_ptr() as *mut u8, f.u.as_ptr() as *mut u8, f.v.as_ptr() as *mut u8, std::ptr::null_mut()],
                iPicWidth: f.w as i32,
                iPicHeight: f.h as i32,
                uiTimeStamp: ts_ms,
                bPsnrY: false,
                bPsnrU: false,
                bPsnrV: false,
            };
            let mut info: SFrameBSInfo = std::mem::zeroed();
            assert_eq!((vt.EncodeFrame.unwrap())(self.raw, &pic, &mut info), 0, "EncodeFrame");
            if info.eFrameType == videoFrameTypeSkip {
                return None;
            }
            let mut au = Vec::new();
            for li in 0..info.iLayerNum as usize {
                let l = &info.sLayerInfo[li];
                let mut len = 0usize;
                for ni in 0..l.iNalCount as usize {
                    len += *l.pNalLengthInByte.add(ni) as usize;
                }
                au.extend_from_slice(std::slice::from_raw_parts(l.pBsBuf, len));
            }
            Some((au, info.eFrameType))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from WelsCreateSVCEncoder and is destroyed exactly once.
        unsafe {
            ((**self.raw).Uninitialize.unwrap())(self.raw);
            self.api.WelsDestroySVCEncoder(self.raw);
        }
    }
}

fn write_stream(dir: &Path, name: &str, w: usize, h: usize, cabac: bool) {
    let mut enc = Encoder::new(w, h, cabac);
    let mut frame = I420::new(w, h);
    let mut data = Vec::new();
    let mut idx = String::new();
    let mut n = 0usize;
    for seg in 0..COLORS.len() {
        for t in 0..FRAMES_PER_SEGMENT {
            draw(&mut frame, seg, t);
            let ts = (n * 1000 / 60) as i64;
            if let Some((au, ftype)) = enc.encode(&frame, ts, t == 0) {
                assert!(t != 0 || ftype == videoFrameTypeIDR, "segment {seg} does not start with an IDR");
                idx.push_str(&format!("{n} {} {} {ftype} {seg}\n", data.len(), au.len()));
                data.extend_from_slice(&au);
            }
            n += 1;
        }
    }
    std::fs::File::create(dir.join(format!("{name}.h264"))).unwrap().write_all(&data).unwrap();
    std::fs::write(dir.join(format!("{name}.idx")), idx).unwrap();
    println!("{name}: {w}x{h} cabac={cabac} {} bytes", data.len());
}

fn main() {
    let out = std::env::args().nth(1).expect("usage: tilt-mock-fixtures <out_dir>");
    let dir = Path::new(&out);
    write_stream(dir, "high_1280x720", 1280, 720, true);
    write_stream(dir, "baseline_1280x720", 1280, 720, false);
    write_stream(dir, "high_1024x768", 1024, 768, true);
}
