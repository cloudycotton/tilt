//! OpenH264 2.6 through its raw ISVCEncoder vtable (brief section 5.7). The openh264 crate's
//! safe wrapper cannot turn on CABAC, so we fill SEncParamExt ourselves.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::time::Instant;
use std::{ptr, slice};

use anyhow::{ensure, Context};
use openh264_sys2::{
    cmMallocMemeError, videoFormatI420, videoFrameTypeIDR, videoFrameTypeInvalid,
    videoFrameTypeSkip, DynamicAPI, ISVCEncoder, ISVCEncoderVtbl, SBitrateInfo, SEncParamExt,
    SEncoderStatistics, SFrameBSInfo, SSourcePicture, WelsTraceCallback, API,
    CAMERA_VIDEO_REAL_TIME, CM_BT709, CONSTANT_ID, CP_BT709, ENCODER_OPTION,
    ENCODER_OPTION_BITRATE, ENCODER_OPTION_DATAFORMAT, ENCODER_OPTION_GET_STATISTICS,
    ENCODER_OPTION_TRACE_CALLBACK, LOW_COMPLEXITY, PRO_BASELINE, PRO_HIGH, RC_BITRATE_MODE,
    SM_SINGLE_SLICE, SPATIAL_LAYER_ALL, TRC_BT709, UNSPECIFIED_BIT_RATE, VF_UNDEF, WELS_LOG_ERROR,
};

use super::convert::I420Frame;

/// OpenH264 refuses pictures with more pixels than its 36864 macroblocks per frame hold.
const MAX_PIXELS: u64 = 36_864 * 256;

/// OpenH264's lowest QP under rate control (GOM_MIN_QP_MODE): it quietly raises lower bounds.
pub const MIN_QP: u8 = 12;

/// H.264's highest QP.
pub const MAX_QP: u8 = 51;

thread_local! {
    /// The errors OpenH264 logged on this thread (its calls log synchronously, and
    /// iMultipleThreadIdc=1 keeps them on the caller's thread). The wrapper clears it before a
    /// call and attaches it to that call's error, so a failure is reported by whoever handles
    /// the error, as often as they choose, rather than by the log sink on every attempt.
    static OPENH264_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The lowest QP that is safe with CABAC. OpenH264 sizes its bitstream buffer to the raw
/// picture, and its CABAC writer never checks for room: full-swing noise overruns the buffer at
/// QP 19 (seen under Guard Malloc) and needs about 96% of it at QP 20. The CAVLC writer checks
/// before every macroblock and fails the frame instead.
pub const MIN_CABAC_QP: u8 = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub max_fps: f32,
    pub bitrate_bps: u32,
    pub min_qp: u8,
    pub max_qp: u8,
    /// High profile with CABAC; false means Constrained Baseline with CAVLC.
    pub cabac: bool,
    /// Lets rate control skip frames, which encode() reports as None.
    pub frame_skip: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// One Annex B access unit.
    pub data: Vec<u8>,
    /// The encoder produced an IDR, so SPS and PPS are included.
    pub keyframe: bool,
    pub encode_us: u32,
    /// The frame's mean quantizer.
    pub qp: u8,
}

/// One OpenH264 encoder. Dropping it must Uninitialize and WelsDestroySVCEncoder.
pub struct H264Encoder {
    api: DynamicAPI,
    raw: *mut ISVCEncoder,
    /// Reused output descriptor; boxed because it is large.
    info: Box<SFrameBSInfo>,
    settings: EncoderSettings,
}

