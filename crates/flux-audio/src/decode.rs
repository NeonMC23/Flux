//! Audio decoding: turns local audio files into interleaved `f32`
//! samples for the playback engine.
//!
//! Supported containers/codecs — exactly these, nothing more:
//!
//! | Extension(s) | Container | Codec | Decoder crate | License |
//! |---|---|---|---|---|
//! | `.wav` | WAV | PCM int/float | `hound` | Apache-2.0 |
//! | `.flac` | FLAC | FLAC | `claxon` | Apache-2.0 |
//! | `.mp3` | MPEG audio stream (ID3 tolerated) | MP3 (Layer III) | `rmp3` (minimp3) | CC0-1.0 |
//! | `.aac` | ADTS stream | AAC | `rusty_aac` | Apache-2.0 |
//! | `.ogg`, `.oga` | Ogg | Vorbis *or* Opus | `lewton` / `opus-pure` (+ `ogg`) | MIT/Apache-2.0, BSD-3-Clause |
//! | `.opus` | Ogg | Opus *or* Vorbis | `opus-pure` / `lewton` (+ `ogg`) | BSD-3-Clause, MIT/Apache-2.0 |
//! | `.m4a`, `.m4b`, `.mp4` | MP4 | AAC audio track | `mp4` + `rusty_aac` | MIT, Apache-2.0 |
//!
//! Everything else returns [`DecodeError::UnsupportedFormat`] with a
//! reason — including formats the library scanner happily *indexes*
//! (AIFF, APE, …). Indexed is not the same as playable, and this module
//! never pretends otherwise.
//!
//! Dispatch is by file extension (case-insensitive) to pick a
//! *container*, but every backend then validates the actual bytes:
//! MP3 must contain MPEG Layer III frames, `.aac` must contain ADTS
//! frames, Ogg files are routed by their first packet (`\x01vorbis`
//! vs `OpusHead`) regardless of which Ogg extension they carry, and
//! M4A/MP4 files must contain an AAC audio track. Bytes that disagree
//! with the extension yield [`DecodeError::Malformed`] (broken file)
//! or [`DecodeError::UnsupportedFormat`] (recognised container,
//! codec Flux cannot decode — for example ALAC inside `.m4a`, an MP4
//! with no AAC audio track, Speex inside Ogg, or raw AAC without ADTS
//! headers).
//!
//! Decoders are pull-based chunks of interleaved `f32` in the file's
//! own sample rate and channel layout; rate/channel conversion to the
//! output device's format happens in the engine, not here. All
//! backends stream from the file incrementally; none of them slurps a
//! whole (compressed) file into memory, and none of them requires an
//! audio device.
//!
//! Deliberate limits of this milestone, all reported rather than
//! papered over: no multichannel (surround-mapping) Opus; MP3/ADTS/Ogg
//! streams report no upfront duration; stream parameters must not
//! change mid-file; only the first Ogg logical stream and the first
//! AAC track of an MP4 are decoded.

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// File extensions [`open_decoder`] dispatches on (compared ASCII
/// case-insensitively). Membership only means " Flux will *try* this
/// file"; the contents still have to parse.
pub const PLAYABLE_EXTENSIONS: &[&str] = &[
    "wav", "flac", "mp3", "aac", "ogg", "oga", "opus", "m4a", "m4b", "mp4",
];

/// Whether `path`'s extension is one [`open_decoder`] can handle. This
/// says nothing about whether the file's contents are valid.
#[must_use]
pub fn is_playable(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            PLAYABLE_EXTENSIONS
                .iter()
                .any(|k| k.eq_ignore_ascii_case(ext))
        })
}

/// Static facts about an opened audio stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// Samples per second per channel, as stored in the file.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Total duration when the container states it up front.
    pub duration: Option<Duration>,
}

/// Errors produced while opening or decoding an audio file.
#[derive(Debug)]
pub enum DecodeError {
    /// The file could not be read (missing, permissions, …).
    Io(std::io::Error),
    /// The file is not something Flux can decode: either an extension
    /// with no backend (the file is never opened) or a recognised
    /// container carrying a codec Flux does not decode.
    UnsupportedFormat {
        /// The path that was rejected.
        path: PathBuf,
        /// What is unsupported about it.
        reason: String,
    },
    /// The file claims to be a supported format but does not parse.
    Malformed {
        /// The path that failed to parse.
        path: PathBuf,
        /// The parser's explanation.
        reason: String,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "could not read audio file: {err}"),
            Self::UnsupportedFormat { path, reason } => {
                write!(f, "unsupported audio format: {} ({reason})", path.display())
            }
            Self::Malformed { path, reason } => {
                write!(f, "malformed audio file {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::UnsupportedFormat { .. } | Self::Malformed { .. } => None,
        }
    }
}

impl From<std::io::Error> for DecodeError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

fn malformed(path: &Path, reason: String) -> DecodeError {
    DecodeError::Malformed {
        path: path.to_path_buf(),
        reason,
    }
}

fn unsupported(path: &Path, reason: String) -> DecodeError {
    DecodeError::UnsupportedFormat {
        path: path.to_path_buf(),
        reason,
    }
}

/// Runs a third-party codec call, converting a panic inside it into a
/// typed [`DecodeError::Malformed`] instead of letting it kill the
/// engine's worker thread. The decoders process untrusted input, and
/// indexing bugs in codec code must degrade to "this file is broken",
/// never to a hung player. After such an error the engine drops the
/// decoder, so possibly-inconsistent codec state is never reused.
fn catch_codec_panics<T>(
    path: &Path,
    what: &str,
    f: impl FnOnce() -> Result<T, DecodeError>,
) -> Result<T, DecodeError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(malformed(
            path,
            format!("{what} decoder crashed on this bitstream (contained)"),
        )),
    }
}

/// Sanity limits applied to stream parameters reported by codecs. The
/// engine's converter indexes by channel count and divides by sample
/// rate, so absurd header values must be rejected at this boundary.
fn check_params(
    path: &Path,
    format: &str,
    sample_rate: u32,
    channels: u16,
) -> Result<(), DecodeError> {
    if !(1_000..=768_000).contains(&sample_rate) || !(1..=8).contains(&channels) {
        return Err(malformed(
            path,
            format!(
                "implausible {format} stream parameters ({sample_rate} Hz, {channels} channel(s))"
            ),
        ));
    }
    Ok(())
}

/// A pull-based decoder producing interleaved `f32` samples in the
/// file's native sample rate and channel layout.
///
/// Implementations perform file I/O and CPU work in
/// [`next_chunk`](Decoder::next_chunk); they belong on a worker thread,
/// never in an audio callback or on a UI thread.
pub trait Decoder: Send {
    /// Facts about the stream, known once the decoder is open.
    fn info(&self) -> &StreamInfo;

    /// Decodes up to `max_frames` frames (one frame = one sample per
    /// channel). Returns fewer at end-of-stream; an **empty** vector
    /// means permanent end-of-stream, after which callers must not call
    /// again.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Io`] for read failures and
    /// [`DecodeError::Malformed`] for parse failures discovered mid-stream.
    fn next_chunk(&mut self, max_frames: usize) -> Result<Vec<f32>, DecodeError>;
}

