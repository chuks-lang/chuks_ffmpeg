// chuks_ffmpeg_shim — C-ABI bridge over FFmpeg (the ffmpeg-next crate / libav).
//
// Foundation for @chuks/ffmpeg. Exports are prefixed `chuks_ffmpeg_` so they can
// never collide with libav's own `av_*` / `avcodec_*` symbols linked in (see the
// whisper-shim symbol-collision lesson). FFmpeg is compiled from source and
// STATIC-linked (Cargo `build` feature), so this cdylib is self-contained.
//
// All ffmpeg-next API used here is verified against the crate's docs.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use ffmpeg_next as ff;
use ff::format::Pixel;
use ff::media::Type as MediaType;
use ff::software::scaling::{context::Context as Scaler, flag::Flags as ScaleFlags};
use ff::util::frame::audio::Audio as AudioFrame;
use ff::util::frame::video::Video as VideoFrame;

thread_local! {
    static LAST_ERR: RefCell<String> = RefCell::new(String::new());
    // Name of the video encoder resolve_video_encoder() last selected, so the
    // caller can tell whether it got hardware (e.g. h264_videotoolbox) or a
    // software fallback for the current platform/build.
    static LAST_VENC: RefCell<String> = RefCell::new(String::new());
}
fn set_err(s: impl Into<String>) {
    LAST_ERR.with(|e| *e.borrow_mut() = s.into());
}

// Resolve a codec FAMILY to the best encoder available in THIS build on THIS
// platform, trying hardware first then software, so callers ask for "h264"
// rather than naming a platform-specific encoder. On macOS "h264" finds
// h264_videotoolbox; on a Linux build with VAAPI/NVENC it finds those; MPEG4 /
// MJPEG are portable native encoders. Falls back to treating `family` as a
// literal encoder name (back-compat). Records the pick in LAST_VENC.
fn resolve_video_encoder(family: &str) -> Option<ff::codec::codec::Codec> {
    let candidates: &[&str] = match family {
        "h264" => &[
            "h264_videotoolbox",
            "h264_nvenc",
            "h264_vaapi",
            "h264_qsv",
            "h264_amf",
            "libx264",
            "libopenh264",
        ],
        "hevc" => &[
            "hevc_videotoolbox",
            "hevc_nvenc",
            "hevc_vaapi",
            "hevc_qsv",
            "hevc_amf",
            "libx265",
        ],
        "mpeg4" => &["mpeg4"],
        "mjpeg" => &["mjpeg"],
        _ => &[],
    };
    for name in candidates {
        if let Some(c) = ff::encoder::find_by_name(name) {
            LAST_VENC.with(|v| *v.borrow_mut() = (*name).to_string());
            return Some(c);
        }
    }
    // Fall back to an explicit encoder name.
    if let Some(c) = ff::encoder::find_by_name(family) {
        LAST_VENC.with(|v| *v.borrow_mut() = family.to_string());
        return Some(c);
    }
    None
}

/// Name of the video encoder the last transcode() actually used (empty if none).
/// Caller frees with `chuks_ffmpeg_free_string`.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_last_video_encoder() -> *mut c_char {
    into_cstring(LAST_VENC.with(|v| v.borrow().clone()))
}

/// 1 if `name` is an encoder compiled into this build, else 0. Lets callers
/// probe hardware availability (e.g. "h264_vaapi", "h264_nvenc") without
/// attempting an encode / needing a device.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_has_encoder(name: *const c_char) -> c_int {
    let _ = ff::init();
    match unsafe { cstr(name) } {
        Some(n) if ff::encoder::find_by_name(n).is_some() => 1,
        _ => 0,
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

fn into_cstring(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

/// Last error message (empty if none). Caller frees with `chuks_ffmpeg_free_string`.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_last_error() -> *mut c_char {
    into_cstring(LAST_ERR.with(|e| e.borrow().clone()))
}

/// FFmpeg build version string. Caller frees with `chuks_ffmpeg_free_string`.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_version() -> *mut c_char {
    let _ = ff::init();
    let s: String = unsafe {
        let p = ff::ffi::av_version_info();
        if p.is_null() {
            "unknown".to_string()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    into_cstring(s)
}

/// Free a string previously returned by this library.
///
/// # Safety
/// `p` must be null or a pointer returned by this library and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn chuks_ffmpeg_free_string(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

// ─── Audio tap ───────────────────────────────────────────────────────────────
//
// Decode a file's best audio stream and resample it to 16 kHz MONO PCM in the
// requested float format, accumulated into one owned, packed buffer. That buffer
// is exposed as a BORROWED pointer so consumers read it zero-copy: @chuks/whisper
// (f32) or any future f64 consumer. The resampler is built lazily from the first
// decoded frame, so we depend only on verified frame accessors.

/// Output sample-format codes (stable ABI).
const CHUKS_FMT_F32: c_int = 0;
const CHUKS_FMT_F64: c_int = 1;
const TARGET_RATE: u32 = 16_000;

/// Owns the decoded 16 kHz mono PCM. `fmt` selects which buffer is populated.
pub struct AudioBuf {
    fmt: c_int,
    f32: Vec<f32>,
    f64: Vec<f64>,
}

impl AudioBuf {
    fn len(&self) -> i64 {
        match self.fmt {
            CHUKS_FMT_F64 => self.f64.len() as i64,
            _ => self.f32.len() as i64,
        }
    }
}

fn append_samples(out: &mut AudioBuf, frame: &AudioFrame) {
    if frame.samples() == 0 {
        return;
    }
    // 16 kHz MONO packed => plane 0 holds `samples()` interleaved values.
    match out.fmt {
        CHUKS_FMT_F64 => out.f64.extend_from_slice(frame.plane::<f64>(0)),
        _ => out.f32.extend_from_slice(frame.plane::<f32>(0)),
    }
}

/// Decode `path`'s best audio stream to 16 kHz mono PCM in float format `fmt`
/// (0 = f32, 1 = f64). Returns a handle, or null on error (see last_error).
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_audio_decode(path: *const c_char, fmt: c_int) -> *mut AudioBuf {
    let path = match unsafe { cstr(path) } {
        Some(s) => s.to_string(),
        None => {
            set_err("audio_decode: null/invalid path");
            return ptr::null_mut();
        }
    };
    match audio_decode_inner(&path, fmt) {
        Ok(buf) => Box::into_raw(Box::new(buf)),
        Err(e) => {
            set_err(format!("audio_decode: {e}"));
            ptr::null_mut()
        }
    }
}

fn audio_decode_inner(path: &str, fmt: c_int) -> Result<AudioBuf, ff::Error> {
    let _ = ff::init();
    // Normalize any unexpected code to f32 (whisper's format).
    let fmt = if fmt == CHUKS_FMT_F64 { CHUKS_FMT_F64 } else { CHUKS_FMT_F32 };

    let mut ictx = ff::format::input(&path)?;
    let stream_index = ictx
        .streams()
        .best(MediaType::Audio)
        .ok_or(ff::Error::StreamNotFound)?
        .index();

    let params = ictx.stream(stream_index).unwrap().parameters();
    let mut decoder = ff::codec::context::Context::from_parameters(params)?
        .decoder()
        .audio()?;

    // Resample via a FILTER GRAPH (abuffer -> anull -> abuffersink), not the
    // swresample Context: under FFmpeg 8's new channel-layout API the Context
    // path errors with "Input changed", but the filter layer negotiates layouts
    // correctly (this is exactly what the crate's transcode-audio example does).
    // The sink is constrained to 16 kHz MONO in the requested float format, so
    // validate() auto-inserts the resampler/format-converter.
    let mut filter = ff::filter::Graph::new();
    // FFmpeg 8's new channel-layout API makes ffmpeg-next's OLD ChannelLayout
    // (bits()/MONO) unreliable (it reports 0x0), so drive layouts entirely by
    // STRING: a named input layout for abuffer, and an aresample+aformat chain
    // that names the mono output. No old channel-layout API is touched anywhere.
    let in_layout = match decoder.channels() {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{}c", n),
    };
    let args = format!(
        "time_base=1/{rate}:sample_rate={rate}:sample_fmt={fmt}:channel_layout={layout}",
        rate = decoder.rate(),
        fmt = decoder.format().name(),
        layout = in_layout,
    );
    filter.add(&ff::filter::find("abuffer").unwrap(), "in", &args)?;
    filter.add(&ff::filter::find("abuffersink").unwrap(), "out", "")?;
    // Packed f32 = "flt", packed f64 = "dbl"; aresample handles rate + downmix.
    let out_sample_fmt = if fmt == CHUKS_FMT_F64 { "dbl" } else { "flt" };
    let spec = format!(
        "aresample={rate},aformat=sample_fmts={sfmt}:channel_layouts=mono",
        rate = TARGET_RATE,
        sfmt = out_sample_fmt,
    );
    filter.output("in", 0)?.input("out", 0)?.parse(&spec)?;
    filter.validate()?;

    let mut out = AudioBuf {
        fmt,
        f32: Vec::new(),
        f64: Vec::new(),
    };
    let mut frame = AudioFrame::empty();
    let mut filtered = AudioFrame::empty();

    // Pull every 16 kHz mono frame the sink has ready.
    macro_rules! pull {
        () => {{
            while filter
                .get("out")
                .unwrap()
                .sink()
                .frame(&mut filtered)
                .is_ok()
            {
                append_samples(&mut out, &filtered);
            }
        }};
    }
    // Feed each decoded frame into the graph, then drain what comes out.
    macro_rules! drain {
        () => {{
            while decoder.receive_frame(&mut frame).is_ok() {
                filter.get("in").unwrap().source().add(&frame)?;
                pull!();
            }
        }};
    }

    for (stream, packet) in ictx.packets() {
        if stream.index() == stream_index {
            decoder.send_packet(&packet)?;
            drain!();
        }
    }
    decoder.send_eof()?;
    drain!();
    filter.get("in").unwrap().source().flush()?;
    pull!();

    Ok(out)
}

/// Number of 16 kHz mono samples decoded, or -1 on a null handle.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_audio_samples(buf: *const AudioBuf) -> i64 {
    if buf.is_null() {
        return -1;
    }
    unsafe { (*buf).len() }
}

/// Borrowed pointer to the packed PCM buffer (f32 or f64 per how it was decoded).
/// Valid until `chuks_ffmpeg_audio_free`. Consumers read it zero-copy.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_audio_ptr(buf: *const AudioBuf) -> *const c_void {
    if buf.is_null() {
        return ptr::null();
    }
    let b = unsafe { &*buf };
    match b.fmt {
        CHUKS_FMT_F64 => b.f64.as_ptr() as *const c_void,
        _ => b.f32.as_ptr() as *const c_void,
    }
}