// SAFETY: an OpenH264 encoder has no thread affinity; it must only not be used from two threads
// at once, which `&mut self` on every call guarantees. The raw pointer keeps the type !Sync.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    pub fn new(s: &EncoderSettings) -> anyhow::Result<Self> {
        check(s)?;
        let api = DynamicAPI::from_source();
        let mut raw = ptr::null_mut();
        // SAFETY: WelsCreateSVCEncoder stores a new encoder in `raw` or returns non-zero.
        let rv = unsafe { api.WelsCreateSVCEncoder(&mut raw) };
        ensure!(
            rv == 0 && !raw.is_null(),
            "WelsCreateSVCEncoder failed with {rv}"
        );
        // From here on Drop releases the encoder, whatever fails next.
        let mut encoder = H264Encoder {
            api,
            raw,
            info: Box::default(),
            settings: s.clone(),
        };
        encoder.initialize()?;
        tracing::debug!(settings = ?s, "H.264 encoder ready");
        Ok(encoder)
    }

    /// Like [`H264Encoder::new`], but the SPS signals a level that also covers
    /// `max_bitrate_bps`, so that [`set_bitrate`](Self::set_bitrate) may later go up to it.
    /// OpenH264 picks the lowest level that fits the size, frame rate and bitrate at
    /// InitializeExt and never revisits it (640x480 at 60 fps and 8 Mbps gets level 3.1, whose
    /// limit is 16.8 Mbps), so this initialises at the higher rate and then sets
    /// `s.bitrate_bps`. Rate control takes its start from that rate: it sets itself up at the
    /// first IDR.
    pub fn with_max_bitrate(s: &EncoderSettings, max_bitrate_bps: u32) -> anyhow::Result<Self> {
        let mut encoder = Self::new(&EncoderSettings {
            bitrate_bps: s.bitrate_bps.max(max_bitrate_bps),
            ..s.clone()
        })?;
        if encoder.settings.bitrate_bps != s.bitrate_bps {
            encoder.set_bitrate(s.bitrate_bps)?;
        }
        Ok(encoder)
    }

    /// Encodes one frame, as an IDR when `force_idr`. None when rate control skipped the frame
    /// or the encoder produced no output.
    ///
    /// After OpenH264 fails a frame, drop the encoder: a frame too big for its bitstream buffer
    /// (noise-like content at low QP) makes it free its state, and every later call fails.
    pub fn encode(
        &mut self,
        f: &I420Frame,
        timestamp_ms: u64,
        force_idr: bool,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        let s = &self.settings;
        ensure!(
            (f.width, f.height) == (s.width, s.height),
            "frame is {}x{} but the encoder is {}x{}",
            f.width,
            f.height,
            s.width,
            s.height
        );
        let luma = s.width as usize * s.height as usize;
        ensure!(
            f.y.len() >= luma && f.u.len() >= luma / 4 && f.v.len() >= luma / 4,
            "I420 planes are too short for {}x{}",
            s.width,
            s.height
        );
        // check() keeps both dims far below c_int::MAX.
        let (width, height) = (s.width as c_int, s.height as c_int);
        let vt = self.vtable();
        let start = Instant::now();
        if force_idr {
            let force_intra = vt
                .ForceIntraFrame
                .context("OpenH264 has no ForceIntraFrame")?;
            clear_openh264_error();
            // SAFETY: `raw` is a live, initialised encoder.
            let rv = unsafe { force_intra(self.raw, true) };
            let said = openh264_said(rv);
            ensure!(rv == 0, "ForceIntraFrame failed with {rv}{said}");
        }
        let picture = SSourcePicture {
            iColorFormat: videoFormatI420,
            iStride: [width, width / 2, width / 2, 0],
            // OpenH264 copies the planes into its own picture and never writes through these.
            pData: [
                f.y.as_ptr().cast_mut(),
                f.u.as_ptr().cast_mut(),
                f.v.as_ptr().cast_mut(),
                ptr::null_mut(),
            ],
            iPicWidth: width,
            iPicHeight: height,
            uiTimeStamp: i64::try_from(timestamp_ms).unwrap_or(i64::MAX),
            ..SSourcePicture::default()
        };
        let encode_frame = vt.EncodeFrame.context("OpenH264 has no EncodeFrame")?;
        clear_openh264_error();
        // SAFETY: `picture` describes planes at least as large as checked above, which outlive
        // the call; `info` is ours for OpenH264 to fill.
        let rv = unsafe { encode_frame(self.raw, &picture, &mut *self.info) };
        let said = openh264_said(rv);
        ensure!(
            rv != cmMallocMemeError,
            "EncodeFrame failed with {rv} (bitstream overflow or out of memory); OpenH264 has \
             dropped its state, so this encoder is unusable{said}"
        );
        ensure!(rv == 0, "EncodeFrame failed with {rv}{said}");
        let frame_type = self.info.eFrameType;
        if frame_type == videoFrameTypeSkip || frame_type == videoFrameTypeInvalid {
            tracing::debug!(frame_type, "OpenH264 produced no frame");
            return Ok(None);
        }
        let data = self.bitstream()?;
        if data.is_empty() {
            return Ok(None);
        }
        let encode_us = u32::try_from(start.elapsed().as_micros()).unwrap_or(u32::MAX);
        Ok(Some(EncodedFrame {
            data,
            keyframe: frame_type == videoFrameTypeIDR,
            encode_us,
            qp: self.last_qp(),
        }))
    }

    /// SetOption(ENCODER_OPTION_BITRATE) for all spatial layers.
    pub fn set_bitrate(&mut self, bps: u32) -> anyhow::Result<()> {
        // OpenH264 keeps a rate below one bit per frame as its target even as it rejects it.
        ensure!(
            bps as f32 >= self.settings.max_fps,
            "bitrate {bps} bps is below one bit per frame"
        );
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: c_int::try_from(bps).unwrap_or(c_int::MAX),
        };
        // SAFETY: ENCODER_OPTION_BITRATE takes an SBitrateInfo.
        unsafe { self.set_option(ENCODER_OPTION_BITRATE, &mut info) }?;
        self.settings.bitrate_bps = bps;
        Ok(())
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// The mean quantizer of the last frame encoded; MAX_QP when OpenH264 cannot say.
    fn last_qp(&self) -> u8 {
        let Some(get_option) = self.vtable().GetOption else {
            return MAX_QP;
        };
        let mut stats = SEncoderStatistics::default();
        // SAFETY: ENCODER_OPTION_GET_STATISTICS fills an SEncoderStatistics, which outlives
        // the call.
        let rv = unsafe {
            get_option(
                self.raw,
                ENCODER_OPTION_GET_STATISTICS,
                ptr::from_mut(&mut stats).cast::<c_void>(),
            )
        };
        if rv != 0 {
            return MAX_QP;
        }
        u8::try_from(stats.uiAverageFrameQP).map_or(MAX_QP, |qp| qp.min(MAX_QP))
    }

    fn initialize(&mut self) -> anyhow::Result<()> {
        // Trace options are the only ones OpenH264 takes before initialisation; setting the
        // callback first sends InitializeExt's own complaints to tracing too.
        let mut trace: WelsTraceCallback = Some(log_openh264);
        // SAFETY: ENCODER_OPTION_TRACE_CALLBACK takes a WelsTraceCallback.
        unsafe { self.set_option(ENCODER_OPTION_TRACE_CALLBACK, &mut trace) }?;

        let vt = self.vtable();
        let mut params = SEncParamExt::default();
        let get_defaults = vt
            .GetDefaultParams
            .context("OpenH264 has no GetDefaultParams")?;
        clear_openh264_error();
        // SAFETY: `params` is a valid SEncParamExt for OpenH264 to fill.
        let rv = unsafe { get_defaults(self.raw, &mut params) };
        let said = openh264_said(rv);
        ensure!(rv == 0, "GetDefaultParams failed with {rv}{said}");
        fill_params(&mut params, &self.settings);
        let initialize = vt.InitializeExt.context("OpenH264 has no InitializeExt")?;
        clear_openh264_error();
        // SAFETY: OpenH264 only reads `params`, which outlives the call.
        let rv = unsafe { initialize(self.raw, &params) };
        let said = openh264_said(rv);
        ensure!(
            rv == 0,
            "OpenH264 rejected {:?} (InitializeExt returned {rv}){said}",
            self.settings
        );

        let mut format: c_int = videoFormatI420;
        // SAFETY: ENCODER_OPTION_DATAFORMAT takes a c_int.
        unsafe { self.set_option(ENCODER_OPTION_DATAFORMAT, &mut format) }
    }

    /// A copy of the encoder's function table, so that calls can borrow `self` mutably.
    fn vtable(&self) -> ISVCEncoderVtbl {
        // SAFETY: `raw` points at the vtable pointer of an encoder that lives until Drop.
        unsafe { **self.raw }
    }

    /// SetOption with `value` as the option's argument.
    ///
    /// # Safety
    /// `T` must be the type OpenH264 reads for `option`.
    unsafe fn set_option<T>(
        &mut self,
        option: ENCODER_OPTION,
        value: &mut T,
    ) -> anyhow::Result<()> {
        let set_option = self
            .vtable()
            .SetOption
            .context("OpenH264 has no SetOption")?;
        clear_openh264_error();
        // SAFETY: the caller matches `T` to `option`, and `value` outlives the call.
        let rv = unsafe { set_option(self.raw, option, ptr::from_mut(value).cast::<c_void>()) };
        let said = openh264_said(rv);
        ensure!(
            rv == 0,
            "OpenH264 SetOption({option}) failed with {rv}{said}"
        );
        Ok(())
    }

    /// The NAL units of the last EncodeFrame, start codes included, as one buffer.
    fn bitstream(&self) -> anyhow::Result<Vec<u8>> {
        let info = &*self.info;
        let layers = usize::try_from(info.iLayerNum)
            .ok()
            .and_then(|n| info.sLayerInfo.get(..n))
            .context("OpenH264 reported a bad layer count")?;
        let mut data = Vec::with_capacity(usize::try_from(info.iFrameSizeInBytes).unwrap_or(0));
        for layer in layers {
            let nals = usize::try_from(layer.iNalCount).context("negative NAL count")?;
            if nals == 0 {
                continue;
            }
            ensure!(
                !layer.pNalLengthInByte.is_null() && !layer.pBsBuf.is_null(),
                "OpenH264 returned a layer without buffers"
            );
            // SAFETY: OpenH264 points pNalLengthInByte at iNalCount lengths, which stay valid
            // until the next EncodeFrame.
            let lengths = unsafe { slice::from_raw_parts(layer.pNalLengthInByte, nals) };
            let size = lengths
                .iter()
                .map(|&len| usize::try_from(len))
                .sum::<Result<usize, _>>()
                .context("negative NAL length")?;
            // SAFETY: a layer's NAL units lie back to back in pBsBuf, `size` bytes in all.
            data.extend_from_slice(unsafe { slice::from_raw_parts(layer.pBsBuf, size) });
        }
        Ok(data)
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        let vt = self.vtable();
        // SAFETY: `raw` is live until WelsDestroySVCEncoder and unused afterwards. Uninitialize
        // copes with an encoder whose InitializeExt failed or whose state a failed frame freed.
        unsafe {
            if let Some(uninitialize) = vt.Uninitialize {
                uninitialize(self.raw);
            }
            self.api.WelsDestroySVCEncoder(self.raw);
        }
    }
}