/// Opens the decoder for `path`, chosen by file extension
/// (case-insensitive) with the contents validated by each backend —
/// see the module documentation for the exact support matrix.
///
/// # Errors
///
/// [`DecodeError::UnsupportedFormat`] for extensions with no backend
/// (the file is not opened) and for recognised containers whose codec
/// Flux cannot decode; [`DecodeError::Io`] if the file cannot be read;
/// [`DecodeError::Malformed`] if it does not parse as its extension
/// claims.
pub fn open_decoder(path: &Path) -> Result<Box<dyn Decoder>, DecodeError> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "wav" => Ok(Box::new(WavDecoder::open(path)?)),
        "flac" => Ok(Box::new(FlacDecoder::open(path)?)),
        "mp3" => Ok(Box::new(Mp3Decoder::open(path)?)),
        "aac" => Ok(Box::new(AdtsAacDecoder::open(path)?)),
        "ogg" | "oga" | "opus" => open_ogg(path),
        "m4a" | "m4b" | "mp4" => Ok(Box::new(Mp4AacDecoder::open(path)?)),
        "" => Err(unsupported(path, "file has no extension".to_string())),
        other => Err(unsupported(
            path,
            format!("no decoder for '.{other}' files"),
        )),
    }
}

// ---------------------------------------------------------------- WAV ---

struct WavDecoder {
    reader: hound::WavReader<std::io::BufReader<std::fs::File>>,
    info: StreamInfo,
    format: hound::SampleFormat,
    /// Normalization factor for integer samples: `1 / 2^(bits-1)`.
    /// (hound yields integers in their native bit-depth range.)
    int_scale: f32,
    path: PathBuf,
}

impl WavDecoder {
    fn open(path: &Path) -> Result<Self, DecodeError> {
        let reader = hound::WavReader::open(path).map_err(|e| map_hound_error(e, path))?;
        let spec = reader.spec();
        if spec.channels == 0 || spec.sample_rate == 0 {
            return Err(DecodeError::Malformed {
                path: path.to_path_buf(),
                reason: format!(
                    "invalid WAV spec ({} channel(s), {} Hz)",
                    spec.channels, spec.sample_rate
                ),
            });
        }
        let frames = u64::from(reader.duration());
        let info = StreamInfo {
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            duration: (frames > 0)
                .then(|| Duration::from_secs_f64(frames as f64 / f64::from(spec.sample_rate))),
        };
        let int_scale = match spec.sample_format {
            hound::SampleFormat::Int if spec.bits_per_sample > 0 => {
                1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32
            }
            _ => 1.0,
        };
        Ok(Self {
            reader,
            info,
            format: spec.sample_format,
            int_scale,
            path: path.to_path_buf(),
        })
    }
}

impl Decoder for WavDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn next_chunk(&mut self, max_frames: usize) -> Result<Vec<f32>, DecodeError> {
        let want = max_frames * usize::from(self.info.channels);
        let mut out = Vec::with_capacity(want);
        match self.format {
            hound::SampleFormat::Float => {
                for sample in self.reader.samples::<f32>().take(want) {
                    out.push(sample.map_err(|e| map_hound_error(e, &self.path))?);
                }
            }
            hound::SampleFormat::Int => {
                for sample in self.reader.samples::<i32>().take(want) {
                    let value = sample.map_err(|e| map_hound_error(e, &self.path))?;
                    out.push(value as f32 * self.int_scale);
                }
            }
        }
        Ok(out)
    }
}

fn map_hound_error(err: hound::Error, path: &Path) -> DecodeError {
    match err {
        hound::Error::IoError(io) => DecodeError::Io(io),
        other => DecodeError::Malformed {
            path: path.to_path_buf(),
            reason: other.to_string(),
        },
    }
}

// --------------------------------------------------------------- FLAC ---

struct FlacDecoder {
    reader: claxon::FlacReader<std::fs::File>,
    info: StreamInfo,
    /// Normalization factor: `1 / 2^(bits_per_sample-1)`.
    scale: f32,
    /// Interleaved samples decoded but not yet handed out.
    pending: VecDeque<f32>,
    /// Reusable claxon decode buffer (its API takes ownership per block).
    scratch: Vec<i32>,
    eof: bool,
    path: PathBuf,
}

impl FlacDecoder {
    fn open(path: &Path) -> Result<Self, DecodeError> {
        let reader = claxon::FlacReader::open(path).map_err(|e| map_claxon_error(e, path))?;
        let stream = reader.streaminfo();
        if stream.channels == 0 || stream.sample_rate == 0 || stream.bits_per_sample == 0 {
            return Err(DecodeError::Malformed {
                path: path.to_path_buf(),
                reason: format!(
                    "invalid FLAC streaminfo ({} channel(s), {} Hz, {} bits)",
                    stream.channels, stream.sample_rate, stream.bits_per_sample
                ),
            });
        }
        let channels = u16::try_from(stream.channels).map_err(|_| DecodeError::Malformed {
            path: path.to_path_buf(),
            reason: format!("implausible FLAC channel count {}", stream.channels),
        })?;
        let info = StreamInfo {
            sample_rate: stream.sample_rate,
            channels,
            duration: stream
                .samples
                .filter(|s| *s > 0)
                .map(|s| Duration::from_secs_f64(s as f64 / f64::from(stream.sample_rate))),
        };
        Ok(Self {
            reader,
            info,
            scale: 1.0 / (1u64 << (stream.bits_per_sample - 1)) as f32,
            pending: VecDeque::new(),
            scratch: Vec::new(),
            eof: false,
            path: path.to_path_buf(),
        })
    }

    /// Decodes one FLAC block into `pending`. Returns false at EOF.
    fn decode_block(&mut self) -> Result<bool, DecodeError> {
        let mut blocks = self.reader.blocks();
        let block = blocks
            .read_next_or_eof(std::mem::take(&mut self.scratch))
            .map_err(|e| map_claxon_error(e, &self.path))?;
        let Some(block) = block else { return Ok(false) };
        let frames = block.duration() as usize;
        let channels = block.channels() as usize;
        // Interleave deinterleaved channel buffers.
        for frame in 0..frames {
            for ch in 0..channels {
                let sample = block.channel(ch as u32)[frame];
                self.pending.push_back(sample as f32 * self.scale);
            }
        }
        self.scratch = block.into_buffer();
        Ok(true)
    }
}

impl Decoder for FlacDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn next_chunk(&mut self, max_frames: usize) -> Result<Vec<f32>, DecodeError> {
        let want = max_frames * usize::from(self.info.channels);
        while self.pending.len() < want && !self.eof {
            if !self.decode_block()? {
                self.eof = true;
            }
        }
        let take = want.min(self.pending.len());
        Ok(self.pending.drain(..take).collect())
    }
}

fn map_claxon_error(err: claxon::Error, path: &Path) -> DecodeError {
    match err {
        claxon::Error::IoError(io) => DecodeError::Io(io),
        other => DecodeError::Malformed {
            path: path.to_path_buf(),
            reason: other.to_string(),
        },
    }
}