/// Output float-format code (0 = f32, 1 = f64), or -1 on a null handle.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_audio_format(buf: *const AudioBuf) -> c_int {
    if buf.is_null() {
        return -1;
    }
    unsafe { (*buf).fmt }
}

/// Free an audio buffer handle.
///
/// # Safety
/// `buf` must be null or a handle from `chuks_ffmpeg_audio_decode`, not yet freed.
#[no_mangle]
pub unsafe extern "C" fn chuks_ffmpeg_audio_free(buf: *mut AudioBuf) {
    if !buf.is_null() {
        drop(Box::from_raw(buf));
    }
}

// ─── Video reader ────────────────────────────────────────────────────────────
//
// Frame-by-frame video decode → RGB24, exposed as a BORROWED pointer per frame
// for zero-copy into @chuks/onnx (which reads the RGB buffer directly to build a
// tensor). Streaming: each next() yields one frame; the borrowed pointer + stride
// are valid until the following next() / free. Packets are pulled one at a time
// via ictx.packets().next() (the read position lives in the format context, so a
// fresh iterator per call advances correctly and holds no long-lived borrow).

pub struct VideoReader {
    ictx: ff::format::context::Input,
    decoder: ff::codec::decoder::Video,
    scaler: Option<Scaler>,
    stream_index: usize,
    frame: VideoFrame,
    rgb: VideoFrame,
    eof_sent: bool,
    has_frame: bool,
    // Optional filtergraph. Empty spec => straight swscale (scaler path). A
    // non-empty spec (e.g. "scale=1280:720", "crop=...", "hflip", "fps=15",
    // "drawtext=...") runs each decoded frame through buffer -> spec ->
    // format=rgb24 -> buffersink. Built lazily from the first frame (needs its
    // real width/height/pix_fmt). Pull-driven so filters that change the frame
    // count (fps) work. Output stays RGB24, so consumers are unaffected.
    filter_spec: String,
    filter: Option<ff::filter::Graph>,
    filter_flushed: bool,
    time_base: ff::Rational,
    frame_rate: ff::Rational,
    // Presentation timestamp of the current frame, in stream time_base units
    // (None until the first frame). Exposed as seconds via video_pts for seek UIs.
    cur_pts: Option<i64>,
    // Precise-seek target (stream time_base units): after a keyframe seek, frames
    // before this pts are decoded then discarded so next() lands on/after the
    // requested time rather than the preceding keyframe. None when not seeking.
    skip_until: Option<i64>,
}

/// Open a media file for frame-by-frame RGB24 video decode. `filter` is an
/// optional FFmpeg video filtergraph applied to every frame (empty = none, e.g.
/// "scale=1280:720", "crop=in_w/2:in_h:0:0", "hflip", "fps=15",
/// "drawtext=text='hi':x=10:y=10"). Output stays RGB24. Null on error.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_open(
    path: *const c_char,
    filter: *const c_char,
) -> *mut VideoReader {
    let path = match unsafe { cstr(path) } {
        Some(s) => s.to_string(),
        None => {
            set_err("video_open: null/invalid path");
            return ptr::null_mut();
        }
    };
    // A null filter pointer means "no filter"; otherwise take it as-is.
    let filter = unsafe { cstr(filter) }.unwrap_or("").to_string();
    match video_open_inner(&path, filter) {
        Ok(r) => Box::into_raw(Box::new(r)),
        Err(e) => {
            set_err(format!("video_open: {e}"));
            ptr::null_mut()
        }
    }
}