/// A frame size OpenH264 cannot encode at all, whatever the other settings: the error
/// [`H264Encoder::new`] returns for it, so that callers can tell it from a failure that a
/// retry might get past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedSize {
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Display for UnsupportedSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h) = (self.width, self.height);
        if w < 16 || h < 16 || w % 2 != 0 || h % 2 != 0 {
            write!(f, "frame size {w}x{h} must be even and at least 16x16")
        } else {
            write!(
                f,
                "frame size {w}x{h} is over OpenH264's 36864 macroblocks (about 4096x2304)"
            )
        }
    }
}

impl std::error::Error for UnsupportedSize {}

/// Rejects settings that OpenH264 would refuse, quietly replace or encode unsafely.
fn check(s: &EncoderSettings) -> anyhow::Result<()> {
    let (w, h) = (s.width, s.height);
    if w < 16 || h < 16 || w % 2 != 0 || h % 2 != 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(UnsupportedSize {
            width: w,
            height: h,
        }
        .into());
    }
    ensure!(
        (1.0..=60.0).contains(&s.max_fps),
        "max_fps {} is outside OpenH264's 1..=60",
        s.max_fps
    );
    ensure!(
        s.bitrate_bps as f32 >= s.max_fps,
        "bitrate {} bps is below one bit per frame",
        s.bitrate_bps
    );
    // Zero is no exception: OpenH264 would swap the range for its own 12..=42.
    ensure!(
        s.min_qp >= MIN_QP && s.min_qp <= s.max_qp && s.max_qp <= MAX_QP,
        "QP range {}..={} is not within OpenH264's {MIN_QP}..={MAX_QP}",
        s.min_qp,
        s.max_qp
    );
    ensure!(
        !s.cabac || s.min_qp >= MIN_CABAC_QP,
        "min_qp {} is below {MIN_CABAC_QP}, where OpenH264's CABAC writer can overrun its buffer",
        s.min_qp
    );
    Ok(())
}

/// Brief section 5.7 on top of OpenH264's defaults.
fn fill_params(p: &mut SEncParamExt, s: &EncoderSettings) {
    // check() keeps both dims far below c_int::MAX.
    let (width, height) = (s.width as c_int, s.height as c_int);
    let bitrate = c_int::try_from(s.bitrate_bps).unwrap_or(c_int::MAX);
    p.iUsageType = CAMERA_VIDEO_REAL_TIME;
    p.iComplexityMode = LOW_COMPLEXITY;
    p.iPicWidth = width;
    p.iPicHeight = height;
    p.fMaxFrameRate = s.max_fps;
    p.iRCMode = RC_BITRATE_MODE;
    p.bEnableFrameSkip = s.frame_skip;
    p.iTargetBitrate = bitrate;
    p.iMaxBitrate = UNSPECIFIED_BIT_RATE as c_int;
    p.iMinQp = c_int::from(s.min_qp);
    p.iMaxQp = c_int::from(s.max_qp);
    p.iEntropyCodingModeFlag = c_int::from(s.cabac);
    // IDRs only on request: no intra period and no scene-change detection.
    p.uiIntraPeriod = 0;
    p.bEnableSceneChangeDetect = false;
    p.eSpsPpsIdStrategy = CONSTANT_ID;
    p.iSpatialLayerNum = 1;
    p.iTemporalLayerNum = 1;
    p.iNumRefFrame = 1;
    p.bEnableLongTermReference = false;
    p.bEnableBackgroundDetection = true;
    // OpenH264 2.6 turns adaptive quantisation off in validation whatever this says.
    p.bEnableAdaptiveQuant = false;
    p.bEnableDenoise = false;
    p.iLoopFilterDisableIdc = 0;
    p.iMultipleThreadIdc = 1;

    let layer = &mut p.sSpatialLayers[0];
    layer.iVideoWidth = width;
    layer.iVideoHeight = height;
    layer.fFrameRate = s.max_fps;
    layer.iSpatialBitrate = bitrate;
    layer.iMaxSpatialBitrate = UNSPECIFIED_BIT_RATE as c_int;
    // OpenH264 codes Baseline with CAVLC whatever iEntropyCodingModeFlag says.
    layer.uiProfileIdc = if s.cabac { PRO_HIGH } else { PRO_BASELINE };
    layer.sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
    // BT.709 limited range, which is what bgrx_to_i420 produces.
    layer.bVideoSignalTypePresent = true;
    layer.uiVideoFormat = VF_UNDEF as u8;
    layer.bFullRange = false;
    layer.bColorDescriptionPresent = true;
    layer.uiColorPrimaries = CP_BT709 as u8;
    layer.uiTransferCharacteristics = TRC_BT709 as u8;
    layer.uiColorMatrix = CM_BT709 as u8;
}

fn clear_openh264_error() {
    OPENH264_ERROR.with(|e| e.borrow_mut().take());
}

/// The errors OpenH264 logged during the call that returned `rv` (since clear_openh264_error),
/// as " (OpenH264: <text>)" for that call's error message, or "". A call that succeeded gets "",
/// and errors it logged anyway are logged here, as nothing else reports them.
fn openh264_said(rv: c_int) -> String {
    match OPENH264_ERROR.with(|e| e.borrow_mut().take()) {
        Some(text) if rv != 0 => format!(" (OpenH264: {text})"),
        Some(text) => {
            tracing::warn!(target: "openh264", "{text}");
            String::new()
        }
        None => String::new(),
    }
}

/// OpenH264's log sink, all at debug level. Its warnings are routine here (rate control warns
/// at every start that it cannot hold the bitrate without frame skipping, and whenever frames
/// come slower than max_fps). Its errors mostly come with a failing call, whose error carries
/// the text (OPENH264_ERROR, openh264_said); logging them here as well would log a failing
/// encoder on every retry.
unsafe extern "C" fn log_openh264(_ctx: *mut c_void, level: c_int, message: *const c_char) {
    if message.is_null() {
        return;
    }
    // SAFETY: OpenH264 passes a NUL-terminated string that outlives the call.
    let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    // Drop the "[OpenH264] this = 0x..., Warning:" prefix.
    let text = message
        .strip_prefix("[OpenH264] ")
        .and_then(|rest| rest.split_once(':'))
        .map_or(&*message, |(_, text)| text.trim_start());
    tracing::debug!(target: "openh264", level, "{text}");
    if level == WELS_LOG_ERROR {
        // try_with: a panic here, on a thread whose locals are gone, would abort the process.
        let _ = OPENH264_ERROR.try_with(|e| match &mut *e.borrow_mut() {
            Some(said) => {
                said.push_str("; ");
                said.push_str(text.trim_end());
            }
            empty => *empty = Some(text.trim_end().to_owned()),
        });
    }
}

#[cfg(test)]
mod tests {
    use openh264_sys2::{
        dsErrorFree, ISVCDecoder, ISVCDecoderVtbl, SBufferInfo, SDecodingParam, VIDEO_BITSTREAM_AVC,
    };

    use super::*;
    use crate::video::convert::{bgrx_to_i420, FramePool};
    use crate::video::sps::{
        codec_string, nal_type, nal_units, parse_sps, SpsInfo, NAL_IDR, NAL_PPS, NAL_SPS,
    };

    /// nal_unit_type of a non-IDR slice.
    const NAL_SLICE: u8 = 1;

    fn settings(width: u32, height: u32, bitrate_bps: u32, cabac: bool) -> EncoderSettings {
        EncoderSettings {
            width,
            height,
            max_fps: 60.0,
            bitrate_bps,
            min_qp: 20,
            max_qp: 28,
            cabac,
            frame_skip: false,
        }
    }

    /// The timestamp of frame `i` at 60 fps.
    fn ts(i: usize) -> u64 {
        i as u64 * 1000 / 60
    }