// ---------------------------------------------------------------- MP3 ---

/// Bytes read from the file per refill for the byte-stream backends.
const READ_CHUNK_BYTES: usize = 32 * 1024;

/// MP3 (MPEG Layer III) over the minimp3 decoder. minimp3 is fed a
/// sliding window of compressed bytes; its state (including the bit
/// reservoir) lives in the decoder struct, so frames split across
/// window refills decode cleanly.
///
/// The C entry points are called through `rmp3::ffi` directly instead
/// of rmp3's high-level `RawDecoder`: rmp3 0.3.1's convenience layer
/// builds slices with `get_unchecked` from partially initialised
/// frame info, which is undefined behaviour on corrupt streams (it
/// aborts debug builds and can read out of bounds in release builds)
/// — see the `hostile_inputs_terminate_cleanly_in_every_backend` test
/// and its mutated MP3 fixture. The small wrapper below keeps every
/// offset in checked Rust arithmetic and never trusts C results
/// beyond the buffer bounds handed in.
struct Minimp3 {
    state: Box<rmp3::ffi::mp3dec_t>,
}

/// What one `mp3dec_decode_frame` call reports, already clamped to
/// the buffer that was handed in.
struct Mp3Frame {
    /// PCM samples per channel written to the output buffer (0 = no
    /// complete Layer III frame was decoded).
    samples_per_channel: usize,
    /// Bytes the caller may advance past (leading junk/tags plus the
    /// frame itself). 0 means "need more data".
    consumed: usize,
    channels: u16,
    sample_rate: u32,
}

impl Minimp3 {
    fn new() -> Self {
        // SAFETY: `mp3dec_t` is a plain-old-data C struct (float and
        // integer arrays), so all-zero is a valid bit pattern;
        // `mp3dec_init` then establishes the decoder invariants.
        let mut state: Box<rmp3::ffi::mp3dec_t> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: `state` is writable owned storage of the right
        // type, as `mp3dec_init` requires.
        unsafe { rmp3::ffi::mp3dec_init(&mut *state) };
        Self { state }
    }

    fn decode_frame(
        &mut self,
        src: &[u8],
        pcm: &mut [f32; rmp3::MAX_SAMPLES_PER_FRAME],
    ) -> Mp3Frame {
        // SAFETY: `mp3dec_frame_info_t` is six C ints; all-zero is a
        // valid value. Pre-zeroing matters because minimp3 leaves
        // some fields unset on its no-frame paths.
        let mut info: rmp3::ffi::mp3dec_frame_info_t = unsafe { std::mem::zeroed() };
        let len = src.len().min(i32::MAX as usize);
        // SAFETY: `src` and `pcm` are valid for `len` and
        // `MAX_SAMPLES_PER_FRAME` elements for the duration of the
        // call; minimp3 contractually reads at most `len` input
        // bytes and writes at most `MAX_SAMPLES_PER_FRAME` samples;
        // `state` and `info` are exclusively borrowed here.
        let samples = unsafe {
            rmp3::ffi::mp3dec_decode_frame(
                &mut *self.state,
                src.as_ptr(),
                len as std::ffi::c_int,
                pcm.as_mut_ptr(),
                &mut info,
            )
        };
        // Clamp everything the C side reported, so our arithmetic
        // stays in bounds even if a bitstream confuses it.
        let channels = info.channels.clamp(0, 2) as u16;
        let mut samples_per_channel = samples.max(0) as usize;
        if channels == 0
            || samples_per_channel * usize::from(channels) > rmp3::MAX_SAMPLES_PER_FRAME
        {
            samples_per_channel = 0;
        }
        Mp3Frame {
            samples_per_channel,
            consumed: (info.frame_bytes.max(0) as usize).min(len),
            channels,
            sample_rate: info.hz.max(0) as u32,
        }
    }
}

struct Mp3Decoder {
    reader: BufReader<File>,
    decoder: Minimp3,
    /// Unconsumed compressed bytes; the front is the current position.
    window: Vec<u8>,
    /// minimp3 PCM output buffer (f32 with the `float` feature).
    scratch: Box<[f32; rmp3::MAX_SAMPLES_PER_FRAME]>,
    pending: VecDeque<f32>,
    file_eof: bool,
    done: bool,
    /// Learned from the first audio frame during `open`.
    info: StreamInfo,
    path: PathBuf,
}

impl Mp3Decoder {
    fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path)?;
        let mut this = Self {
            reader: BufReader::new(file),
            decoder: Minimp3::new(),
            window: Vec::new(),
            scratch: Box::new([0.0; rmp3::MAX_SAMPLES_PER_FRAME]),
            pending: VecDeque::new(),
            file_eof: false,
            done: false,
            info: StreamInfo {
                sample_rate: 0,
                channels: 0,
                duration: None,
            },
            path: path.to_path_buf(),
        };
        // Prime: decode until the first audio frame is buffered (this
        // also proves the file really is MP3 and fixes the stream
        // parameters). ID3 tags and junk between frames are skipped by
        // minimp3's frame scanner.
        while this.info.sample_rate == 0 {
            if !this.step()? {
                return Err(malformed(
                    path,
                    "no MPEG Layer III audio frames found".to_string(),
                ));
            }
        }
        Ok(this)
    }

    /// Refills `window` from the file. Returns false at end of file.
    fn fill_window(&mut self) -> Result<bool, DecodeError> {
        if self.file_eof {
            return Ok(false);
        }
        let mut buf = [0u8; READ_CHUNK_BYTES];
        let read = self.reader.read(&mut buf)?;
        if read == 0 {
            self.file_eof = true;
            return Ok(false);
        }
        self.window.extend_from_slice(&buf[..read]);
        Ok(true)
    }

    /// Decodes one frame into `pending`, skipping any non-audio
    /// regions on the way. Returns false once the stream is
    /// exhausted.
    fn step(&mut self) -> Result<bool, DecodeError> {
        if self.done {
            return Ok(false);
        }
        loop {
            if self.window.is_empty() && !self.fill_window()? {
                self.done = true;
                return Ok(false);
            }
            let frame = self.decoder.decode_frame(&self.window, &mut self.scratch);
            if frame.samples_per_channel > 0 {
                if frame.consumed == 0 {
                    // Would re-decode the same bytes forever; treat
                    // as corruption instead of looping.
                    return Err(malformed(
                        &self.path,
                        "MP3 decoder made no progress on a frame".to_string(),
                    ));
                }
                if self.info.sample_rate == 0 {
                    check_params(&self.path, "MP3", frame.sample_rate, frame.channels)?;
                    self.info.sample_rate = frame.sample_rate;
                    self.info.channels = frame.channels;
                } else if frame.sample_rate != self.info.sample_rate
                    || frame.channels != self.info.channels
                {
                    return Err(malformed(
                        &self.path,
                        format!(
                            "MP3 stream parameters change mid-stream \
                             ({} Hz/{} ch after {} Hz/{} ch)",
                            frame.sample_rate,
                            frame.channels,
                            self.info.sample_rate,
                            self.info.channels
                        ),
                    ));
                }
                let count = frame.samples_per_channel * usize::from(frame.channels);
                self.pending.extend(self.scratch[..count].iter().copied());
                self.window.drain(..frame.consumed);
                return Ok(true);
            }
            if frame.consumed > 0 {
                // Junk, a tag, or a non-Layer-III frame that minimp3
                // skipped: drop it and look at what follows.
                self.window.drain(..frame.consumed);
                continue;
            }
            // More bytes are needed to complete (or rule out) a frame.
            if self.file_eof {
                self.window.clear();
                self.done = true;
                return Ok(false);
            }
            if self.window.len() > 1024 * 1024 {
                return Err(malformed(
                    &self.path,
                    "over 1 MiB of data without a complete MP3 frame".to_string(),
                ));
            }
            self.fill_window()?;
        }
    }
}