fn video_open_inner(path: &str, filter_spec: String) -> Result<VideoReader, ff::Error> {
    let _ = ff::init();
    let ictx = ff::format::input(&path)?;
    let stream = ictx
        .streams()
        .best(MediaType::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let frame_rate = {
        let r = stream.rate();
        if r.numerator() > 0 && r.denominator() > 0 {
            r
        } else {
            ff::Rational::new(30, 1)
        }
    };
    let params = ictx.stream(stream_index).unwrap().parameters();
    let decoder = ff::codec::context::Context::from_parameters(params)?
        .decoder()
        .video()?;
    Ok(VideoReader {
        ictx,
        decoder,
        scaler: None,
        stream_index,
        frame: VideoFrame::empty(),
        rgb: VideoFrame::empty(),
        eof_sent: false,
        has_frame: false,
        filter_spec,
        filter: None,
        filter_flushed: false,
        time_base,
        frame_rate,
        cur_pts: None,
        skip_until: None,
    })
}

// After a produced frame's cur_pts is set: return true if it should be emitted,
// false if it should be discarded (precise-seek skip). Clears the target once
// reached so subsequent frames flow normally.
fn reached_target(r: &mut VideoReader) -> bool {
    match r.skip_until {
        Some(target) => {
            if r.cur_pts.unwrap_or(i64::MIN) >= target {
                r.skip_until = None;
                true
            } else {
                false
            }
        }
        None => true,
    }
}

// Build a per-frame video filtergraph lazily (needs the first frame's real
// dimensions / pixel format). buffer(source) -> user spec -> format=<out_pix> ->
// buffersink(sink). The trailing format converter pins the sink's output pixel
// format (rgb24 for the reader's zero-copy contract, yuv420p for the encoder's
// input) regardless of what the user's spec produces. Formats/layouts are driven
// by NAME (FFmpeg-8 safe). An empty spec is a pure format-normalizing pass.
fn build_video_filter(
    frame: &VideoFrame,
    spec: &str,
    out_pix: &str,
    time_base: ff::Rational,
    frame_rate: ff::Rational,
) -> Result<ff::filter::Graph, ff::Error> {
    let mut g = ff::filter::Graph::new();
    let pix = frame
        .format()
        .descriptor()
        .map(|d| d.name())
        .unwrap_or("yuv420p");
    let args = format!(
        "width={w}:height={h}:pix_fmt={pf}:time_base={tn}/{td}:pixel_aspect=1/1:frame_rate={frn}/{frd}",
        w = frame.width(),
        h = frame.height(),
        pf = pix,
        tn = time_base.numerator(),
        td = time_base.denominator(),
        frn = frame_rate.numerator(),
        frd = frame_rate.denominator(),
    );
    g.add(&ff::filter::find("buffer").unwrap(), "in", &args)?;
    g.add(&ff::filter::find("buffersink").unwrap(), "out", "")?;
    let full = if spec.is_empty() {
        format!("format={out_pix}")
    } else {
        format!("{spec},format={out_pix}")
    };
    g.output("in", 0)?.input("out", 0)?.parse(&full)?;
    g.validate()?;
    Ok(g)
}

fn scale_to_rgb(r: &mut VideoReader) -> Result<(), ff::Error> {
    if r.scaler.is_none() {
        r.scaler = Some(Scaler::get(
            r.frame.format(),
            r.frame.width(),
            r.frame.height(),
            Pixel::RGB24,
            r.frame.width(),
            r.frame.height(),
            ScaleFlags::BILINEAR,
        )?);
    }
    r.scaler.as_mut().unwrap().run(&r.frame, &mut r.rgb)?;
    Ok(())
}

fn video_next_inner(r: &mut VideoReader) -> Result<bool, ff::Error> {
    let filtering = !r.filter_spec.is_empty();
    loop {
        // When filtering, a ready RGB24 frame from the sink is the output. Pull
        // first so filters that emit >1 output per input (or per flush) drain.
        if filtering {
            if let Some(g) = r.filter.as_mut() {
                if g.get("out").unwrap().sink().frame(&mut r.rgb).is_ok() {
                    r.cur_pts = r.rgb.pts();
                    if !reached_target(r) {
                        continue; // precise-seek: discard pre-target frame
                    }
                    r.has_frame = true;
                    return Ok(true);
                }
            }
        }
        // Pull a decoded frame.
        if r.decoder.receive_frame(&mut r.frame).is_ok() {
            if !filtering {
                r.cur_pts = r.frame.pts();
                if !reached_target(r) {
                    continue; // precise-seek: skip cheaply, before scaling
                }
                scale_to_rgb(r)?;
                r.has_frame = true;
                return Ok(true);
            }
            // Feed it into the graph (built lazily from this first frame), then
            // loop back to pull the filtered result.
            if r.filter.is_none() {
                r.filter = Some(build_video_filter(
                    &r.frame,
                    &r.filter_spec,
                    "rgb24",
                    r.time_base,
                    r.frame_rate,
                )?);
            }
            r.filter
                .as_mut()
                .unwrap()
                .get("in")
                .unwrap()
                .source()
                .add(&r.frame)?;
            continue;
        }
        if r.eof_sent {
            // Decoder drained. If filtering, flush the graph once and keep
            // pulling; otherwise we are done.
            if filtering && !r.filter_flushed {
                r.filter_flushed = true;
                if let Some(g) = r.filter.as_mut() {
                    g.get("in").unwrap().source().flush()?;
                    continue;
                }
            }
            r.has_frame = false;
            return Ok(false);
        }
        // Read one packet, releasing the ictx borrow before touching the decoder.
        let mut got: Option<ff::Packet> = None;
        let mut at_eof = false;
        {
            match r.ictx.packets().next() {
                Some((s, p)) => {
                    if s.index() == r.stream_index {
                        got = Some(p);
                    }
                }
                None => at_eof = true,
            }
        }
        if at_eof {
            r.decoder.send_eof()?;
            r.eof_sent = true;
        } else if let Some(p) = got {
            r.decoder.send_packet(&p)?;
        }
    }
}

/// Decode + scale the next frame to RGB24. Returns 1 = frame ready, 0 = end of
/// stream, <0 = error (see last_error).
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_next(reader: *mut VideoReader) -> c_int {
    if reader.is_null() {
        return -1;
    }
    let r = unsafe { &mut *reader };
    match video_next_inner(r) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            set_err(format!("video_next: {e}"));
            -1
        }
    }
}

/// Width of the current RGB frame in pixels (-1 on null).
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_width(reader: *const VideoReader) -> c_int {
    if reader.is_null() {
        return -1;
    }
    unsafe { (*reader).rgb.width() as c_int }
}

/// Height of the current RGB frame in pixels (-1 on null).
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_height(reader: *const VideoReader) -> c_int {
    if reader.is_null() {
        return -1;
    }
    unsafe { (*reader).rgb.height() as c_int }
}

/// Bytes per row of the RGB buffer (may exceed width*3 due to alignment). -1 null.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_stride(reader: *const VideoReader) -> c_int {
    if reader.is_null() {
        return -1;
    }
    unsafe { (*reader).rgb.stride(0) as c_int }
}

/// Borrowed pointer to the current RGB24 frame (packed R,G,B rows of `stride`
/// bytes). Valid until the next `chuks_ffmpeg_video_next` / free. Zero-copy:
/// @chuks/onnx reads it directly to build a tensor. Null if no frame.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_ptr(reader: *const VideoReader) -> *const c_void {
    if reader.is_null() {
        return ptr::null();
    }
    let r = unsafe { &*reader };
    if !r.has_frame {
        return ptr::null();
    }
    r.rgb.data(0).as_ptr() as *const c_void
}

/// Seek to (the keyframe at or before) `seconds`, then resume decoding from
/// there via `chuks_ffmpeg_video_next`. Flushes the decoder and resets stream
/// state (any filtergraph is rebuilt on the next frame). Returns 0 on success,
/// <0 on error (see last_error).
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_seek(reader: *mut VideoReader, seconds: f64) -> c_int {
    if reader.is_null() {
        return -1;
    }
    let r = unsafe { &mut *reader };
    let target = seconds.max(0.0);
    // stream_index -1 => timestamp is in AV_TIME_BASE units (microseconds).
    let ts = (target * 1_000_000.0) as i64;
    if let Err(e) = r.ictx.seek(ts, ..ts) {
        set_err(format!("video_seek: {e}"));
        return -1;
    }
    r.decoder.flush();
    // Reset streaming state so next() re-primes from the new position.
    r.eof_sent = false;
    r.has_frame = false;
    r.filter_flushed = false;
    r.filter = None; // rebuilt lazily; its time state would otherwise be stale
    r.cur_pts = None;
    // Precise seek: the demuxer lands on the keyframe at/before `target`; skip
    // decoded frames until we reach `target` (in stream time_base units).
    let tb = r.time_base;
    r.skip_until = if tb.numerator() > 0 {
        Some((target * (tb.denominator() as f64) / (tb.numerator() as f64)) as i64)
    } else {
        None
    };
    0
}

/// Presentation timestamp of the current frame in SECONDS, or -1 if no frame /
/// unknown. Useful for driving a seek UI ("where am I").
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_pts(reader: *const VideoReader) -> f64 {
    if reader.is_null() {
        return -1.0;
    }
    let r = unsafe { &*reader };
    match r.cur_pts {
        Some(p) => {
            let tb = r.time_base;
            p as f64 * (tb.numerator() as f64) / (tb.denominator() as f64)
        }
        None => -1.0,
    }
}

