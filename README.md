# chuks_ffmpeg — User Manual

Decode, encode, transcode, filter, seek and resample media natively in Chuks.
`chuks_ffmpeg` is a self-contained Rust `cdylib` shim over the
[`ffmpeg-next`](https://crates.io/crates/ffmpeg-next) crate (the libav
libraries), with FFmpeg compiled from source and **statically linked** — no
system FFmpeg, no shelling out to a binary, one library that ships with your app.

It is the media foundation of the Chuks ecosystem: the **audio tap** that feeds
[`@chuks/whisper`](https://github.com/chuks-lang/chuks_whisper) live captions,
the **frame plumbing** that feeds [`@chuks/onnx`](https://github.com/chuks-lang/chuks_onnx)
video effects, and Stage 0 of `@chuks/stream` (real-time streaming).

**Verified VM + AOT on macOS arm64, Linux aarch64, and Linux x86_64.**

---

## Install

```bash
chuks add @chuks/ffmpeg
```

---

## Quick start

```chuks
import { Ffmpeg, VideoEncoder, AudioEncoder } from "pkg/@chuks/ffmpeg"

const ff = new Ffmpeg()
println(ff.version())                                   // FFmpeg build string

// 1. Audio tap: any file -> 16 kHz mono PCM (whisper's format), zero-copy.
//    `using const` closes the buffer automatically at the end of the block.
using const pcm = ff.decodeAudio("clip.mp4")
println("samples: " + string(pcm.samples()))            // ~16000 per second
const buf = pcm.data()                                  // borrowed CPtr -> whisper

// 2. Video: decode frame-by-frame to RGB24, zero-copy.
using const vid = ff.openVideo("clip.mp4")
while (vid.next()) {
    const rgb = vid.frame()                             // borrowed CPtr -> onnx
    // ... use rgb (width()*height()*3 bytes, rows of stride()) ...
}

// 3. Transcode: re-encode video (+ keep audio), optionally filter.
const frames = ff.transcode("in.mov", "out.mp4",
    VideoEncoder.H264, "scale=1280:720", AudioEncoder.COPY)
println("wrote " + string(frames) + " frames with " + ff.lastVideoEncoder())
// pcm.close() and vid.close() run here automatically (LIFO).
```

> **Memory.** `AudioBuffer`, `VideoReader`, `VideoWriter` and `MediaInfo` own
> shim-side state and expose `close()`. Bind them with **`using const`** and the
> compiler closes each one for you on every exit path (return, throw, break) —
> the idiom throughout this manual. You can still call `.close()` explicitly
> (it's idempotent). Buffers returned by `data()` / `frame()` are **borrowed** and
> valid only until the next call / `close()`.

`new Ffmpeg(shimDir?)` builds (or loads the cached) shim. `shimDir` defaults to
`chuks_packages/@chuks/ffmpeg`; pass an explicit path only when developing
locally.

---

## The audio tap — `decodeAudio`

Decode a file's best audio stream and resample it to **16 kHz mono PCM** in one
packed, owned buffer, exposed as a **borrowed** pointer for zero-copy hand-off to
`@chuks/whisper` (or any consumer).

```chuks
const pcm = ff.decodeAudio(path, fmt)
```

| Argument | Default        | Description                                   |
| -------- | -------------- | --------------------------------------------- |
| `path`   | —              | Any media file with an audio stream.          |
| `fmt`    | `FFMPEG_F32` (0) | Output sample format: `FFMPEG_F32` or `FFMPEG_F64`. |

`AudioBuffer`:

| Call            | Description                                                        |
| --------------- | ----------------------------------------------------------------- |
| `.samples()`    | Number of 16 kHz mono samples.                                    |
| `.format()`     | `FFMPEG_F32` (0) or `FFMPEG_F64` (1).                             |
| `.data()`       | Borrowed `CPtr` to the packed PCM (byte length tagged). Zero-copy — pass straight to whisper; never copied through Chuks. |
| `.close()`      | Free the buffer.                                                  |

```chuks
import { Ffmpeg, FFMPEG_F64 } from "pkg/@chuks/ffmpeg"
const pcm = ff.decodeAudio("podcast.m4a", FFMPEG_F64)   // 64-bit float
```

---

## Video decode — `openVideo` / `VideoReader`

A streaming reader: each `next()` advances one frame, scaled to packed **RGB24**;
`frame()` borrows that buffer zero-copy (valid until the next `next()` /
`close()`).

```chuks
const vid = ff.openVideo(path, filter)
```

| Call             | Description                                                       |
| ---------------- | ---------------------------------------------------------------- |
| `.next()`        | Decode the next frame. `false` at end of stream.                 |
| `.width()`       | Frame width in pixels.                                           |
| `.height()`      | Frame height in pixels.                                          |
| `.stride()`      | Bytes per RGB row (`>= width*3`, alignment-padded).              |
| `.frame()`       | Borrowed `CPtr` to the current RGB24 frame (`stride*height` bytes). Zero-copy -> `@chuks/onnx`. |
| `.seek(seconds)` | Precise seek (see below).                                        |
| `.pts()`         | Current frame time in seconds (`-1` if unknown).                 |
| `.duration()`    | Total media duration in seconds (`-1` if unknown).              |
| `.close()`       | Free the reader.                                                 |

### Filters

Pass an FFmpeg video filtergraph as the second argument and every frame is run
through it before the RGB24 hand-off (empty = none):

```chuks
ff.openVideo("in.mp4", "scale=1280:720")                // resize
ff.openVideo("in.mp4", "crop=in_w/2:in_h:0:0")          // left half
ff.openVideo("in.mp4", "hflip")                         // mirror
ff.openVideo("in.mp4", "fps=15")                        // resample frame rate
ff.openVideo("in.mp4", "drawtext=text='LIVE':x=10:y=10")// burn-in text
ff.openVideo("in.mp4", "scale=640:-1,hflip")            // chain with commas
```

The graph is pull-driven, so frame-count-changing filters like `fps` drain
correctly (a 10-frame 10 fps clip through `fps=5` yields 5 frames). The output
stays RGB24, so downstream consumers are unaffected.

### Seeking

`seek(seconds)` is a **precise** seek: it seeks to the keyframe at or before the
target, then decodes and discards up to the target, so you land on the right
frame rather than the preceding keyframe.

```chuks
const vid = ff.openVideo("movie.mp4")
println("length: " + string(vid.duration()) + "s")
vid.seek(30.0)                                          // jump to 30s
while (vid.next()) {
    println("frame at " + string(vid.pts()) + "s")
    if (vid.pts() > 35.0) { break }                     // read 30s..35s
}
vid.close()
```

Seeking flushes the decoder and rebuilds any active filtergraph, so it composes
with the filter argument.

---

## Transcode — `transcode`

Decode → (filter) → re-encode video → mux, keeping the audio track.

```chuks
const frames = ff.transcode(inPath, outPath, encoder, filter, audio)
```

| Argument  | Default              | Description                                            |
| --------- | -------------------- | ----------------------------------------------------- |
| `inPath`  | —                    | Source media file.                                    |
| `outPath` | —                    | Output file (container inferred from extension).      |
| `encoder` | `VideoEncoder.H264`  | Video codec (see enum below).                          |
| `filter`  | `""`                 | Optional video filtergraph applied before encoding.   |
| `audio`   | `AudioEncoder.COPY`  | How the audio track is handled (see enum below).       |

Returns the number of video frames written. Throws on error (message via
`lastError()`).

```chuks
// Downscale to 720p, keep the original audio.
ff.transcode("4k.mov", "hd.mp4", VideoEncoder.H264, "scale=-2:720")

// Re-encode audio to AAC (e.g. source was mp3/opus/flac).
ff.transcode("in.mkv", "out.mp4", VideoEncoder.HEVC, "", AudioEncoder.AAC)

// Software, cross-platform, no hardware needed.
ff.transcode("in.mp4", "out.mp4", VideoEncoder.MPEG4)
```

Details: every frame is normalized to `yuv420p` through the filtergraph (any
decoder pixel format is handled), the encoder is opened lazily from the first
filtered frame (so a resize reshapes the output), video is stream 0 / audio is
stream 1, and per-packet durations are set so mp4 doesn't edit-list-trim the last
frame. Audio in `COPY` mode is stream-copied byte-for-byte; in `AAC` mode it is
decoded → `aformat` → AAC-encoded and interleaved with the video.

### `VideoEncoder`

| Value    | Resolves to                                                            |
| -------- | --------------------------------------------------------------------- |
| `H264`   | Best available H.264 encoder for the platform (VideoToolbox → NVENC → VAAPI → QSV → AMF → software). |
| `HEVC`   | Best available H.265/HEVC encoder, same order.                        |
| `MPEG4`  | Native MPEG-4 Part 2 (software, everywhere).                          |
| `MJPEG`  | Native Motion JPEG (software, everywhere).                            |

You pass a *family*; the shim picks the best encoder actually built for the host.
Use `lastVideoEncoder()` after a transcode to see what was chosen.

### `AudioEncoder`

| Value  | Behavior                                                                 |
| ------ | ------------------------------------------------------------------------ |
| `COPY` | Stream-copy the audio unchanged (fast, lossless). Default.               |
| `AAC`  | Re-encode the audio to AAC (needed when the container can't hold the source codec, or to normalize it). |

---

## Frame encoder — `createVideo` / `VideoWriter`

The inverse of `openVideo`: when your program *renders its own frames* (an editor,
a generator, a compositor), push them straight into an encoder instead of shelling
out to an `ffmpeg` binary or writing intermediate image files.

```chuks
// 640x360, 30 fps, hardware H.264.
const w = ff.createVideo("out.mp4", 640, 360, 30, VideoEncoder.H264)
for (var i = 0; i < 90; i = i + 1) {
    const rgb = renderMyFrameRGB24(i)   // a CPtr to width*height*3 bytes
    w.writeFrame(rgb)                   // stride defaults to width*3 (packed)
}
const frames = w.close()               // flush + finalize; returns frame count
```

| `createVideo` arg | Default             | Description                                       |
| ----------------- | ------------------- | ------------------------------------------------- |
| `outPath`         | —                   | Output file (container inferred from extension).  |
| `width` `height`  | —                   | Frame size in pixels.                             |
| `fps`             | `30`                | Frames per second (integer timebase `fps/1`).     |
| `encoder`         | `VideoEncoder.H264` | Codec family, resolved like `transcode`.          |

`w.close()` returns the frame count (as above). If you don't need the count, bind
the writer with `using const` and it finalizes automatically at block end.

`writeFrame(rgb, stride = 0)` encodes one RGB24 frame. `rgb` is a `CPtr` to
`width*height*3` bytes; `stride` is its bytes-per-row (`0` = packed `width*3`).
Zero-copy: the buffer is borrowed for the call only, so a decoder frame flows
straight through without a copy:

```chuks
// Re-encode by piping a reader's frames into a writer (padded stride and all).
using const src = ff.openVideo("in.mp4")
using const dst = ff.createVideo("out.mp4", 1920, 1080, 30)
while (src.next()) { dst.writeFrame(src.frame(), src.stride()) }
// dst.close() then src.close() run automatically at block end (LIFO).
```

## Muxing audio in — `mux`

`createVideo` writes a silent video. To attach a soundtrack (the common editor
export: render frames, then add the timeline's audio) stream-copy the two files
together:

```chuks
ff.createVideo("silent.mp4", w, h, 30)  // ... write frames ... close()
const n = ff.mux("silent.mp4", "score.m4a", "final.mp4")   // -> video pkt count
```

`mux(videoPath, audioPath, outPath)` takes the best video stream from the first
file and the best audio stream from the second, interleaves their packets by
timestamp, and copies both without re-encoding (no quality loss). Throws on error.

---

## Probe — `probe` / `MediaInfo`

Read a file's metadata **without decoding a single frame** — cheap enough to run
on every clip you import, to show its size/length or validate it before use.

```chuks
using const info = ff.probe("clip.mp4")
println(string(info.width()) + "x" + string(info.height())
    + " @ " + string(info.fps()) + "fps, " + string(info.duration()) + "s")
if (info.hasAudio()) {
    println("audio: " + info.audioCodec()
        + " " + string(info.sampleRate()) + "Hz x" + string(info.channels()))
}
```

`MediaInfo` accessors:

| Call             | Description                                          |
| ---------------- | ---------------------------------------------------- |
| `.hasVideo()` / `.hasAudio()` | Whether that stream is present.         |
| `.width()` `.height()` | Video frame size in pixels (`0` if none).      |
| `.duration()`    | Container duration in seconds (`-1` if unknown).     |
| `.fps()`         | Average video frame rate (`0` if none).              |
| `.frameCount()`  | Video frame count if the container records it (`0` if unknown). |
| `.bitRate()`     | Overall bitrate in bits/sec (`0` if unknown).        |
| `.sampleRate()` `.channels()` | Audio rate in Hz / channel count (`0` if none). |
| `.videoCodec()` `.audioCodec()` | Codec short names, e.g. `"h264"` / `"aac"` (empty if absent). |
| `.close()`       | Release the handle (or use `using const`).           |

All accessors are typed (no map/JSON), so the VM and AOT report identical values.

## Concat — `concat`

Join clips end to end by **stream copy** (no re-encode), the in-process
equivalent of the ffmpeg concat demuxer. This is what a chunked exporter needs to
stitch its segments into one file:

```chuks
const packets = ff.concat(
    ["chunk_000.mp4", "chunk_001.mp4", "chunk_002.mp4"],
    "final.mp4")
```

Streams are taken from the first file and each subsequent file's timestamps are
shifted by the running duration, so playback is continuous. **The inputs must
share a stream layout** (same codecs, resolution, sample rate) — exactly the case
for an editor's own export chunks. Returns the number of packets written; throws
on error. For clips with mismatched formats, normalize them with `transcode`
first, then `concat`.

---

## Encoder capabilities

```chuks
ff.hasEncoder("h264_videotoolbox")   // -> bool: is this encoder in the build?
ff.hasEncoder("h264_vaapi")          // probe hardware without attempting an encode
ff.lastVideoEncoder()                // -> the encoder the last transcode() used
```

Use these to branch on hardware availability (e.g. show a "GPU encode" badge, or
fall back to `MPEG4` where no H.264 encoder is built).

---

## Zero-copy (design rule)

Decoded media is large, so `chuks_ffmpeg` never round-trips it through Chuks:

- **Audio** — the packed 16 kHz PCM lives in the shim; `data()` returns a
  borrowed `CPtr` that `@chuks/whisper` reads directly.
- **Video** — each RGB24 frame lives in the shim; `frame()` returns a borrowed
  `CPtr` (+ `width`/`height`/`stride`) that `@chuks/onnx` turns into a tensor via
  `tensorFromF32Buf`-style borrowing.

Borrowed pointers are valid only until the next `next()` / decode / `close()`.
Copy out if you need to retain data past that.

---

## Platforms & prebuilts

The shim is self-contained on every platform (`ldd` / `otool -L` show no external
FFmpeg — only the OS's own libraries).

| Platform        | Suite (VM + AOT) | Hardware encode                          |
| --------------- | ---------------- | ---------------------------------------- |
| macOS arm64     | ✅ 8/8           | VideoToolbox (H.264/HEVC), runtime       |
| Linux aarch64   | ✅ 8/8           | VAAPI via `hw-vaapi` build feature       |
| Linux x86_64    | ✅ 8/8           | VAAPI via `hw-vaapi`, NVENC via `hw-nvenc` |
| macOS x86_64    | via CI           | VideoToolbox                             |
| Windows x86_64  | via CI           | (QSV/NVENC/AMF)                          |

Prebuilt libraries are published per-platform (built by the release workflow /
`shim-publish-action`) and resolved automatically at runtime from
`native_lib/@chuks/ffmpeg/.prebuilt/<platform>/`, so a deployed binary needs no
Rust toolchain.

### Hardware encoders

`H264` / `HEVC` resolve to the best encoder **built into the shim** for the host.
The default build is **software-only LGPL** (portable, no GPU needed). To add
Linux hardware encoders, build the shim with the matching feature and its SDK:

```bash
# VAAPI (Intel/AMD, most Linux desktops/servers) — needs libva-dev
cargo build --release --features hw-vaapi --manifest-path shim/Cargo.toml

# NVENC (NVIDIA) — needs nv-codec-headers
cargo build --release --features hw-nvenc --manifest-path shim/Cargo.toml
```

macOS enables VideoToolbox automatically (it is a macOS-only build flag).

---

## Building the shim locally

`new Ffmpeg()` builds the shim on first use (via `chuksToRust`) and caches it.
The `build` cargo feature compiles FFmpeg from source and static-links it, so the
first build takes a while; subsequent runs are instant. Requirements: a Rust
toolchain plus FFmpeg's build deps (`clang`/`libclang`, `pkg-config`, and on x86
`nasm`).

---

## Licensing

The code of this package (`shim/`, `src/`) is **MIT** (see `LICENSE`).

The prebuilt shim statically links **FFmpeg**, which is **LGPL-2.1-or-later**.
This package uses FFmpeg's default LGPL configuration with **no GPL / non-free
components** (no libx264, libx265, or GPL filters). If you distribute a binary
that bundles the prebuilt shim, the FFmpeg portion carries LGPL obligations:
recipients must be able to relink against a modified FFmpeg. This repository
satisfies that by shipping the shim source and the exact FFmpeg build recipe (the
`ffmpeg-next` `build` feature), so anyone can rebuild the shim against their own
FFmpeg. Retain the notices in `LICENSE`.

---

## See also

- [chuks_whisper](https://github.com/chuks-lang/chuks_whisper) — speech-to-text; consumes the audio tap.
- [chuks_onnx](https://github.com/chuks-lang/chuks_onnx) — run ONNX models; consumes video frames for effects.
- [ffmpeg-next](https://crates.io/crates/ffmpeg-next) — the Rust FFmpeg binding this shim wraps.
- [FFmpeg](https://www.ffmpeg.org) — the media engine.