impl Decoder for Mp3Decoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn next_chunk(&mut self, max_frames: usize) -> Result<Vec<f32>, DecodeError> {
        let want = max_frames * usize::from(self.info.channels);
        while self.pending.len() < want && self.step()? {}
        let take = want.min(self.pending.len());
        Ok(self.pending.drain(..take).collect())
    }
}

// ----------------------------------------------------------- ADTS AAC ---

/// AAC in an ADTS stream (the usual form of a `.aac` file). Raw AAC
/// access units without ADTS headers have no self-describing framing
/// and are reported as malformed here; AAC in MP4 is handled by
/// [`Mp4AacDecoder`].
struct AdtsAacDecoder {
    reader: BufReader<File>,
    decoder: rusty_aac::AacDecoder,
    window: Vec<u8>,
    pending: VecDeque<f32>,
    file_eof: bool,
    done: bool,
    /// Bytes skipped while searching for the *first* ADTS frame
    /// (leading ID3 tags are fine; megabytes of junk are not).
    presync_skipped: usize,
    info: StreamInfo,
    path: PathBuf,
}

/// Maximum junk tolerated before the first ADTS frame (covers large
/// ID3 tags with embedded art while still rejecting non-AAC files).
const ADTS_PRESYNC_LIMIT: usize = 1024 * 1024;

impl AdtsAacDecoder {
    fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path)?;
        let mut this = Self {
            reader: BufReader::new(file),
            decoder: rusty_aac::AacDecoder::new(),
            window: Vec::new(),
            pending: VecDeque::new(),
            file_eof: false,
            done: false,
            presync_skipped: 0,
            info: StreamInfo {
                sample_rate: 0,
                channels: 0,
                duration: None,
            },
            path: path.to_path_buf(),
        };
        while this.info.sample_rate == 0 {
            if !this.step()? {
                return Err(malformed(
                    path,
                    "no ADTS frames found (raw AAC without ADTS headers is not supported)"
                        .to_string(),
                ));
            }
        }
        Ok(this)
    }

    fn fill_window(&mut self) -> Result<bool, DecodeError> {
        if self.file_eof {
            return Ok(false);
        }
        let mut buf = [0u8; READ_CHUNK_BYTES];
        let read = self.reader.read(&mut buf)?;
        if read == 0 {
            self.file_eof = true;
            return Ok(false);
        }
        self.window.extend_from_slice(&buf[..read]);
        Ok(true)
    }

    /// Decodes one ADTS frame into `pending`. Returns false once the
    /// stream is exhausted. Bytes that are not an ADTS header are
    /// skipped one at a time (resync), bounded before the first frame.
    fn step(&mut self) -> Result<bool, DecodeError> {
        if self.done {
            return Ok(false);
        }
        let path = self.path.clone();
        loop {
            while self.window.len() < 7 && !self.file_eof {
                self.fill_window()?;
            }
            if self.window.len() < 7 {
                self.done = true;
                return Ok(false);
            }
            let header = match rusty_aac::parse_adts(&self.window) {
                Ok(h) if h.frame_length >= h.header_len => h,
                Ok(_) | Err(_) => {
                    // Not a (plausible) header: slide by one byte.
                    self.window.drain(..1);
                    if self.info.sample_rate == 0 {
                        self.presync_skipped += 1;
                        if self.presync_skipped > ADTS_PRESYNC_LIMIT {
                            return Err(malformed(
                                &path,
                                "no ADTS frame sync found in the first 1 MiB".to_string(),
                            ));
                        }
                    }
                    continue;
                }
            };
            let frame_len = header.frame_length as usize;
            while self.window.len() < frame_len && !self.file_eof {
                self.fill_window()?;
            }
            if self.window.len() < frame_len {
                // Truncated final frame: what is already decoded
                // stands; the stream simply ends.
                self.done = true;
                return Ok(false);
            }
            let frame: Vec<u8> = self.window.drain(..frame_len).collect();
            let audio = catch_codec_panics(&path, "AAC", || {
                self.decoder
                    .decode(&frame, None)
                    .map_err(|e| malformed(&path, format!("AAC decode failed: {e}")))
            })?;
            if self.info.sample_rate == 0 {
                check_params(&path, "AAC", audio.sample_rate, audio.channels)?;
                self.info.sample_rate = audio.sample_rate;
                self.info.channels = audio.channels;
            } else if audio.sample_rate != self.info.sample_rate
                || audio.channels != self.info.channels
            {
                return Err(malformed(
                    &path,
                    format!(
                        "AAC stream parameters change mid-stream \
                         ({} Hz/{} ch after {} Hz/{} ch)",
                        audio.sample_rate,
                        audio.channels,
                        self.info.sample_rate,
                        self.info.channels
                    ),
                ));
            }
            self.pending.extend(audio.samples);
            return Ok(true);
        }
    }
}

impl Decoder for AdtsAacDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn next_chunk(&mut self, max_frames: usize) -> Result<Vec<f32>, DecodeError> {
        let want = max_frames * usize::from(self.info.channels);
        while self.pending.len() < want && self.step()? {}
        let take = want.min(self.pending.len());
        Ok(self.pending.drain(..take).collect())
    }
}

// ---------------------------------------------------------------- Ogg ---

type OggPacketReader = ogg::reading::PacketReader<BufReader<File>>;

fn map_ogg_error(err: ogg::OggReadError, path: &Path) -> DecodeError {
    match err {
        ogg::OggReadError::ReadError(io) => DecodeError::Io(io),
        other => malformed(path, format!("invalid Ogg stream: {other}")),
    }
}

/// Ogg dispatch: the container is chosen by extension, but the codec
/// is chosen by the first packet's magic, so an Opus stream named
/// `.ogg` (and a Vorbis stream named `.opus`) still plays.
fn open_ogg(path: &Path) -> Result<Box<dyn Decoder>, DecodeError> {
    let file = File::open(path)?;
    let mut reader = ogg::reading::PacketReader::new(BufReader::new(file));
    let first = match reader.read_packet() {
        Ok(Some(packet)) => packet,
        Ok(None) => {
            return Err(malformed(path, "empty Ogg stream (no packets)".to_string()));
        }
        Err(e) => return Err(map_ogg_error(e, path)),
    };
    if first.data.starts_with(b"\x01vorbis") {
        Ok(Box::new(VorbisDecoder::new(path, reader, first)?))
    } else if first.data.starts_with(b"OpusHead") {
        Ok(Box::new(OpusDecoder::new(path, reader, first)?))
    } else {
        Err(unsupported(
            path,
            "Ogg container holds a codec Flux cannot decode (only Vorbis and Opus are supported)"
                .to_string(),
        ))
    }
}