/// Total duration of the media in SECONDS, or -1 if unknown.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_video_duration(reader: *const VideoReader) -> f64 {
    if reader.is_null() {
        return -1.0;
    }
    let d = unsafe { (*reader).ictx.duration() };
    if d > 0 {
        d as f64 / 1_000_000.0 // AV_TIME_BASE units
    } else {
        -1.0
    }
}

/// Free a video reader handle.
///
/// # Safety
/// `reader` must be null or a handle from `chuks_ffmpeg_video_open`, not freed.
#[no_mangle]
pub unsafe extern "C" fn chuks_ffmpeg_video_free(reader: *mut VideoReader) {
    if !reader.is_null() {
        drop(Box::from_raw(reader));
    }
}

// ─── Transcode (decode → hardware H.264 encode → mp4 mux) ─────────────────────
//
// Proves the encode + mux write path. Uses a named encoder (e.g.
// "h264_videotoolbox", macOS hardware — available in our LGPL build via the
// linked VideoToolbox framework; libx264 is GPL and not built). Assigns a
// monotonic PTS per frame for a clean constant-frame-rate output.

// Build the audio re-encode filtergraph: abuffer(decoder fmt) -> anull ->
// abuffersink(encoder fmt). The sink is constrained to the encoder's sample
// format / channel layout / rate, so validate() inserts the resampler/format
// converter; when the encoder needs a fixed block size (AAC = 1024) the sink is
// told that frame size so it chunks output (no manual FIFO). Input layout is
// driven by NAME (FFmpeg-8 safe), never the old bitmask.
fn build_audio_filter(
    dec: &ff::codec::decoder::Audio,
    enc: &ff::codec::encoder::audio::Encoder,
    acodec: &ff::codec::audio::Audio,
    in_tb: ff::Rational,
) -> Result<ff::filter::Graph, ff::Error> {
    let mut g = ff::filter::Graph::new();
    let in_layout = match dec.channels() {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{}c", n),
    };
    let args = format!(
        "time_base={tn}/{td}:sample_rate={rate}:sample_fmt={fmt}:channel_layout={layout}",
        tn = in_tb.numerator(),
        td = in_tb.denominator(),
        rate = dec.rate(),
        fmt = dec.format().name(),
        layout = in_layout,
    );
    g.add(&ff::filter::find("abuffer").unwrap(), "in", &args)?;
    g.add(&ff::filter::find("abuffersink").unwrap(), "out", "")?;
    {
        let mut out = g.get("out").unwrap();
        out.set_sample_format(enc.format());
        out.set_channel_layout(enc.channel_layout());
        out.set_sample_rate(enc.rate());
    }
    g.output("in", 0)?.input("out", 0)?.parse("anull")?;
    g.validate()?;
    if !acodec
        .capabilities()
        .contains(ff::codec::capabilities::Capabilities::VARIABLE_FRAME_SIZE)
    {
        g.get("out").unwrap().sink().set_frame_size(enc.frame_size());
    }
    Ok(g)
}