    /// Encodes frame `i`, which rate control may not skip while frame skipping is off.
    fn encode(enc: &mut H264Encoder, frame: &I420Frame, i: usize, force_idr: bool) -> EncodedFrame {
        enc.encode(frame, ts(i), force_idr)
            .unwrap()
            .expect("frame skipping is off")
    }

    fn nal_types(data: &[u8]) -> Vec<u8> {
        nal_units(data).filter_map(nal_type).collect()
    }

    fn first_sps(data: &[u8]) -> SpsInfo {
        let nal = nal_units(data)
            .find(|nal| nal_type(nal) == Some(NAL_SPS))
            .expect("an SPS");
        parse_sps(nal).unwrap()
    }

    fn blank(width: u32, height: u32) -> I420Frame {
        let luma = width as usize * height as usize;
        I420Frame {
            width,
            height,
            y: vec![16; luma],
            u: vec![128; luma / 4],
            v: vec![128; luma / 4],
        }
    }

    /// Uniform noise in every plane: unlike any desktop, and unlike the frame before.
    fn noise(width: u32, height: u32, rng: &mut Rng) -> I420Frame {
        let luma = width as usize * height as usize;
        let mut plane = |len: usize| (0..len).map(|_| rng.next() as u8).collect::<Vec<u8>>();
        I420Frame {
            width,
            height,
            y: plane(luma),
            u: plane(luma / 4),
            v: plane(luma / 4),
        }
    }

    /// Mid grey with faint luma noise, new on every frame, like film grain.
    fn grain(width: u32, height: u32, rng: &mut Rng) -> I420Frame {
        let mut frame = blank(width, height);
        for px in &mut frame.y {
            *px = 120 + (rng.next() % 17) as u8;
        }
        frame
    }