/// Reads packets of one chosen logical stream, skipping any others
/// (muxed Ogg files may interleave streams; only the first stream is
/// decoded). Returns `Ok(None)` at end of stream.
fn next_stream_packet(
    reader: &mut OggPacketReader,
    serial: u32,
    path: &Path,
) -> Result<Option<Vec<u8>>, DecodeError> {
    loop {
        match reader.read_packet() {
            Ok(Some(packet)) => {
                if packet.stream_serial() == serial {
                    return Ok(Some(packet.data));
                }
            }
            Ok(None) => return Ok(None),
            Err(e) => return Err(map_ogg_error(e, path)),
        }
    }
}

// ---------------------------------------------------------- Ogg Vorbis --

struct VorbisDecoder {
    reader: OggPacketReader,
    serial: u32,
    ident: lewton::header::IdentHeader,
    setup: lewton::header::SetupHeader,
    window_state: lewton::audio::PreviousWindowRight,
    pending: VecDeque<f32>,
    done: bool,
    info: StreamInfo,
    path: PathBuf,
}

impl VorbisDecoder {
    fn new(
        path: &Path,
        mut reader: OggPacketReader,
        first: ogg::Packet,
    ) -> Result<Self, DecodeError> {
        let serial = first.stream_serial();
        let ident = catch_codec_panics(path, "Vorbis", || {
            lewton::header::read_header_ident(&first.data)
                .map_err(|e| malformed(path, format!("invalid Vorbis identification header: {e}")))
        })?;
        let channels = u16::from(ident.audio_channels);
        check_params(path, "Vorbis", ident.audio_sample_rate, channels)?;
        let comment = next_stream_packet(&mut reader, serial, path)?.ok_or_else(|| {
            malformed(
                path,
                "truncated Vorbis headers (missing comment header)".to_string(),
            )
        })?;
        catch_codec_panics(path, "Vorbis", || {
            lewton::header::read_header_comment(&comment)
                .map(|_| ())
                .map_err(|e| malformed(path, format!("invalid Vorbis comment header: {e}")))
        })?;
        let setup_packet = next_stream_packet(&mut reader, serial, path)?.ok_or_else(|| {
            malformed(
                path,
                "truncated Vorbis headers (missing setup header)".to_string(),
            )
        })?;
        let setup = catch_codec_panics(path, "Vorbis", || {
            lewton::header::read_header_setup(
                &setup_packet,
                ident.audio_channels,
                (ident.blocksize_0, ident.blocksize_1),
            )
            .map_err(|e| malformed(path, format!("invalid Vorbis setup header: {e}")))
        })?;
        let info = StreamInfo {
            sample_rate: ident.audio_sample_rate,
            channels,
            duration: None,
        };
        Ok(Self {
            reader,
            serial,
            ident,
            setup,
            window_state: lewton::audio::PreviousWindowRight::new(),
            pending: VecDeque::new(),
            done: false,
            info,
            path: path.to_path_buf(),
        })
    }

    /// Decodes one Vorbis packet into `pending` (the first audio
    /// packet of a stream legitimately yields zero samples — it
    /// primes the overlap window). Returns false at end of stream.
    fn step(&mut self) -> Result<bool, DecodeError> {
        if self.done {
            return Ok(false);
        }
        let Some(packet) = next_stream_packet(&mut self.reader, self.serial, &self.path)? else {
            self.done = true;
            return Ok(false);
        };
        let path = self.path.clone();
        let channels_pcm = catch_codec_panics(&path, "Vorbis", || {
            lewton::audio::read_audio_packet_generic::<Vec<Vec<f32>>>(
                &self.ident,
                &self.setup,
                &packet,
                &mut self.window_state,
            )
            .map_err(|e| malformed(&path, format!("Vorbis decode failed: {e}")))
        })?;
        // Interleave per-channel planes into pending.
        if let Some((first, rest)) = channels_pcm.split_first() {
            for frame in 0..first.len() {
                self.pending.push_back(first[frame]);
                for plane in rest {
                    self.pending.push_back(plane[frame]);
                }
            }
        }
        Ok(true)
    }
}

impl Decoder for VorbisDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn next_chunk(&mut self, max_frames: usize) -> Result<Vec<f32>, DecodeError> {
        let want = max_frames * usize::from(self.info.channels);
        while self.pending.len() < want && self.step()? {}
        let take = want.min(self.pending.len());
        Ok(self.pending.drain(..take).collect())
    }
}

// ------------------------------------------------------------ Ogg Opus --

/// Opus always decodes at 48 kHz (the rate in OpusHead is the
/// *original* input rate and is informational only).
const OPUS_DECODE_RATE: u32 = 48_000;
/// Longest legal Opus packet output: 120 ms at 48 kHz.
const OPUS_MAX_PACKET_FRAMES: usize = 5_760;

struct OpusDecoder {
    reader: OggPacketReader,
    serial: u32,
    decoder: opus_pure::OpusDecoder,
    /// First audio packet, when the stream had no OpusTags packet to
    /// skip and the packet read for that check turned out to be audio.
    stashed: Option<Vec<u8>>,
    scratch: Box<[f32; OPUS_MAX_PACKET_FRAMES * 2]>,
    /// Frames left to discard at the start (OpusHead `pre_skip`).
    preskip: usize,
    pending: VecDeque<f32>,
    done: bool,
    info: StreamInfo,
    path: PathBuf,
}

impl OpusDecoder {
    fn new(
        path: &Path,
        mut reader: OggPacketReader,
        first: ogg::Packet,
    ) -> Result<Self, DecodeError> {
        // OpusHead (RFC 7845 §5.1): magic(8) version(1) channels(1)
        // pre_skip(2 LE) input_rate(4) gain(2) mapping_family(1) …
        if first.data.len() < 19 {
            return Err(malformed(path, "truncated OpusHead header".to_string()));
        }
        let version = first.data[8];
        if version > 1 {
            return Err(malformed(
                path,
                format!("unsupported OpusHead version {version}"),
            ));
        }
        let channels = first.data[9];
        let preskip = u16::from_le_bytes([first.data[10], first.data[11]]) as usize;
        let family = first.data[18];
        if family != 0 {
            return Err(unsupported(
                path,
                format!("multichannel Opus (channel mapping family {family}) is not supported"),
            ));
        }
        check_params(path, "Opus", OPUS_DECODE_RATE, u16::from(channels))?;
        if channels > 2 {
            return Err(unsupported(
                path,
                format!("{channels}-channel Opus is not supported (mono/stereo only)"),
            ));
        }
        let decoder =
            opus_pure::OpusDecoder::new(OPUS_DECODE_RATE as i32, usize::from(channels))
                .map_err(|e| malformed(path, format!("could not create Opus decoder: {e}")))?;
        let serial = first.stream_serial();
        // RFC 7845 wants exactly one OpusTags header packet next;
        // skip it if present, but tolerate streams where audio starts
        // immediately by stashing that packet instead of dropping it.
        let mut stashed = None;
        if let Some(second) = next_stream_packet(&mut reader, serial, path)?
            && !second.starts_with(b"OpusTags")
        {
            stashed = Some(second);
        }
        Ok(Self {
            reader,
            serial,
            decoder,
            stashed,
            scratch: Box::new([0.0; OPUS_MAX_PACKET_FRAMES * 2]),
            preskip,
            pending: VecDeque::new(),
            done: false,
            info: StreamInfo {
                sample_rate: OPUS_DECODE_RATE,
                channels: u16::from(channels),
                duration: None,
            },
            path: path.to_path_buf(),
        })
    }