fn transcode_inner(
    in_path: &str,
    out_path: &str,
    encoder_name: &str,
    filter_spec: &str,
    audio_codec: &str,
) -> Result<i64, ff::Error> {
    let _ = ff::init();
    // "" / "copy" => stream-copy the audio; any other value names an audio
    // encoder to re-encode to (e.g. "aac").
    let want_reencode = !audio_codec.is_empty() && audio_codec != "copy";

    // ---- input / decoder ----
    let mut ictx = ff::format::input(&in_path)?;
    let stream = ictx
        .streams()
        .best(MediaType::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let vindex = stream.index();
    let time_base = stream.time_base();
    let fps = {
        let r = stream.rate();
        if r.numerator() > 0 && r.denominator() > 0 {
            r
        } else {
            ff::Rational::new(30, 1)
        }
    };
    let params = ictx.stream(vindex).unwrap().parameters();
    let mut decoder = ff::codec::context::Context::from_parameters(params)?
        .decoder()
        .video()?;

    // ---- output context + encoder codec (encoder opened lazily) ----
    let mut octx = ff::format::output(&out_path)?;
    let codec = resolve_video_encoder(encoder_name).ok_or(ff::Error::EncoderNotFound)?;
    let global_header = octx
        .format()
        .flags()
        .contains(ff::format::flag::Flags::GLOBAL_HEADER);

    // Reserve the video output stream FIRST so it is stream 0 (the conventional
    // layout players expect). It is configured lazily via stream_mut once the
    // encoder is opened from the first filtered frame, before write_header.
    let ost_index: usize = octx.add_stream(codec)?.index();

    // ---- audio: keep the best audio track. Two modes:
    //   * COPY (default): stream-copy the audio packets through unchanged.
    //   * RE-ENCODE: decode -> aformat filtergraph -> encode to `audio_codec`.
    // Either way the output audio stream is added NOW (before the header) so
    // stream order is video(0), audio(1). The video header is written lazily
    // (from the first filtered video frame), so audio output packets seen before
    // then are buffered and flushed once the header exists. `a_src_tb` is the
    // time base the buffered/emitted audio packets are in (input stream tb for
    // copy, encoder tb for re-encode); they are rescaled to `a_out_tb` at mux.
    let mut a_in_index: i64 = -1;
    let mut a_out_index: usize = 0;
    let mut a_out_tb = ff::Rational::new(1, 1);
    let mut a_src_tb = ff::Rational::new(1, 1);
    let mut audio_buf: Vec<ff::Packet> = Vec::new();
    let mut a_reencode = false;
    let mut a_decoder: Option<ff::codec::decoder::Audio> = None;
    let mut a_opened: Option<ff::codec::encoder::audio::Encoder> = None;
    let mut a_filter: Option<ff::filter::Graph> = None;
    let mut a_dec_frame = AudioFrame::empty();
    let mut a_filt_frame = AudioFrame::empty();
    if let Some(astream) = ictx.streams().best(MediaType::Audio) {
        a_in_index = astream.index() as i64;
        let a_in_tb = astream.time_base();
        let aparams = astream.parameters();
        if want_reencode {
            let dec = ff::codec::context::Context::from_parameters(aparams)?
                .decoder()
                .audio()?;
            let acodec = ff::encoder::find_by_name(audio_codec)
                .ok_or(ff::Error::EncoderNotFound)?
                .audio()?;
            let mut aout = octx.add_stream(acodec)?;
            let mut aenc = ff::codec::context::Context::from_parameters(aout.parameters())?
                .encoder()
                .audio()?;
            if global_header {
                aenc.set_flags(ff::codec::flag::Flags::GLOBAL_HEADER);
            }
            aenc.set_rate(dec.rate() as i32);
            aenc.set_channel_layout(ff::channel_layout::ChannelLayout::default(
                dec.channels() as i32,
            ));
            aenc.set_format(
                acodec
                    .formats()
                    .and_then(|mut f| f.next())
                    .ok_or(ff::Error::EncoderNotFound)?,
            );
            let br = dec.bit_rate();
            aenc.set_bit_rate(if br > 0 { br } else { 128_000 });
            aenc.set_time_base((1, dec.rate() as i32));
            let opened_enc = aenc.open_as(acodec)?;
            aout.set_parameters(&opened_enc);
            aout.set_time_base((1, dec.rate() as i32));
            a_out_index = aout.index();
            a_src_tb = opened_enc.time_base();
            drop(aout); // release the octx borrow
            a_filter = Some(build_audio_filter(&dec, &opened_enc, &acodec, a_in_tb)?);
            a_decoder = Some(dec);
            a_opened = Some(opened_enc);
            a_reencode = true;
        } else {
            let mut ost = octx.add_stream(ff::encoder::find(ff::codec::Id::None))?;
            ost.set_parameters(aparams);
            // Clear codec_tag so muxing into a (possibly different) container is safe.
            unsafe {
                (*ost.parameters().as_mut_ptr()).codec_tag = 0;
            }
            a_out_index = ost.index();
            a_src_tb = a_in_tb;
        }
    }

    // Every frame flows: decode -> filtergraph -> encode. The graph normalizes
    // the pixel format to yuv420p for the encoder and applies the user's spec
    // (e.g. "scale=1280:720", empty = normalize only). The encoder is configured
    // from the FIRST FILTERED frame so a resize filter reshapes the output.
    let mut filter: Option<ff::filter::Graph> = None;
    let mut opened: Option<ff::codec::encoder::video::Encoder> = None;
    let mut ost_tb = ff::Rational::new(1, 1);
    let mut enc_tb = ff::Rational::new(1, 1);

    let mut dec_frame = VideoFrame::empty();
    let mut filt_frame = VideoFrame::empty();
    let mut pts: i64 = 0;
    let mut count: i64 = 0;

    // Mux one finished audio packet: rescale from its source time base to the
    // output audio stream's, and write it. Only valid once the header is written.
    macro_rules! write_audio {
        ($p:expr) => {{
            let mut ap: ff::Packet = $p;
            ap.set_stream(a_out_index);
            ap.rescale_ts(a_src_tb, a_out_tb);
            ap.set_position(-1);
            ap.write_interleaved(&mut octx)?;
        }};
    }
    // Buffer-or-write a finished audio packet depending on whether the (lazy)
    // video header has been written yet.
    macro_rules! emit_audio_pkt {
        ($p:expr) => {{
            if opened.is_some() {
                write_audio!($p);
            } else {
                audio_buf.push($p);
            }
        }};
    }
    // Open the encoder + write the header once, sized from the first filtered
    // frame. Order matters: add_stream/set_parameters/write_header on octx must
    // happen after the encoder is opened. Writing the header also unblocks any
    // audio packets buffered before this point, so flush them here.
    macro_rules! ensure_started {
        () => {{
            if opened.is_none() {
                let mut enc = ff::codec::context::Context::new_with_codec(codec)
                    .encoder()
                    .video()?;
                enc.set_width(filt_frame.width());
                enc.set_height(filt_frame.height());
                enc.set_format(filt_frame.format()); // yuv420p (graph output)
                enc.set_time_base(fps.invert());
                enc.set_frame_rate(Some(fps));
                if global_header {
                    enc.set_flags(ff::codec::flag::Flags::GLOBAL_HEADER);
                }
                let enc_opened = enc.open()?;
                {
                    let mut ost = octx.stream_mut(ost_index).unwrap();
                    ost.set_parameters(&enc_opened);
                    ost.set_time_base(fps.invert());
                }
                octx.write_header()?;
                ost_tb = octx.stream(ost_index).unwrap().time_base();
                enc_tb = enc_opened.time_base();
                opened = Some(enc_opened);
                // Now that the header exists, capture the audio output time base
                // and flush any audio packets that arrived before this frame.
                if a_in_index >= 0 {
                    a_out_tb = octx.stream(a_out_index).unwrap().time_base();
                    let buffered: Vec<ff::Packet> = std::mem::take(&mut audio_buf);
                    for ap in buffered {
                        write_audio!(ap);
                    }
                }
            }
        }};
    }
    macro_rules! write_encoded {
        () => {{
            if let Some(enc) = opened.as_mut() {
                let mut pkt = ff::Packet::empty();
                while enc.receive_packet(&mut pkt).is_ok() {
                    pkt.set_stream(ost_index);
                    // One frame's worth of duration (in the encoder time base,
                    // which is 1/fps). Without this the mp4 muxer gives the final
                    // sample zero duration and writes an edit list that trims it,
                    // so the last frame is lost on playback / re-decode.
                    pkt.set_duration(1);
                    pkt.rescale_ts(enc_tb, ost_tb);
                    pkt.write_interleaved(&mut octx)?;
                }
            }
        }};
    }
    // Pull every filtered frame the graph has ready and encode it.
    macro_rules! drain_filter {
        () => {{
            if filter.is_some() {
                while filter
                    .as_mut()
                    .unwrap()
                    .get("out")
                    .unwrap()
                    .sink()
                    .frame(&mut filt_frame)
                    .is_ok()
                {
                    ensure_started!();
                    filt_frame.set_pts(Some(pts));
                    pts += 1;
                    count += 1;
                    opened.as_mut().unwrap().send_frame(&filt_frame)?;
                    write_encoded!();
                }
            }
        }};
    }
    // Drain the decoder, feeding each frame into the (lazily built) graph.
    macro_rules! feed_decoded {
        () => {{
            while decoder.receive_frame(&mut dec_frame).is_ok() {
                if filter.is_none() {
                    filter = Some(build_video_filter(
                        &dec_frame,
                        filter_spec,
                        "yuv420p",
                        time_base,
                        fps,
                    )?);
                }
                filter
                    .as_mut()
                    .unwrap()
                    .get("in")
                    .unwrap()
                    .source()
                    .add(&dec_frame)?;
                drain_filter!();
            }
        }};
    }

    // Pull chunked audio frames from the audio graph, encode each, emit packets.
    macro_rules! drain_audio_filter {
        () => {{
            while a_filter
                .as_mut()
                .unwrap()
                .get("out")
                .unwrap()
                .sink()
                .frame(&mut a_filt_frame)
                .is_ok()
            {
                a_opened.as_mut().unwrap().send_frame(&a_filt_frame)?;
                loop {
                    let mut pkt = ff::Packet::empty();
                    if a_opened.as_mut().unwrap().receive_packet(&mut pkt).is_ok() {
                        emit_audio_pkt!(pkt);
                    } else {
                        break;
                    }
                }
            }
        }};
    }
    // Decode one audio packet, feed the graph, encode + emit what comes out.
    macro_rules! reencode_audio {
        ($p:expr) => {{
            a_decoder.as_mut().unwrap().send_packet(&$p)?;
            while a_decoder
                .as_mut()
                .unwrap()
                .receive_frame(&mut a_dec_frame)
                .is_ok()
            {
                a_filter
                    .as_mut()
                    .unwrap()
                    .get("in")
                    .unwrap()
                    .source()
                    .add(&a_dec_frame)?;
                drain_audio_filter!();
            }
        }};
    }

    // demux one packet at a time (position lives in the format context)
    let mut eof = false;
    while !eof {
        let mut got_video: Option<ff::Packet> = None;
        let mut got_audio: Option<ff::Packet> = None;
        {
            match ictx.packets().next() {
                Some((s, p)) => {
                    let idx = s.index();
                    if idx == vindex {
                        got_video = Some(p);
                    } else if idx as i64 == a_in_index {
                        got_audio = Some(p);
                    }
                }
                None => eof = true,
            }
        }
        if let Some(p) = got_video {
            decoder.send_packet(&p)?;
            feed_decoded!();
        }
        if let Some(p) = got_audio {
            if a_reencode {
                reencode_audio!(p); // decode -> filter -> encode -> emit
            } else {
                emit_audio_pkt!(p); // copy straight through (buffer or write)
            }
        }
    }
    decoder.send_eof()?;
    feed_decoded!();
    // flush the video filtergraph, then the video encoder
    if let Some(g) = filter.as_mut() {
        g.get("in").unwrap().source().flush()?;
    }
    drain_filter!();
    if opened.is_none() {
        // No video frames made it through (empty/failed input); nothing was muxed.
        return Ok(0);
    }
    opened.as_mut().unwrap().send_eof()?;
    write_encoded!();
    // Drain the audio re-encode pipeline (decoder -> filter -> encoder). The
    // video header is written by now, so emitted packets mux directly.
    if a_reencode {
        a_decoder.as_mut().unwrap().send_eof()?;
        while a_decoder
            .as_mut()
            .unwrap()
            .receive_frame(&mut a_dec_frame)
            .is_ok()
        {
            a_filter
                .as_mut()
                .unwrap()
                .get("in")
                .unwrap()
                .source()
                .add(&a_dec_frame)?;
            drain_audio_filter!();
        }
        a_filter.as_mut().unwrap().get("in").unwrap().source().flush()?;
        drain_audio_filter!();
        a_opened.as_mut().unwrap().send_eof()?;
        loop {
            let mut pkt = ff::Packet::empty();
            if a_opened.as_mut().unwrap().receive_packet(&mut pkt).is_ok() {
                emit_audio_pkt!(pkt);
            } else {
                break;
            }
        }
    }
    octx.write_trailer()?;
    Ok(count)
}

/// Transcode `in_path` → `out_path`, re-encoding video with the named `encoder`
/// (e.g. "h264_videotoolbox"), optionally applying video `filter` (empty = none,
/// e.g. "scale=1280:720"). `audio` selects the audio handling: "" or "copy" =
/// stream-copy the audio track; any other value names an audio encoder to
/// re-encode to (e.g. "aac"). Returns video frames written, or -1 on error.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_transcode(
    in_path: *const c_char,
    out_path: *const c_char,
    encoder: *const c_char,
    filter: *const c_char,
    audio: *const c_char,
) -> i64 {
    let inp = match unsafe { cstr(in_path) } {
        Some(s) => s.to_string(),
        None => {
            set_err("transcode: null in_path");
            return -1;
        }
    };
    let outp = match unsafe { cstr(out_path) } {
        Some(s) => s.to_string(),
        None => {
            set_err("transcode: null out_path");
            return -1;
        }
    };
    let enc = match unsafe { cstr(encoder) } {
        Some(s) => s.to_string(),
        None => {
            set_err("transcode: null encoder");
            return -1;
        }
    };
    let filt = unsafe { cstr(filter) }.unwrap_or("").to_string();
    let aud = unsafe { cstr(audio) }.unwrap_or("").to_string();
    match transcode_inner(&inp, &outp, &enc, &filt, &aud) {
        Ok(n) => n,
        Err(e) => {
            set_err(format!("transcode: {e}"));
            -1
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Frame encoder — push rendered RGB24 frames, get an encoded video file.
//
// The inverse of the video reader: an editor renders frames itself and pushes
// them here to be scaled (RGB24 -> yuv420p), encoded, and muxed, WITHOUT
// shelling out to an ffmpeg binary. writeFrame borrows the caller's packed
// RGB24 buffer (width*height*3) only for the duration of the call. The encoder
// family is resolved adaptively (hardware first) via resolve_video_encoder,
// exactly like transcode.
// ─────────────────────────────────────────────────────────────────────────────

pub struct VideoWriter {
    octx: ff::format::context::Output,
    encoder: ff::codec::encoder::video::Encoder,
    scaler: Scaler,
    rgb: VideoFrame,
    yuv: VideoFrame,
    ost_index: usize,
    ost_tb: ff::Rational,
    enc_tb: ff::Rational,
    width: u32,
    height: u32,
    pts: i64,
    count: i64,
}

fn video_writer_open(
    path: &str,
    width: u32,
    height: u32,
    fps_num: i32,
    fps_den: i32,
    encoder_name: &str,
) -> Result<VideoWriter, ff::Error> {
    let _ = ff::init();
    let fps = if fps_num > 0 && fps_den > 0 {
        ff::Rational::new(fps_num, fps_den)
    } else {
        ff::Rational::new(30, 1)
    };
    let mut octx = ff::format::output(&path)?;
    let codec = resolve_video_encoder(encoder_name).ok_or(ff::Error::EncoderNotFound)?;
    let global_header = octx
        .format()
        .flags()
        .contains(ff::format::flag::Flags::GLOBAL_HEADER);

    let mut enc = ff::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    enc.set_width(width);
    enc.set_height(height);
    enc.set_format(Pixel::YUV420P);
    enc.set_time_base(fps.invert());
    enc.set_frame_rate(Some(fps));
    if global_header {
        enc.set_flags(ff::codec::flag::Flags::GLOBAL_HEADER);
    }
    let encoder = enc.open()?;

    let ost_index = {
        let mut ost = octx.add_stream(codec)?;
        ost.set_parameters(&encoder);
        ost.set_time_base(fps.invert());
        ost.index()
    };
    octx.write_header()?;
    let ost_tb = octx.stream(ost_index).unwrap().time_base();
    let enc_tb = encoder.time_base();

    let scaler = Scaler::get(
        Pixel::RGB24,
        width,
        height,
        Pixel::YUV420P,
        width,
        height,
        ScaleFlags::BILINEAR,
    )?;

    Ok(VideoWriter {
        octx,
        encoder,
        scaler,
        rgb: VideoFrame::new(Pixel::RGB24, width, height),
        yuv: VideoFrame::new(Pixel::YUV420P, width, height),
        ost_index,
        ost_tb,
        enc_tb,
        width,
        height,
        pts: 0,
        count: 0,
    })
}

fn video_writer_write(w: &mut VideoWriter, data: &[u8], in_stride: usize) -> Result<(), ff::Error> {
    // Copy RGB24 into the frame's own (possibly padded) stride, row by row. The
    // caller's rows may themselves be padded (`in_stride`), so a stride-padded
    // buffer from the video reader can be fed straight through without repacking.
    let row = (w.width * 3) as usize;
    let src_stride = if in_stride == 0 { row } else { in_stride };
    let dst_stride = w.rgb.stride(0);
    {
        let dst = w.rgb.data_mut(0);
        for y in 0..w.height as usize {
            dst[y * dst_stride..y * dst_stride + row]
                .copy_from_slice(&data[y * src_stride..y * src_stride + row]);
        }
    }
    w.scaler.run(&w.rgb, &mut w.yuv)?;
    w.yuv.set_pts(Some(w.pts));
    w.pts += 1;
    w.count += 1;
    w.encoder.send_frame(&w.yuv)?;
    let mut pkt = ff::Packet::empty();
    while w.encoder.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(w.ost_index);
        pkt.set_duration(1);
        pkt.rescale_ts(w.enc_tb, w.ost_tb);
        pkt.write_interleaved(&mut w.octx)?;
    }
    Ok(())
}

fn video_writer_finish(w: &mut VideoWriter) -> Result<(), ff::Error> {
    w.encoder.send_eof()?;
    let mut pkt = ff::Packet::empty();
    while w.encoder.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(w.ost_index);
        pkt.set_duration(1);
        pkt.rescale_ts(w.enc_tb, w.ost_tb);
        pkt.write_interleaved(&mut w.octx)?;
    }
    w.octx.write_trailer()?;
    Ok(())
}

/// Open a frame encoder. `encoder` is a codec FAMILY ("h264"/"hevc"/"mpeg4"/
/// "mjpeg"), resolved to the best encoder available for this platform/build.
/// fps is num/den (pass 30,1 for 30fps). Returns an opaque *mut VideoWriter, or
/// null on error (see chuks_ffmpeg_last_error).
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_venc_open(
    path: *const c_char,
    width: c_int,
    height: c_int,
    fps_num: c_int,
    fps_den: c_int,
    encoder: *const c_char,
) -> *mut VideoWriter {
    let path = match unsafe { cstr(path) } {
        Some(s) => s,
        None => {
            set_err("venc_open: null path");
            return ptr::null_mut();
        }
    };
    let family = unsafe { cstr(encoder) }.unwrap_or("h264");
    if width <= 0 || height <= 0 {
        set_err("venc_open: width/height must be > 0");
        return ptr::null_mut();
    }
    match video_writer_open(path, width as u32, height as u32, fps_num, fps_den, family) {
        Ok(w) => Box::into_raw(Box::new(w)),
        Err(e) => {
            set_err(format!("venc_open: {e}"));
            ptr::null_mut()
        }
    }
}

/// Push one RGB24 frame. `stride` is the input's bytes per row (0 = packed,
/// i.e. width*3); pass a video reader's stride() to feed its frames through
/// zero-copy. `byte_len` must cover stride*height. Returns 0 ok, -1 on error.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_venc_write(
    writer: *mut VideoWriter,
    data: *const c_void,
    byte_len: c_int,
    stride: c_int,
) -> c_int {
    if writer.is_null() || data.is_null() {
        set_err("venc_write: null writer/data");
        return -1;
    }
    let w = unsafe { &mut *writer };
    let row = (w.width as usize) * 3;
    let in_stride = if stride <= 0 { row } else { stride as usize };
    if in_stride < row {
        set_err(format!("venc_write: stride {} < width*3 {}", in_stride, row));
        return -1;
    }
    let need = in_stride * (w.height as usize);
    if (byte_len as usize) < need {
        set_err(format!(
            "venc_write: need {} bytes for {}x{} RGB24 (stride {}), got {}",
            need, w.width, w.height, in_stride, byte_len
        ));
        return -1;
    }
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, need) };
    match video_writer_write(w, slice, in_stride) {
        Ok(()) => 0,
        Err(e) => {
            set_err(format!("venc_write: {e}"));
            -1
        }
    }
}