    fn to_i420(bgrx: &[u8], width: u32, height: u32) -> I420Frame {
        let mut frame = blank(width, height);
        bgrx_to_i420(bgrx, width as usize * 4, &mut frame).unwrap();
        frame
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let sse: f64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| f64::from(x.abs_diff(y)).powi(2))
            .sum();
        10.0 * (255.0 * 255.0 * a.len() as f64 / sse.max(1.0)).log10()
    }

    #[test]
    fn keyframes_carry_parameter_sets_and_follow_force_idr() {
        let (w, h) = (320, 240);
        for cabac in [true, false] {
            let mut enc = H264Encoder::new(&settings(w, h, 1_000_000, cabac)).unwrap();
            let mut desktop = Desktop::new(w as usize, h as usize, 1);
            let mut screen = vec![0; w as usize * h as usize * 4];
            let mut got = Vec::new();
            for i in 0..8 {
                desktop.step(Scene::Text, i, &mut screen);
                let out = encode(&mut enc, &to_i420(&screen, w, h), i, i == 5);
                got.push((out.keyframe, nal_types(&out.data)));
            }
            let want: Vec<_> = (0..8)
                .map(|i| match i {
                    0 | 5 => (true, vec![NAL_SPS, NAL_PPS, NAL_IDR]),
                    _ => (false, vec![NAL_SLICE]),
                })
                .collect();
            assert_eq!(got, want, "cabac={cabac}");
        }
    }

    #[test]
    fn codec_strings_follow_profile_and_level() {
        let cases = [
            (1920, 1080, 8_000_000, true, "avc1.640C2A"),
            (1920, 1080, 8_000_000, false, "avc1.42C02A"),
            (1280, 720, 5_000_000, true, "avc1.640C20"),
            (1280, 720, 5_000_000, false, "avc1.42C020"),
        ];
        for (w, h, bps, cabac, want) in cases {
            let mut enc = H264Encoder::new(&settings(w, h, bps, cabac)).unwrap();
            let first = encode(&mut enc, &blank(w, h), 0, false);
            assert_eq!(
                codec_string(&first.data).as_deref(),
                Some(want),
                "{w}x{h} cabac={cabac}"
            );
        }
    }

    #[test]
    fn level_covers_the_max_bitrate() {
        // 640x480 at 60 fps fits level 3.1 (0x1F), whose 16.8 Mbps is below a 20 Mbps ceiling;
        // level 3.2 (0x20) allows 24 Mbps.
        let (w, h) = (640, 480);
        for (cabac, profile) in [(true, "640C"), (false, "42C0")] {
            let s = settings(w, h, 8_000_000, cabac);
            let mut plain = H264Encoder::new(&s).unwrap();
            let plain = codec_string(&encode(&mut plain, &blank(w, h), 0, false).data);
            assert_eq!(plain, Some(format!("avc1.{profile}1F")), "cabac={cabac}");

            let mut enc = H264Encoder::with_max_bitrate(&s, 20_000_000).unwrap();
            assert_eq!(enc.settings().bitrate_bps, 8_000_000);
            let first = encode(&mut enc, &blank(w, h), 0, false);
            let covered = Some(format!("avc1.{profile}20"));
            assert_eq!(codec_string(&first.data), covered, "cabac={cabac}");
            // Lower targets, and keyframes after them, keep the level.
            enc.set_bitrate(1_000_000).unwrap();
            let idr = encode(&mut enc, &blank(w, h), 1, true);
            assert!(idr.keyframe);
            assert_eq!(codec_string(&idr.data), covered, "cabac={cabac}");
        }
        // A maximum at or below the start rate changes nothing.
        let s = settings(w, h, 8_000_000, true);
        let mut enc = H264Encoder::with_max_bitrate(&s, 1_000_000).unwrap();
        assert_eq!(enc.settings(), &s);
        let first = encode(&mut enc, &blank(w, h), 0, false);
        assert_eq!(codec_string(&first.data).as_deref(), Some("avc1.640C1F"));
    }

    #[test]
    fn rate_control_starts_from_the_start_rate_under_a_higher_level() {
        // Faint grain fits 1 Mbps at QP 28 (see set_bitrate_changes_frame_size_both_ways), so
        // both encoders should settle near 2083 bytes a frame, including the first IDR's QP.
        let (w, h) = (320, 240);
        let s = settings(w, h, 1_000_000, true);
        let mut sizes = Vec::new();
        for mut enc in [
            H264Encoder::new(&s).unwrap(),
            H264Encoder::with_max_bitrate(&s, 20_000_000).unwrap(),
        ] {
            let mut rng = Rng(9);
            let frames: Vec<usize> = (0..40)
                .map(|i| {
                    encode(&mut enc, &grain(w, h, &mut rng), i, false)
                        .data
                        .len()
                })
                .collect();
            sizes.push((frames[0], frames[20..].iter().sum::<usize>() / 20));
        }
        println!(
            "(idr bytes, settled bytes/frame): plain {:?}, with max {:?}",
            sizes[0], sizes[1]
        );
        let (plain, covered) = (sizes[0], sizes[1]);
        assert!(covered.1.abs_diff(2083) < 2083 / 5, "{covered:?}");
        assert!(
            covered.1.abs_diff(plain.1) < plain.1 / 10,
            "{plain:?} vs {covered:?}"
        );
        assert!(
            covered.0.abs_diff(plain.0) < plain.0 / 10,
            "{plain:?} vs {covered:?}"
        );
    }

    #[test]
    fn sps_signals_bt709_limited_range_without_reordering() {
        // Not a multiple of 16, so the SPS crops as well.
        let (w, h) = (326, 246);
        for cabac in [true, false] {
            let mut enc = H264Encoder::new(&settings(w, h, 1_000_000, cabac)).unwrap();
            let sps = first_sps(&encode(&mut enc, &blank(w, h), 0, false).data);
            assert_eq!(sps.profile_idc, if cabac { 100 } else { 66 });
            assert_eq!((sps.width, sps.height), (w, h));
            assert!(sps.vui_parameters_present);
            assert_eq!(sps.video_full_range, Some(false));
            let colour = (
                sps.colour_primaries,
                sps.transfer_characteristics,
                sps.matrix_coefficients,
            );
            assert_eq!(colour, (Some(1), Some(1), Some(1)));
            // A decoder may show each picture as soon as it is decoded.
            assert_eq!(sps.max_num_reorder_frames, Some(0));
            assert_eq!(sps.max_dec_frame_buffering, Some(1));
        }
    }

    #[test]
    fn scene_cuts_do_not_trigger_idrs() {
        let (w, h) = (320, 240);
        let mut enc = H264Encoder::new(&settings(w, h, 2_000_000, true)).unwrap();
        let mut desktop = Desktop::new(w as usize, h as usize, 2);
        let mut screen = vec![0; w as usize * h as usize * 4];
        desktop.step(Scene::Text, 0, &mut screen);
        let calm = to_i420(&screen, w, h);
        let mut rng = Rng(3);
        let mut keyframes = Vec::new();
        for i in 0..40 {
            // A still desktop, then a hard cut to or from noise on every frame.
            let noisy;
            let frame = if i < 20 || i % 2 == 0 {
                &calm
            } else {
                noisy = noise(w, h, &mut rng);
                &noisy
            };
            if encode(&mut enc, frame, i, false).keyframe {
                keyframes.push(i);
            }
        }
        assert_eq!(keyframes, [0]);
    }

    #[test]
    fn set_bitrate_changes_frame_size_both_ways() {
        let (w, h) = (320, 240);
        let mut enc = H264Encoder::new(&settings(w, h, 1_000_000, true)).unwrap();
        let mut rng = Rng(4);
        let first = encode(&mut enc, &grain(w, h, &mut rng), 0, false);
        let codec = codec_string(&first.data);
        let mut i = 0;
        // The mean size of the last 10 of 30 frames, once rate control has settled. Faint grain
        // costs about 15 KB a frame at QP 20 yet fits 1 Mbps at QP 28, so no phase overshoots
        // into a debt that rate control would carry over into the next one.
        let mut settle = |enc: &mut H264Encoder| {
            let sizes: Vec<usize> = (0..30)
                .map(|_| {
                    i += 1;
                    encode(enc, &grain(w, h, &mut rng), i, false).data.len()
                })
                .collect();
            sizes[20..].iter().sum::<usize>() / 10
        };
        let low = settle(&mut enc);
        enc.set_bitrate(20_000_000).unwrap();
        assert_eq!(enc.settings().bitrate_bps, 20_000_000);
        let high = settle(&mut enc);
        enc.set_bitrate(1_000_000).unwrap();
        let low_again = settle(&mut enc);
        println!("bytes/frame: {low} at 1 Mbps, {high} at 20 Mbps, {low_again} at 1 Mbps");
        assert!(high > 3 * low.max(low_again));
        // 1 Mbps at 60 fps is 2083 bytes a frame.
        for size in [low, low_again] {
            assert!(
                size.abs_diff(2083) < 2083 / 5,
                "{size} bytes/frame at 1 Mbps"
            );
        }

        // The SPS keeps the level chosen at start, although 20 Mbps is beyond it.
        enc.set_bitrate(20_000_000).unwrap();
        let idr = encode(&mut enc, &grain(w, h, &mut rng), i + 1, true);
        assert!(idr.keyframe);
        assert_eq!(codec_string(&idr.data), codec);

        // Less than a bit per frame is refused and changes nothing.
        assert!(enc.set_bitrate(0).is_err());
        assert!(enc.set_bitrate(59).is_err());
        assert_eq!(enc.settings().bitrate_bps, 20_000_000);
    }

    #[test]
    fn static_reencodes_shrink_while_quality_rises() {
        let (w, h) = (640, 360);
        let mut enc = H264Encoder::new(&settings(w, h, 300_000, true)).unwrap();
        let mut dec = Decoder::new();
        let mut desktop = Desktop::new(w as usize, h as usize, 5);
        let mut screen = vec![0; w as usize * h as usize * 4];
        desktop.step(Scene::Text, 0, &mut screen);
        let still = to_i420(&screen, w, h);
        let mut sizes = Vec::new();
        let mut quality = Vec::new();
        for i in 0..30 {
            let out = encode(&mut enc, &still, i, false);
            let (_, _, y) = dec.decode(&out.data).unwrap();
            sizes.push(out.data.len());
            quality.push(psnr(&y, &still.y));
        }
        println!("sizes {sizes:?}\nY PSNR {quality:.1?}");
        let idr = sizes[0];
        assert!(
            sizes[1..].iter().all(|&size| size * 8 < idr),
            "a re-encode is not small"
        );
        assert!(
            sizes[20..].iter().all(|&size| size * 50 < idr),
            "re-encodes did not shrink"
        );
        assert_refines(&quality, 0.5);
    }

    /// The worker ends the refinement tail at the first re-encode of a still screen that comes
    /// out with the same size and quantizer as the one before, at the lowest quantizer. From
    /// there on, re-encodes must add no quality (they hover within a thousandth of a dB). At a
    /// low bitrate the quantizer stays above the floor, and the tail keeps refining.
    #[test]
    fn a_converged_tail_stays_converged() {
        let (w, h) = (640, 360);
        for (bitrate, scene) in [
            (8_000_000, Scene::Text),
            (8_000_000, Scene::Drag),
            (300_000, Scene::Text),
            (300_000, Scene::Drag),
        ] {
            let s = settings(w, h, bitrate, true);
            let mut enc = H264Encoder::new(&s).unwrap();
            let mut dec = Decoder::new();
            let mut desktop = Desktop::new(w as usize, h as usize, 7);
            let mut screen = vec![0; w as usize * h as usize * 4];
            for i in 0..20 {
                desktop.step(scene, i, &mut screen);
                let out = encode(&mut enc, &to_i420(&screen, w, h), i, false);
                assert!(out.qp >= s.min_qp && out.qp <= s.max_qp, "qp {}", out.qp);
                dec.decode(&out.data).unwrap();
            }
            let still = to_i420(&screen, w, h);
            let mut tail = Vec::new();
            for i in 20..60 {
                let out = encode(&mut enc, &still, i, false);
                let (_, _, y) = dec.decode(&out.data).unwrap();
                tail.push((out.data.len(), out.qp, psnr(&y, &still.y)));
            }
            let at = (1..tail.len()).find(|&i| {
                tail[i].1 <= s.min_qp && (tail[i].0, tail[i].1) == (tail[i - 1].0, tail[i - 1].1)
            });
            let what = format!("{bitrate} bps {scene:?}: {tail:?}");
            if let Some(at) = at {
                let best_later = tail[at..].iter().map(|t| t.2).fold(f64::MIN, f64::max);
                assert!(
                    best_later - tail[at].2 < 0.01,
                    "converged at {at} but refined later; {what}"
                );
            }
            if bitrate == 8_000_000 {
                assert!(at.is_some_and(|at| at < 5), "no early convergence; {what}");
            }
        }
    }

    #[test]
    fn tail_after_motion_refines_then_settles() {
        let (w, h) = (640, 360);
        let mut enc = H264Encoder::new(&settings(w, h, 300_000, true)).unwrap();
        let mut dec = Decoder::new();
        let mut desktop = Desktop::new(w as usize, h as usize, 5);
        let mut screen = vec![0; w as usize * h as usize * 4];
        // Dragging a window at 300 kbps keeps QP at its maximum of 28.
        let mut moving = Vec::new();
        for i in 0..30 {
            desktop.step(Scene::Drag, i, &mut screen);
            let out = encode(&mut enc, &to_i420(&screen, w, h), i, false);
            dec.decode(&out.data).unwrap();
            moving.push(out.data.len());
        }
        // Then the screen holds still and the worker's tail re-encodes it.
        let still = to_i420(&screen, w, h);
        let mut sizes = Vec::new();
        let mut quality = Vec::new();
        for i in 30..60 {
            let out = encode(&mut enc, &still, i, false);
            let (_, _, y) = dec.decode(&out.data).unwrap();
            sizes.push(out.data.len());
            quality.push(psnr(&y, &still.y));
        }
        println!("moving {moving:?}\ntail {sizes:?}\nY PSNR {quality:.1?}");
        let moving_mean = moving[1..].iter().sum::<usize>() / (moving.len() - 1);
        assert!(
            sizes[20..].iter().all(|&size| size * 3 < moving_mean),
            "the tail did not settle"
        );
        assert_refines(&quality, 1.0);
    }

    /// Re-encoding a still picture never makes it worse, and makes it `gain_db` better overall.
    fn assert_refines(quality: &[f64], gain_db: f64) {
        for pair in quality.windows(2) {
            assert!(
                pair[1] > pair[0] - 0.1,
                "Y PSNR fell from {:.2} to {:.2} dB",
                pair[0],
                pair[1]
            );
        }
        let (first, last) = (quality[0], quality[quality.len() - 1]);
        assert!(
            last >= first + gain_db,
            "Y PSNR went from {first:.2} to {last:.2} dB"
        );
    }

    #[test]
    fn decodes_back_to_the_source() {
        let (w, h) = (326, 246);
        for cabac in [true, false] {
            let mut enc = H264Encoder::new(&settings(w, h, 2_000_000, cabac)).unwrap();
            let mut dec = Decoder::new();
            let mut desktop = Desktop::new(w as usize, h as usize, 6);
            let mut screen = vec![0; w as usize * h as usize * 4];
            for i in 0..10 {
                desktop.step(Scene::Drag, i, &mut screen);
                let source = to_i420(&screen, w, h);
                let out = encode(&mut enc, &source, i, false);
                let (dw, dh, y) = dec.decode(&out.data).expect("a picture per access unit");
                assert_eq!((dw, dh), (w as usize, h as usize));
                let psnr = psnr(&y, &source.y);
                assert!(psnr > 40.0, "cabac={cabac} frame {i}: Y PSNR {psnr:.1} dB");
            }
        }
    }

    #[test]
    fn frame_skip_lets_rate_control_drop_frames() {
        let (w, h) = (320, 240);
        let s = EncoderSettings {
            frame_skip: true,
            ..settings(w, h, 200_000, true)
        };
        let mut enc = H264Encoder::new(&s).unwrap();
        let mut rng = Rng(7);
        let out: Vec<Option<EncodedFrame>> = (0..30)
            .map(|i| enc.encode(&noise(w, h, &mut rng), ts(i), false).unwrap())
            .collect();
        assert!(out[0].as_ref().is_some_and(|f| f.keyframe));
        assert!(out.iter().any(Option::is_none), "no frame was skipped");
    }

    #[test]
    fn rejects_bad_settings_and_frames() {
        let good = settings(64, 48, 500_000, true);
        let bad = [
            EncoderSettings {
                width: 62,
                height: 7,
                ..good.clone()
            },
            EncoderSettings {
                width: 14,
                ..good.clone()
            },
            EncoderSettings {
                width: 4096,
                height: 4096,
                ..good.clone()
            },
            EncoderSettings {
                max_fps: 0.0,
                ..good.clone()
            },
            EncoderSettings {
                max_fps: 61.0,
                ..good.clone()
            },
            EncoderSettings {
                max_fps: f32::NAN,
                ..good.clone()
            },
            EncoderSettings {
                bitrate_bps: 59,
                ..good.clone()
            },
            // Below the CABAC floor these are CAVLC, so only OpenH264's own floor rejects them.
            EncoderSettings {
                min_qp: 0,
                cabac: false,
                ..good.clone()
            },
            EncoderSettings {
                min_qp: 11,
                cabac: false,
                ..good.clone()
            },
            EncoderSettings {
                min_qp: 29,
                ..good.clone()
            },
            EncoderSettings {
                max_qp: 52,
                ..good.clone()
            },
            EncoderSettings {
                min_qp: 19,
                ..good.clone()
            },
            // OpenH264 itself refuses more than level 5.2 allows, in InitializeExt.
            EncoderSettings {
                bitrate_bps: u32::MAX,
                ..good.clone()
            },
        ];
        for s in &bad {
            let e = H264Encoder::new(s)
                .err()
                .unwrap_or_else(|| panic!("accepted {s:?}"));
            // Only the frame size is beyond any retry.
            let size = e.downcast_ref::<UnsupportedSize>();
            assert_eq!(size.is_some(), s.width != good.width, "{s:?}: {e:#}");
        }
        // CAVLC bounds its writes, so it may go below the CABAC floor.
        assert!(H264Encoder::new(&EncoderSettings {
            min_qp: 12,
            cabac: false,
            ..good.clone()
        })
        .is_ok());

        let mut enc = H264Encoder::new(&good).unwrap();
        assert!(enc.encode(&blank(64, 46), 0, false).is_err(), "wrong size");
        let mut short = blank(64, 48);
        short.v.truncate(10);
        assert!(enc.encode(&short, 0, false).is_err(), "short plane");
        // A refused frame leaves the encoder usable.
        assert!(encode(&mut enc, &blank(64, 48), 0, false).keyframe);
    }

    #[test]
    fn overflowing_frames_fail_cleanly() {
        // Full-swing noise needs more than the raw picture's size at QP 20. CAVLC, which checks
        // for room, fails the frame; CABAC would write past the buffer, so it is not tried here.
        let (w, h) = (320, 240);
        let s = EncoderSettings {
            max_qp: 20,
            ..settings(w, h, 1_000_000, false)
        };
        let mut enc = H264Encoder::new(&s).unwrap();
        let mut frame = noise(w, h, &mut Rng(8));
        for px in frame.y.iter_mut().chain(&mut frame.u).chain(&mut frame.v) {
            *px = if *px & 1 == 0 { 0 } else { 255 };
        }
        let e = enc.encode(&frame, ts(0), false).unwrap_err();
        // The error carries what OpenH264 logged about it, which the log sink keeps at debug.
        assert!(format!("{e:#}").contains("(OpenH264: "), "{e:#}");
        // OpenH264 has dropped its state, so even a blank frame fails now...
        assert!(enc.encode(&blank(w, h), ts(1), true).is_err());
        assert!(enc.set_bitrate(500_000).is_err());
        drop(enc);
        // ...but a new encoder is fine.
        let mut enc = H264Encoder::new(&s).unwrap();
        assert!(encode(&mut enc, &blank(w, h), 0, false).keyframe);
    }

    #[test]
    fn encoder_moves_between_threads() {
        let mut enc = H264Encoder::new(&settings(64, 48, 500_000, true)).unwrap();
        let worker = std::thread::spawn(move || {
            let first = encode(&mut enc, &blank(64, 48), 0, false);
            (enc, first)
        });
        let (mut enc, first) = worker.join().unwrap();
        assert!(first.keyframe);
        assert!(!encode(&mut enc, &blank(64, 48), 1, false).keyframe);
    }

    /// Prints p50/p95 of BGRX -> I420 conversion and of encoding, per resolution, scene and
    /// entropy coder. Run with
    /// `cargo test --release bench_convert_and_encode -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark; needs --release"]
    fn bench_convert_and_encode() {
        const FRAMES: usize = 180;
        let pool = FramePool::new();
        for (w, h, bitrate) in [(1920, 1080, 8_000_000), (1280, 720, 5_000_000)] {
            let mut screen = vec![0; w * h * 4];
            for scene in [Scene::Text, Scene::Scroll, Scene::Drag] {
                for cabac in [true, false] {
                    let mut desktop = Desktop::new(w, h, 42);
                    let s = settings(w as u32, h as u32, bitrate, cabac);
                    let mut enc = H264Encoder::new(&s).unwrap();
                    let (mut convert_ms, mut encode_ms) = (Vec::new(), Vec::new());
                    let (mut bytes, mut idrs, mut idr_ms) = (0, 0, 0.0);
                    for i in 0..FRAMES {
                        desktop.step(scene, i, &mut screen);
                        let mut frame = pool.get(w as u32, h as u32);
                        let start = Instant::now();
                        bgrx_to_i420(&screen, w * 4, &mut frame).unwrap();
                        convert_ms.push(millis(start));
                        let start = Instant::now();
                        let out = encode(&mut enc, &frame, i, false);
                        // The first frame is the IDR; the percentiles cover the rest.
                        match i {
                            0 => idr_ms = millis(start),
                            _ => encode_ms.push(millis(start)),
                        }
                        bytes += out.data.len();
                        idrs += usize::from(out.keyframe);
                    }
                    let (c50, c95) = p50_p95(convert_ms);
                    let (e50, e95) = p50_p95(encode_ms);
                    let scene = format!("{scene:?}");
                    println!(
                        "{w}x{h} {scene:<6} {}: convert p50 {c50:.2} p95 {c95:.2} ms | \
                         encode p50 {e50:.2} p95 {e95:.2} ms | {:.1} KB/frame | \
                         IDRs {idrs}, first {idr_ms:.1} ms",
                        if cabac { "CABAC" } else { "CAVLC" },
                        bytes as f64 / FRAMES as f64 / 1024.0,
                    );
                }
            }
        }
    }

    fn millis(start: Instant) -> f64 {
        start.elapsed().as_secs_f64() * 1e3
    }

    fn p50_p95(mut values: Vec<f64>) -> (f64, f64) {
        values.sort_by(f64::total_cmp);
        let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
        (at(0.5), at(0.95))
    }

    /// OpenH264's decoder, to show that the stream decodes, at once and faithfully.
    struct Decoder {
        api: DynamicAPI,
        raw: *mut ISVCDecoder,
    }

    impl Decoder {
        fn new() -> Decoder {
            let api = DynamicAPI::from_source();
            let mut raw = ptr::null_mut();
            // SAFETY: WelsCreateDecoder stores a new decoder in `raw` or returns non-zero.
            assert_eq!(unsafe { api.WelsCreateDecoder(&mut raw) }, 0);
            let decoder = Decoder { api, raw };
            let mut param = SDecodingParam::default();
            param.sVideoProperty.eVideoBsType = VIDEO_BITSTREAM_AVC;
            let initialize = decoder.vtable().Initialize.unwrap();
            // SAFETY: `raw` is a live decoder and `param` outlives the call.
            assert_eq!(unsafe { initialize(raw, &param) }, 0);
            decoder
        }

        fn vtable(&self) -> ISVCDecoderVtbl {
            // SAFETY: `raw` points at the vtable pointer of a decoder that lives until Drop.
            unsafe { **self.raw }
        }

        /// Decodes one access unit into the picture's size and tight Y plane, if one came out.
        fn decode(&mut self, au: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
            let mut planes = [ptr::null_mut(); 3];
            let mut info = SBufferInfo::default();
            let vt = self.vtable();
            let len = c_int::try_from(au.len()).unwrap();
            // SAFETY: `au` and the out-parameters outlive the call.
            let state = unsafe {
                vt.DecodeFrameNoDelay.unwrap()(
                    self.raw,
                    au.as_ptr(),
                    len,
                    planes.as_mut_ptr(),
                    &mut info,
                )
            };
            assert_eq!(state, dsErrorFree);
            if info.iBufferStatus != 1 {
                // OpenH264's decoder holds back one picture of any stream that is not Baseline,
                // whatever the SPS says about reordering (ReorderPicturesInDisplay). The
                // end-of-stream call inside DecodeFrameNoDelay lets FlushFrame hand it over.
                // SAFETY: the out-parameters outlive the call.
                let state =
                    unsafe { vt.FlushFrame.unwrap()(self.raw, planes.as_mut_ptr(), &mut info) };
                assert_eq!(state, dsErrorFree);
            }
            if info.iBufferStatus != 1 {
                return None;
            }
            // SAFETY: with iBufferStatus 1 the decoder filled sSystemBuffer, the union's only
            // member, for the picture in pDst.
            let picture = unsafe { info.UsrData.sSystemBuffer };
            let (w, h) = (picture.iWidth as usize, picture.iHeight as usize);
            let stride = picture.iStride[0] as usize;
            let mut y = Vec::with_capacity(w * h);
            for row in 0..h {
                // SAFETY: the Y plane has `h` rows of `stride` >= `w` bytes, valid until the next
                // decode call.
                y.extend_from_slice(unsafe {
                    slice::from_raw_parts(info.pDst[0].add(row * stride), w)
                });
            }
            Some((w, h, y))
        }
    }

    impl Drop for Decoder {
        fn drop(&mut self) {
            let vt = self.vtable();
            // SAFETY: `raw` is live until WelsDestroyDecoder and unused afterwards.
            unsafe {
                if let Some(uninitialize) = vt.Uninitialize {
                    uninitialize(self.raw);
                }
                self.api.WelsDestroyDecoder(self.raw);
            }
        }
    }

    /// xorshift64: cheap, repeatable test content.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 16) as u32
        }

        fn below(&mut self, n: usize) -> usize {
            self.next() as usize % n
        }
    }

    const GLYPH_W: usize = 8;
    const GLYPH_H: usize = 16;
    const LINE_H: usize = 18;
    const TITLE_H: usize = 30;

    /// Coverage of an 8x16 glyph cell.
    type Glyph = [u8; GLYPH_W * GLYPH_H];

    #[derive(Debug, Clone, Copy)]
    enum Scene {
        /// Typing into the editor.
        Text,
        /// The editor's document scrolling by 5 px a frame.
        Scroll,
        /// The editor window dragged diagonally, bouncing off the screen edges.
        Drag,
    }

    /// A synthetic desktop: a gradient wallpaper and an editor window whose document, rows of
    /// anti-aliased glyphs, is taller than the window so that it can scroll.
    struct Desktop {
        width: usize,
        height: usize,
        wallpaper: Vec<u8>,
        doc: Vec<u8>,
        doc_w: usize,
        doc_h: usize,
        win_w: usize,
        win_h: usize,
        glyphs: Vec<Glyph>,
        rng: Rng,
        /// Where typing goes: (column, line) of the document.
        caret: (usize, usize),
    }

    impl Desktop {
        fn new(width: usize, height: usize, seed: u64) -> Desktop {
            let mut rng = Rng(seed);
            let glyphs = make_glyphs(&mut rng);
            let mut wallpaper = vec![0; width * height * 4];
            for (i, px) in wallpaper.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let fx = (i % width) as f32 / width as f32;
                let fy = (i / width) as f32 / height as f32;
                let wave = (fx * 9.0).sin() * (fy * 7.0).cos() * 18.0;
                let (b, g, r) = (
                    120.0 + 90.0 * fy + wave,
                    70.0 + 60.0 * fx + wave,
                    90.0 - 50.0 * fy - wave,
                );
                *px = [b as u8, g as u8, r as u8, 0];
            }
            let (win_w, win_h) = ((width * 58 / 100) & !1, (height * 70 / 100) & !1);
            let (doc_w, doc_h) = (win_w - 2, height * 3);
            let mut doc = vec![255; doc_w * doc_h * 4];
            let cols = (doc_w - 16) / GLYPH_W;
            for line in 0..doc_h / LINE_H {
                // Indented words, mostly dark grey with some red, blue and green ones.
                let len = rng.below(cols);
                let mut col = rng.below(4) * 2;
                while col < len {
                    let colour = match rng.below(100) {
                        0..=77 => [30, 30, 30],
                        78..=87 => [200, 0, 0],
                        88..=93 => [21, 21, 163],
                        _ => [0, 128, 0],
                    };
                    for _ in 0..2 + rng.below(9) {
                        if col < len {
                            let glyph = &glyphs[rng.below(glyphs.len())];
                            let at = (8 + col * GLYPH_W, line * LINE_H + 1);
                            blend(&mut doc, (doc_w, doc_h), at, glyph, colour);
                            col += 1;
                        }
                    }
                    col += 1;
                }
            }
            Desktop {
                width,
                height,
                wallpaper,
                doc,
                doc_w,
                doc_h,
                win_w,
                win_h,
                glyphs,
                rng,
                // Halfway down the window, so that typing shows.
                caret: (4, (win_h - TITLE_H) / LINE_H / 2),
            }
        }

        /// Advances `scene` to frame `i` and draws the screen into `out` (BGRX).
        fn step(&mut self, scene: Scene, i: usize, out: &mut [u8]) {
            let home = (self.width / 8, self.height / 12);
            match scene {
                Scene::Text => {
                    self.type_glyph();
                    self.render(out, home, 0, true);
                }
                Scene::Scroll => self.render(out, home, i * 5, false),
                Scene::Drag => {
                    // 9 px right and 5 px down a frame, reflected at the screen edges.
                    let bounce = |start: usize, travel: usize, span: usize| {
                        let p = (start + travel) % (2 * span);
                        p.min(2 * span - p)
                    };
                    let x = bounce(home.0, i * 9, self.width - self.win_w);
                    let y = bounce(home.1, i * 5, self.height - self.win_h);
                    self.render(out, (x, y), 0, false);
                }
            }
        }

        /// Types one glyph at the caret, clearing each line before its first glyph.
        fn type_glyph(&mut self) {
            let (col, line) = self.caret;
            let row_bytes = self.doc_w * 4;
            if col == 4 {
                self.doc[line * LINE_H * row_bytes..(line + 1) * LINE_H * row_bytes].fill(255);
            }
            let glyph = self.glyphs[self.rng.below(self.glyphs.len())];
            let at = (8 + col * GLYPH_W, line * LINE_H + 1);
            blend(
                &mut self.doc,
                (self.doc_w, self.doc_h),
                at,
                &glyph,
                [30, 30, 30],
            );
            let cols = ((self.doc_w - 16) / GLYPH_W).min(90);
            self.caret = if col + 1 < cols {
                (col + 1, line)
            } else {
                (4, (line + 1) % (self.doc_h / LINE_H))
            };
        }

        /// Draws the wallpaper and the window at (x, y), its document scrolled down by `scroll`
        /// pixels and, if `caret`, the caret. The window must fit on the screen.
        fn render(&self, out: &mut [u8], (x, y): (usize, usize), scroll: usize, caret: bool) {
            let (w, h) = (self.width, self.height);
            out.copy_from_slice(&self.wallpaper);
            for row in y..y + self.win_h {
                let line = &mut out[(row * w + x) * 4..(row * w + x + self.win_w) * 4];
                if row < y + TITLE_H {
                    line.fill(0xdd);
                } else {
                    let doc_row = (row - y - TITLE_H + scroll) % self.doc_h;
                    let text = &self.doc[doc_row * self.doc_w * 4..][..self.doc_w * 4];
                    line[4..4 + text.len()].copy_from_slice(text);
                    // A grey border on either side.
                    line[..4].fill(0x88);
                    line[4 + text.len()..].fill(0x88);
                }
            }
            for (i, &glyph) in [3, 17, 42, 8, 66, 12, 5, 90, 33].iter().enumerate() {
                let at = (x + 12 + i * GLYPH_W, y + 7);
                blend(out, (w, h), at, &self.glyphs[glyph], [20, 20, 20]);
            }
            if caret {
                let (col, line) = self.caret;
                let (left, top) = (x + 9 + col * GLYPH_W, y + TITLE_H + line * LINE_H);
                for row in top..(top + GLYPH_H).min(y + self.win_h) {
                    out[(row * w + left) * 4..(row * w + left + 2) * 4].fill(0);
                }
            }
        }
    }

    /// 96 glyphs of two to four random strokes, softened like anti-aliased text.
    fn make_glyphs(rng: &mut Rng) -> Vec<Glyph> {
        (0..96)
            .map(|_| {
                let mut ink = [0f32; GLYPH_W * GLYPH_H];
                for _ in 0..2 + rng.below(3) {
                    let (x0, y0) = (1 + rng.below(6), 3 + rng.below(10));
                    let (x1, y1) = (1 + rng.below(6), 3 + rng.below(10));
                    let steps = x0.abs_diff(x1).max(y0.abs_diff(y1)).max(1);
                    for s in 0..=steps {
                        let x = (x0 * (steps - s) + x1 * s) / steps;
                        let y = (y0 * (steps - s) + y1 * s) / steps;
                        ink[y * GLYPH_W + x] = 1.0;
                    }
                }
                let at = |x: usize, y: usize| {
                    if x < GLYPH_W && y < GLYPH_H {
                        ink[y * GLYPH_W + x]
                    } else {
                        0.0
                    }
                };
                let mut glyph = [0; GLYPH_W * GLYPH_H];
                for (i, coverage) in glyph.iter_mut().enumerate() {
                    let (x, y) = (i % GLYPH_W, i / GLYPH_W);
                    let around = at(x + 1, y)
                        + at(x.wrapping_sub(1), y)
                        + at(x, y + 1)
                        + at(x, y.wrapping_sub(1));
                    *coverage = ((0.7 * at(x, y) + 0.12 * around).min(1.0) * 255.0) as u8;
                }
                glyph
            })
            .collect()
    }

    /// Blends `glyph` in `colour` (B, G, R) into a BGRX image of `size` at `at`, clipped.
    fn blend(
        img: &mut [u8],
        size: (usize, usize),
        at: (usize, usize),
        glyph: &Glyph,
        colour: [u8; 3],
    ) {
        for gy in 0..GLYPH_H.min(size.1.saturating_sub(at.1)) {
            for gx in 0..GLYPH_W.min(size.0.saturating_sub(at.0)) {
                let a = u32::from(glyph[gy * GLYPH_W + gx]);
                let px = &mut img[((at.1 + gy) * size.0 + at.0 + gx) * 4..][..3];
                for (c, ink) in px.iter_mut().zip(colour) {
                    *c = ((u32::from(*c) * (255 - a) + u32::from(ink) * a) / 255) as u8;
                }
            }
        }
    }
}