    /// Decodes one Opus packet into `pending`. Returns false at end
    /// of stream.
    fn step(&mut self) -> Result<bool, DecodeError> {
        if self.done {
            return Ok(false);
        }
        let packet = match self.stashed.take() {
            Some(packet) => Some(packet),
            None => next_stream_packet(&mut self.reader, self.serial, &self.path)?,
        };
        let Some(packet) = packet else {
            self.done = true;
            return Ok(false);
        };
        let path = self.path.clone();
        let frames = catch_codec_panics(&path, "Opus", || {
            self.decoder
                .decode(&packet, OPUS_MAX_PACKET_FRAMES, &mut self.scratch[..])
                .map_err(|e| malformed(&path, format!("Opus decode failed: {e}")))
        })?;
        let channels = usize::from(self.info.channels);
        let skip = self.preskip.min(frames);
        self.preskip -= skip;
        self.pending.extend(
            self.scratch[skip * channels..frames * channels]
                .iter()
                .copied(),
        );
        Ok(true)
    }
}

impl Decoder for OpusDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn next_chunk(&mut self, max_frames: usize) -> Result<Vec<f32>, DecodeError> {
        let want = max_frames * usize::from(self.info.channels);
        while self.pending.len() < want && self.step()? {}
        let take = want.min(self.pending.len());
        Ok(self.pending.drain(..take).collect())
    }
}

// ------------------------------------------------------------- MP4/M4A --

/// AAC inside MP4 (`.m4a`, `.m4b`, `.mp4`): the `mp4` crate demuxes
/// access units, `rusty_aac` decodes them with an
/// `AudioSpecificConfig` rebuilt from the track's esds box. ALAC
/// tracks, video-only files, and other codecs are rejected by name.
struct Mp4AacDecoder {
    reader: mp4::Mp4Reader<BufReader<File>>,
    decoder: rusty_aac::AacDecoder,
    track_id: u32,
    next_sample: u32,
    sample_count: u32,
    pending: VecDeque<f32>,
    /// Rate/channels come from the first decoded sample (this is what
    /// makes HE-AAC output rates and PS stereo report correctly).
    info: StreamInfo,
    path: PathBuf,
}

impl Mp4AacDecoder {
    fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path)?;
        let reader = catch_codec_panics(path, "MP4", || {
            mp4::read_mp4(file)
                .map_err(|e| malformed(path, format!("not a readable MP4/M4A file: {e}")))
        })?;
        // Find the first AAC audio track and its decoder config.
        let (track_id, sample_count, duration, asc) = {
            let mut found = None;
            for (id, track) in reader.tracks() {
                if !matches!(track.media_type(), Ok(mp4::MediaType::AAC)) {
                    continue;
                }
                let aot = track.audio_profile().map_err(|_| {
                    unsupported(
                        path,
                        "MP4 audio track has no readable AAC decoder config".to_string(),
                    )
                })?;
                let freq_index = track.sample_freq_index().map_err(|_| {
                    unsupported(
                        path,
                        "MP4 audio track has no readable AAC sample rate".to_string(),
                    )
                })?;
                let chan_config = track.channel_config().map_err(|_| {
                    unsupported(
                        path,
                        "MP4 audio track uses a program-config (PCE) AAC channel layout, \
                         which is not supported"
                            .to_string(),
                    )
                })?;
                let aot = aot as u32;
                if aot == 0 || aot >= 31 {
                    return Err(unsupported(
                        path,
                        format!("AAC object type {aot} in MP4 is not supported"),
                    ));
                }
                // Rebuild the two-byte AudioSpecificConfig from the
                // parsed esds fields (enum discriminants are the Raw
                // bitstream values).
                let freq = freq_index as u32;
                let chan = chan_config as u32;
                let asc = [
                    ((aot << 3) | (freq >> 1)) as u8,
                    (((freq & 1) << 7) | (chan << 3)) as u8,
                ];
                found = Some((*id, track.sample_count(), track.duration(), asc));
                break;
            }
            found.ok_or_else(|| {
                unsupported(
                    path,
                    "MP4/M4A file has no AAC audio track Flux can decode \
                     (Apple does not play, and video-only files have no audio)"
                        .to_string(),
                )
            })?
        };
        if sample_count == 0 {
            return Err(malformed(
                path,
                "AAC track in MP4 contains no samples".to_string(),
            ));
        }
        let decoder = rusty_aac::AacDecoder::with_config_bytes(&asc)
            .map_err(|e| unsupported(path, format!("unsupported AAC configuration in MP4: {e}")))?;
        let mut this = Self {
            reader,
            decoder,
            track_id,
            next_sample: 1,
            sample_count,
            pending: VecDeque::new(),
            info: StreamInfo {
                sample_rate: 0,
                channels: 0,
                duration: (duration > Duration::ZERO).then_some(duration),
            },
            path: path.to_path_buf(),
        };
        // Prime with the first sample to fix the real output format.
        if !this.step()? {
            return Err(malformed(
                path,
                "AAC track in MP4 produced no audio".to_string(),
            ));
        }
        Ok(this)
    }

    /// Decodes one MP4 sample (access unit) into `pending`. Returns
    /// false once every sample has been read.
    fn step(&mut self) -> Result<bool, DecodeError> {
        if self.next_sample > self.sample_count {
            return Ok(false);
        }
        let path = self.path.clone();
        let index = self.next_sample;
        self.next_sample += 1;
        let sample = catch_codec_panics(&path, "MP4", || {
            self.reader
                .read_sample(self.track_id, index)
                .map_err(|e| malformed(&path, format!("could not read MP4 sample {index}: {e}")))
        })?
        .ok_or_else(|| {
            malformed(
                &path,
                format!("MP4 sample {index} is missing from the media data"),
            )
        })?;
        let audio = catch_codec_panics(&path, "AAC", || {
            self.decoder
                .decode(&sample.bytes, None)
                .map_err(|e| malformed(&path, format!("AAC decode failed: {e}")))
        })?;
        if self.info.sample_rate == 0 {
            check_params(&path, "AAC", audio.sample_rate, audio.channels)?;
            self.info.sample_rate = audio.sample_rate;
            self.info.channels = audio.channels;
        } else if audio.sample_rate != self.info.sample_rate || audio.channels != self.info.channels
        {
            return Err(malformed(
                &path,
                format!(
                    "AAC stream parameters change mid-stream \
                     ({} Hz/{} ch after {} Hz/{} ch)",
                    audio.sample_rate, audio.channels, self.info.sample_rate, self.info.channels
                ),
            ));
        }
        self.pending.extend(audio.samples);
        Ok(true)
    }
}