/// Flush, write the trailer, then close and FREE the writer. Returns the number
/// of frames written (>=0), or -1 on error. The pointer is invalid afterwards.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_venc_close(writer: *mut VideoWriter) -> i64 {
    if writer.is_null() {
        set_err("venc_close: null writer");
        return -1;
    }
    let mut w = unsafe { Box::from_raw(writer) };
    match video_writer_finish(&mut w) {
        Ok(()) => w.count,
        Err(e) => {
            set_err(format!("venc_close: {e}"));
            -1
        }
    }
    // w dropped here -> encoder + octx freed
}

// ─────────────────────────────────────────────────────────────────────────────
// mux — combine a video file and an audio file into one container by stream
// copy (no re-encode). Pairs with the frame encoder: render frames -> silent
// video, then mux in the timeline's audio. Packets are interleaved by rescaled
// dts so the muxer never has to buffer a whole stream.
// ─────────────────────────────────────────────────────────────────────────────

fn mux_video_audio(video_path: &str, audio_path: &str, out_path: &str) -> Result<i64, ff::Error> {
    use ff::util::mathematics::Rescale;
    let _ = ff::init();
    let mut vin = ff::format::input(&video_path)?;
    let mut ain = ff::format::input(&audio_path)?;

    let v_idx = vin
        .streams()
        .best(MediaType::Video)
        .ok_or(ff::Error::StreamNotFound)?
        .index();
    let v_in_tb = vin.stream(v_idx).unwrap().time_base();
    let a_idx = ain
        .streams()
        .best(MediaType::Audio)
        .ok_or(ff::Error::StreamNotFound)?
        .index();
    let a_in_tb = ain.stream(a_idx).unwrap().time_base();

    let mut octx = ff::format::output(&out_path)?;
    // video -> stream 0
    {
        let params = vin.stream(v_idx).unwrap().parameters();
        let mut ost = octx.add_stream(ff::encoder::find(ff::codec::Id::None))?;
        ost.set_parameters(params);
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
    }
    // audio -> stream 1
    {
        let params = ain.stream(a_idx).unwrap().parameters();
        let mut ost = octx.add_stream(ff::encoder::find(ff::codec::Id::None))?;
        ost.set_parameters(params);
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
    }
    octx.write_header()?;
    let v_out_tb = octx.stream(0).unwrap().time_base();
    let a_out_tb = octx.stream(1).unwrap().time_base();

    const COMMON: (i32, i32) = (1, 1_000_000);
    let mut count: i64 = 0;
    let mut vpkt: Option<ff::Packet> = None;
    let mut apkt: Option<ff::Packet> = None;
    let mut vdone = false;
    let mut adone = false;

    // Pull the next packet belonging to $idx from $ctx, skipping other streams.
    macro_rules! fill {
        ($slot:ident, $ctx:ident, $idx:ident, $done:ident) => {
            if $slot.is_none() && !$done {
                loop {
                    match $ctx.packets().next() {
                        Some((s, p)) => {
                            if s.index() == $idx {
                                $slot = Some(p);
                                break;
                            }
                        }
                        None => {
                            $done = true;
                            break;
                        }
                    }
                }
            }
        };
    }

    loop {
        fill!(vpkt, vin, v_idx, vdone);
        fill!(apkt, ain, a_idx, adone);
        if vpkt.is_none() && apkt.is_none() {
            break;
        }
        // Compare dts in a common time base; write the earlier packet.
        let vk = vpkt
            .as_ref()
            .map(|p| p.dts().unwrap_or(0).rescale(v_in_tb, COMMON));
        let ak = apkt
            .as_ref()
            .map(|p| p.dts().unwrap_or(0).rescale(a_in_tb, COMMON));
        let take_video = match (vk, ak) {
            (Some(v), Some(a)) => v <= a,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_video {
            let mut p = vpkt.take().unwrap();
            p.set_stream(0);
            p.rescale_ts(v_in_tb, v_out_tb);
            p.set_position(-1);
            p.write_interleaved(&mut octx)?;
            count += 1;
        } else {
            let mut p = apkt.take().unwrap();
            p.set_stream(1);
            p.rescale_ts(a_in_tb, a_out_tb);
            p.set_position(-1);
            p.write_interleaved(&mut octx)?;
        }
    }
    octx.write_trailer()?;
    Ok(count)
}

/// Mux a video file + audio file into out_path by stream copy (no re-encode).
/// Returns the number of video packets written (>=0), or -1 on error.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_mux(
    video_path: *const c_char,
    audio_path: *const c_char,
    out_path: *const c_char,
) -> i64 {
    let v = match unsafe { cstr(video_path) } {
        Some(s) => s,
        None => {
            set_err("mux: null video path");
            return -1;
        }
    };
    let a = match unsafe { cstr(audio_path) } {
        Some(s) => s,
        None => {
            set_err("mux: null audio path");
            return -1;
        }
    };
    let o = match unsafe { cstr(out_path) } {
        Some(s) => s,
        None => {
            set_err("mux: null out path");
            return -1;
        }
    };
    match mux_video_audio(v, a, o) {
        Ok(n) => n,
        Err(e) => {
            set_err(format!("mux: {e}"));
            -1
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Probe — read a file's metadata WITHOUT decoding any frames. Opens the
// container, inspects the best video/audio streams, and caches the facts an
// editor needs before importing a clip (size, duration, fps, codecs, audio
// layout). Returned as an opaque handle with typed accessors (no map/JSON, so
// VM and AOT read identical values).
// ─────────────────────────────────────────────────────────────────────────────

pub struct Probe {
    width: i32,
    height: i32,
    duration: f64,
    fps: f64,
    has_video: bool,
    has_audio: bool,
    video_codec: String,
    audio_codec: String,
    sample_rate: i32,
    channels: i32,
    bit_rate: i64,
    nb_frames: i64,
}

fn probe_open(path: &str) -> Result<Probe, ff::Error> {
    let _ = ff::init();
    let ictx = ff::format::input(&path)?;
    let mut p = Probe {
        width: 0,
        height: 0,
        duration: -1.0,
        fps: 0.0,
        has_video: false,
        has_audio: false,
        video_codec: String::new(),
        audio_codec: String::new(),
        sample_rate: 0,
        channels: 0,
        bit_rate: ictx.bit_rate(),
        nb_frames: 0,
    };
    let d = ictx.duration();
    if d > 0 {
        p.duration = d as f64 / 1_000_000.0; // AV_TIME_BASE units
    }
    if let Some(v) = ictx.streams().best(MediaType::Video) {
        p.has_video = true;
        let params = v.parameters();
        p.video_codec = params.id().name().to_string();
        let fr = v.avg_frame_rate();
        if fr.denominator() != 0 {
            p.fps = fr.numerator() as f64 / fr.denominator() as f64;
        }
        p.nb_frames = v.frames();
        if let Ok(dec) = ff::codec::context::Context::from_parameters(params)
            .and_then(|c| c.decoder().video())
        {
            p.width = dec.width() as i32;
            p.height = dec.height() as i32;
        }
    }
    if let Some(a) = ictx.streams().best(MediaType::Audio) {
        p.has_audio = true;
        let params = a.parameters();
        p.audio_codec = params.id().name().to_string();
        if let Ok(dec) = ff::codec::context::Context::from_parameters(params)
            .and_then(|c| c.decoder().audio())
        {
            p.sample_rate = dec.rate() as i32;
            p.channels = dec.channels() as i32;
        }
    }
    Ok(p)
}

/// Probe a media file. Returns an opaque *mut Probe (free with
/// chuks_ffmpeg_probe_free), or null on error (see chuks_ffmpeg_last_error).
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_probe(path: *const c_char) -> *mut Probe {
    let path = match unsafe { cstr(path) } {
        Some(s) => s,
        None => {
            set_err("probe: null path");
            return ptr::null_mut();
        }
    };
    match probe_open(path) {
        Ok(p) => Box::into_raw(Box::new(p)),
        Err(e) => {
            set_err(format!("probe: {e}"));
            ptr::null_mut()
        }
    }
}

macro_rules! probe_getter {
    ($name:ident, $ty:ty, $field:ident, $null:expr) => {
        #[no_mangle]
        pub extern "C" fn $name(p: *const Probe) -> $ty {
            if p.is_null() {
                return $null;
            }
            unsafe { (*p).$field }
        }
    };
}
probe_getter!(chuks_ffmpeg_probe_width, c_int, width, 0);
probe_getter!(chuks_ffmpeg_probe_height, c_int, height, 0);
probe_getter!(chuks_ffmpeg_probe_duration, f64, duration, -1.0);
probe_getter!(chuks_ffmpeg_probe_fps, f64, fps, 0.0);
probe_getter!(chuks_ffmpeg_probe_sample_rate, c_int, sample_rate, 0);
probe_getter!(chuks_ffmpeg_probe_channels, c_int, channels, 0);
probe_getter!(chuks_ffmpeg_probe_bit_rate, i64, bit_rate, 0);
probe_getter!(chuks_ffmpeg_probe_nb_frames, i64, nb_frames, 0);

#[no_mangle]
pub extern "C" fn chuks_ffmpeg_probe_has_video(p: *const Probe) -> c_int {
    if p.is_null() {
        return 0;
    }
    if unsafe { (*p).has_video } {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn chuks_ffmpeg_probe_has_audio(p: *const Probe) -> c_int {
    if p.is_null() {
        return 0;
    }
    if unsafe { (*p).has_audio } {
        1
    } else {
        0
    }
}

/// Video codec name (e.g. "h264"), empty if no video. Caller frees the string.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_probe_video_codec(p: *const Probe) -> *mut c_char {
    if p.is_null() {
        return into_cstring(String::new());
    }
    into_cstring(unsafe { (*p).video_codec.clone() })
}

/// Audio codec name (e.g. "aac"), empty if no audio. Caller frees the string.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_probe_audio_codec(p: *const Probe) -> *mut c_char {
    if p.is_null() {
        return into_cstring(String::new());
    }
    into_cstring(unsafe { (*p).audio_codec.clone() })
}

/// Free a probe handle.
///
/// # Safety
/// `p` must be null or a handle from `chuks_ffmpeg_probe`, not already freed.
#[no_mangle]
pub unsafe extern "C" fn chuks_ffmpeg_probe_free(p: *mut Probe) {
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Concat — join several media files end to end by stream copy (no re-encode),
// the in-process equivalent of the ffmpeg concat demuxer. Inputs must share a
// stream layout (same codecs/params, e.g. an editor's export chunks). Streams
// are taken from the first file; each subsequent file's packet timestamps are
// shifted by the running per-stream duration so playback is continuous. Built
// incrementally (new -> add* -> run) to avoid passing an array across the C ABI.
// ─────────────────────────────────────────────────────────────────────────────

pub struct ConcatBuilder {
    inputs: Vec<String>,
}

fn concat_files(inputs: &[&str], out_path: &str) -> Result<i64, ff::Error> {
    use ff::util::mathematics::Rescale;
    let _ = ff::init();
    let mut octx = ff::format::output(&out_path)?;

    // Define output streams from the FIRST input (copy parameters).
    {
        let first = ff::format::input(&inputs[0])?;
        for st in first.streams() {
            let params = st.parameters();
            let mut ost = octx.add_stream(ff::encoder::find(ff::codec::Id::None))?;
            ost.set_parameters(params);
            unsafe {
                (*ost.parameters().as_mut_ptr()).codec_tag = 0;
            }
        }
    }
    octx.write_header()?;

    let nstreams = octx.nb_streams() as usize;
    let out_tb: Vec<ff::Rational> = (0..nstreams)
        .map(|i| octx.stream(i).unwrap().time_base())
        .collect();

    // A single accumulating wall-clock offset (microseconds, AV_TIME_BASE). Every
    // stream is shifted by the SAME elapsed time, converted into its own output
    // time base, so all streams stay in sync and dts is strictly increasing
    // across the file boundary. Per-packet durations are unreliable on some
    // containers, so we advance by each file's overall duration instead.
    const US: (i32, i32) = (1, 1_000_000);
    let mut offset_us: i64 = 0;
    let mut count: i64 = 0;

    for path in inputs {
        let mut ictx = ff::format::input(path)?;
        let dur_us = ictx.duration();
        let in_tb: Vec<ff::Rational> = (0..ictx.nb_streams() as usize)
            .map(|i| ictx.stream(i).unwrap().time_base())
            .collect();

        for (stream, mut pkt) in ictx.packets() {
            let idx = stream.index();
            if idx >= nstreams {
                continue;
            }
            pkt.rescale_ts(in_tb[idx], out_tb[idx]);
            let shift = offset_us.rescale(US, out_tb[idx]);
            let new_dts = pkt.dts().map(|v| v + shift);
            let new_pts = pkt.pts().map(|v| v + shift);
            pkt.set_dts(new_dts);
            pkt.set_pts(new_pts);
            pkt.set_stream(idx);
            pkt.set_position(-1);
            pkt.write_interleaved(&mut octx)?;
            count += 1;
        }
        if dur_us > 0 {
            offset_us += dur_us;
        }
    }
    octx.write_trailer()?;
    Ok(count)
}

/// Start building a concat job. Free implicitly by passing to concat_run, or
/// explicitly via concat_free if abandoned.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_concat_new() -> *mut ConcatBuilder {
    Box::into_raw(Box::new(ConcatBuilder { inputs: Vec::new() }))
}

/// Append an input file to the concat job. Returns 0 ok, -1 on error.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_concat_add(b: *mut ConcatBuilder, path: *const c_char) -> c_int {
    if b.is_null() {
        set_err("concat_add: null builder");
        return -1;
    }
    match unsafe { cstr(path) } {
        Some(s) => {
            unsafe { (*b).inputs.push(s.to_string()) };
            0
        }
        None => {
            set_err("concat_add: null path");
            -1
        }
    }
}

/// Run the concat into out_path (stream copy) and FREE the builder. Returns the
/// number of packets written (>=0), or -1 on error. The builder is invalid after.
#[no_mangle]
pub extern "C" fn chuks_ffmpeg_concat_run(b: *mut ConcatBuilder, out_path: *const c_char) -> i64 {
    if b.is_null() {
        set_err("concat_run: null builder");
        return -1;
    }
    let b = unsafe { Box::from_raw(b) };
    let out = match unsafe { cstr(out_path) } {
        Some(s) => s,
        None => {
            set_err("concat_run: null out path");
            return -1;
        }
    };
    if b.inputs.is_empty() {
        set_err("concat_run: no input files added");
        return -1;
    }
    let refs: Vec<&str> = b.inputs.iter().map(|s| s.as_str()).collect();
    match concat_files(&refs, out) {
        Ok(n) => n,
        Err(e) => {
            set_err(format!("concat: {e}"));
            -1
        }
    }
}

/// Free an abandoned concat builder (not needed after concat_run).
///
/// # Safety
/// `b` must be null or a handle from `chuks_ffmpeg_concat_new`, not already freed.
#[no_mangle]
pub unsafe extern "C" fn chuks_ffmpeg_concat_free(b: *mut ConcatBuilder) {
    if !b.is_null() {
        drop(Box::from_raw(b));
    }
}