impl Decoder for Mp4AacDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn next_chunk(&mut self, max_frames: usize) -> Result<Vec<f32>, DecodeError> {
        let want = max_frames * usize::from(self.info.channels);
        while self.pending.len() < want && self.step()? {}
        let take = want.min(self.pending.len());
        Ok(self.pending.drain(..take).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::{DecodeError, is_playable, open_decoder};

    /// Committed fixtures; see `tests/fixtures/README.md` for how each
    /// was produced. All are short synthetic tones/silence generated
    /// for this project (no third-party audio).
    const MP3_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/silence.mp3");
    const VORBIS_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone.ogg");
    const OPUS_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone.opus");
    const AAC_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone.aac");
    const M4A_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone.m4a");

    /// Minimal unique temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-audio-test-{}-{test}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, bytes).expect("write fixture");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Writes a real WAV file via hound's writer: `frames` frames of a
    /// 440 Hz sine at 0.5 amplitude (same signal on every channel).
    fn write_wav(dir: &TempDir, name: &str, spec: hound::WavSpec, frames: usize) -> PathBuf {
        let path = dir.0.join(name);
        let mut writer = hound::WavWriter::create(&path, spec).expect("create wav");
        for i in 0..frames {
            let t = i as f32 / spec.sample_rate as f32;
            let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            for _ in 0..spec.channels {
                match spec.sample_format {
                    hound::SampleFormat::Float => writer.write_sample(s).expect("write f32"),
                    hound::SampleFormat::Int => {
                        let max = (1i64 << (spec.bits_per_sample - 1)) - 1;
                        writer
                            .write_sample((s * max as f32) as i32)
                            .expect("write int");
                    }
                }
            }
        }
        writer.finalize().expect("finalize wav");
        path
    }

    fn decode_all(path: &std::path::Path) -> (super::StreamInfo, Vec<f32>) {
        let mut decoder = open_decoder(path).expect("decoder opens");
        let info = *decoder.info();
        let mut all = Vec::new();
        loop {
            let chunk = decoder.next_chunk(1024).expect("chunk decodes");
            if chunk.is_empty() {
                break;
            }
            all.extend(chunk);
        }
        (info, all)
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn wav_int16_stereo_decodes_with_expected_shape_and_amplitude() {
        let dir = TempDir::new("wav16");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let path = write_wav(&dir, "tone.wav", spec, 11_025);

        let (info, samples) = decode_all(&path);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.duration.map(|d| d.as_millis()), Some(250));
        assert_eq!(samples.len(), 11_025 * 2);
        assert!(
            (peak(&samples) - 0.5).abs() < 0.02,
            "peak {} should be ≈0.5 after normalization",
            peak(&samples)
        );
    }

    #[test]
    fn wav_float_mono_decodes_unscaled() {
        let dir = TempDir::new("wavf32");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 22_050,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let path = write_wav(&dir, "tone.wav", spec, 2_205);

        let (info, samples) = decode_all(&path);
        assert_eq!(info.channels, 1);
        assert_eq!(info.sample_rate, 22_050);
        assert_eq!(samples.len(), 2_205);
        assert!((peak(&samples) - 0.5).abs() < 0.01);
    }

    /// Minimal structurally valid FLAC: STREAMINFO + empty Vorbis
    /// comment, no audio frames (the same fixture shape the M2 scanner
    /// tests use). Claxon must report the stream params and then EOF.
    fn minimal_flac() -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        let mut streaminfo = Vec::new();
        streaminfo.extend(4096u16.to_be_bytes());
        streaminfo.extend(4096u16.to_be_bytes());
        streaminfo.extend([0, 0, 0]);
        streaminfo.extend([0, 0, 0]);
        let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 132_300u64;
        streaminfo.extend(packed.to_be_bytes());
        streaminfo.extend([0u8; 16]);
        out.push(0x00);
        out.extend([0, 0, 34]);
        out.extend(streaminfo);
        let mut comment = Vec::new();
        comment.extend(0u32.to_le_bytes()); // vendor len
        comment.extend(0u32.to_le_bytes()); // comment count
        out.push(0x84);
        let len = comment.len() as u32;
        out.extend([(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        out.extend(comment);
        out
    }

    #[test]
    fn flac_reports_streaminfo_then_eof() {
        let dir = TempDir::new("flac");
        let path = dir.write("empty.flac", &minimal_flac());
        let mut decoder = open_decoder(&path).expect("flac opens");
        assert_eq!(decoder.info().sample_rate, 44_100);
        assert_eq!(decoder.info().channels, 2);
        assert_eq!(decoder.info().duration, Some(Duration::from_secs(3)));
        let chunk = decoder.next_chunk(1024).expect("eof is not an error");
        assert!(
            chunk.is_empty(),
            "no audio frames in fixture -> immediate EOF"
        );
    }

    #[test]
    fn mp3_fixture_decodes_to_silence_with_correct_shape() {
        let dir = TempDir::new("mp3");
        let path = dir.write("silence.mp3", MP3_FIXTURE);
        let (info, samples) = decode_all(&path);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(
            info.duration, None,
            "MP3 streams report no upfront duration"
        );
        // 20 silent MPEG-1 Layer III frames × 1152 samples/channel.
        assert_eq!(samples.len(), 20 * 1152 * 2);
        assert!(
            samples.iter().all(|s| *s == 0.0),
            "silent MP3 frames must decode to exact silence"
        );
    }

    #[test]
    fn vorbis_fixture_decodes_with_tone() {
        let dir = TempDir::new("Vorbis");
        let path = dir.write("tone.ogg", VORBIS_FIXTURE);
        let (info, samples) = decode_all(&path);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        let frames = samples.len() / 2;
        // 0.5 s were encoded; Vorbis window priming legitimately
        // returns slightly fewer output frames.
        assert!((18_000..=22_050).contains(&frames), "frames {frames}");
        let p = peak(&samples);
        assert!((0.2..0.45).contains(&p), "peak {p} ≈ 0.3 input sine");
    }

    #[test]
    fn opus_fixture_decodes_as_48k_mono_and_ogg_routing_is_by_content() {
        let dir = TempDir::new("opus");
        // Right bytes, "wrong" extensions on purpose: Ogg routing
        // must follow the first packet's magic, not the name.
        for name in ["tone.opus", "tone.ogg", "TONE.OGA"] {
            let path = dir.write(name, OPUS_FIXTURE);
            let (info, samples) = decode_all(&path);
            assert_eq!(info.sample_rate, 48_000, "{name}");
            assert_eq!(info.channels, 1, "{name}");
            assert!(
                (11_000..=12_480).contains(&samples.len()),
                "{name}: {} mono frames (pre_skip may trim encoder delay)",
                samples.len()
            );
            assert!((0.2..0.4).contains(&peak(&samples)), "{name} peak");
        }
        // And the converse: Vorbis bytes under an .opus name.
        let path = dir.write("mislabeled.opus", VORBIS_FIXTURE);
        let (info, _) = decode_all(&path);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
    }

    #[test]
    fn aac_adts_fixture_decodes_with_tone() {
        let dir = TempDir::new("aac");
        let path = dir.write("tone.aac", AAC_FIXTURE);
        let (info, samples) = decode_all(&path);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(samples.len(), 13 * 1024 * 2, "13 AAC frames of 1024");
        let p = peak(&samples);
        assert!((0.2..0.4).contains(&p), "peak {p}");
    }

    #[test]
    fn m4a_fixture_decodes_via_mp4_demux() {
        let dir = TempDir::new("m4a");
        for name in ["tone.m4a", "tone.mp4", "tone.m4b"] {
            let path = dir.write(name, M4A_FIXTURE);
            let (info, samples) = decode_all(&path);
            assert_eq!(info.sample_rate, 44_100, "{name}");
            assert_eq!(info.channels, 2, "{name}");
            let millis = info.duration.expect("MP4 states a duration").as_millis();
            assert!((200..=400).contains(&millis), "{name} duration {millis} ms");
            assert_eq!(samples.len(), 13 * 1024 * 2, "{name}");
            assert!((0.2..0.4).contains(&peak(&samples)), "{name} peak");
        }
    }

    /// Opens `path` and drains it with a hard iteration bound,
    /// asserting the decoder terminates (typed error or empty chunk)
    /// instead of panicking or spinning. Used for hostile inputs.
    fn drain_expect_clean_termination(path: &std::path::Path) {
        let mut decoder = match open_decoder(path) {
            Ok(d) => d,
            Err(_) => return, // typed rejection at open: fine
        };
        for _ in 0..10_000 {
            match decoder.next_chunk(1024) {
                Ok(chunk) if chunk.is_empty() => return,
                Ok(_) => {}
                Err(_) => return, // typed mid-stream error: fine
            }
        }
        panic!("decoder for {} did not terminate", path.display());
    }

    #[test]
    fn hostile_inputs_terminate_cleanly_in_every_backend() {
        let dir = TempDir::new("hostile");
        // Deterministic pseudo-random garbage (LCG), and truncations
        // / corruptions of the real fixtures.
        let mut state = 0x243F6A88u32;
        let garbage: Vec<u8> = (0..4096)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let fixtures: [(&[u8], &str); 5] = [
            (MP3_FIXTURE, "mp3"),
            (VORBIS_FIXTURE, "ogg"),
            (OPUS_FIXTURE, "opus"),
            (AAC_FIXTURE, "aac"),
            (M4A_FIXTURE, "m4a"),
        ];
        for (bytes, ext) in fixtures {
            for (label, variant) in [
                ("empty", Vec::new()),
                ("truncated", bytes[..bytes.len() / 3].to_vec()),
                ("garbage", garbage.clone()),
                ("mutated", {
                    let mut m = bytes.to_vec();
                    for i in 0..300 {
                        let idx = (i * 37) % m.len();
                        m[idx] ^= 0x5A;
                    }
                    m
                }),
            ] {
                let path = dir.write(&format!("{label}.{ext}"), &variant);
                drain_expect_clean_termination(&path);
            }
        }
        // Garbage under WAV/FLAC extensions as well.
        for ext in ["wav", "flac"] {
            let path = dir.write(&format!("garbage.{ext}"), &garbage);
            drain_expect_clean_termination(&path);
        }
    }

    #[test]
    fn mismatched_contents_and_error_typing() {
        let dir = TempDir::new("errors");

        // Indexed but undecodable extension: never opened.
        let aif = dir.write("song.aif", b"whatever");
        assert!(matches!(
            open_decoder(&aif).map(|_| ()),
            Err(DecodeError::UnsupportedFormat { .. })
        ));
        let no_ext = dir.write("noextension", b"whatever");
        assert!(matches!(
            open_decoder(&no_ext).map(|_| ()),
            Err(DecodeError::UnsupportedFormat { .. })
        ));

        // Supported extensions with wrong/garbage bytes -> Malformed.
        for (name, bytes) in [
            ("bad.wav", &b"not a wav file at all"[..]),
            ("bad.flac", &b"not flac data, no marker here"[..]),
            ("bad.mp3", &b"not an mp3 stream at all"[..]),
            ("bad.aac", &b"not an adts stream at all"[..]),
            ("bad.ogg", &b"not an ogg stream at all"[..]),
            ("bad.m4a", &b"not an mp4 file at all, nope"[..]),
        ] {
            let path = dir.write(name, bytes);
            match open_decoder(&path) {
                Err(DecodeError::Malformed { .. }) => {}
                Err(other) => panic!("{name}: expected Malformed, got {other:?}"),
                Ok(_) => panic!("{name}: garbage must not open"),
            }
        }

        // Cross-format content swaps -> errors, never a wrong decode.
        let swaps: [(&str, &[u8]); 4] = [
            ("wav-bytes.mp3", MP3_FIXTURE),
            ("ogg-bytes.m4a", VORBIS_FIXTURE),
            ("m4a-bytes.ogg", M4A_FIXTURE),
            ("mp3-bytes.aac", MP3_FIXTURE),
        ];
        for (name, bytes) in swaps {
            // For "wav-bytes.mp3" write actual WAV bytes instead.
            let path = if name == "wav-bytes.mp3" {
                write_wav(
                    &dir,
                    name,
                    hound::WavSpec {
                        channels: 1,
                        sample_rate: 8_000,
                        bits_per_sample: 16,
                        sample_format: hound::SampleFormat::Int,
                    },
                    100,
                )
            } else {
                dir.write(name, bytes)
            };
            match open_decoder(&path) {
                Err(DecodeError::Malformed { .. }) => {}
                Err(other) => panic!("{name}: expected Malformed, got {other:?}"),
                Ok(_) => panic!("{name}: mismatched content must not open"),
            }
        }

        // Missing file with a playable extension -> Io (NotFound).
        for name in ["missing.flac", "missing.mp3", "missing.m4a"] {
            match open_decoder(&dir.0.join(name)) {
                Err(DecodeError::Io(err)) => {
                    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{name}")
                }
                Err(other) => panic!("{name}: expected Io error, got {other:?}"),
                Ok(_) => panic!("{name}: missing file must not open"),
            }
        }
    }

    #[test]
    fn playable_check_is_extension_based_and_case_insensitive() {
        use std::path::Path;
        for yes in [
            "a.wav", "a.FLAC", "a.mp3", "a.aac", "a.ogg", "a.oga", "a.opus", "a.m4a", "a.m4b",
            "a.mp4",
        ] {
            assert!(is_playable(Path::new(yes)), "{yes}");
        }
        for no in ["a.aif", "a.ape", "a.wma", "a.m4p", "a.spx", "noext"] {
            assert!(!is_playable(Path::new(no)), "{no}");
        }
    }
}
