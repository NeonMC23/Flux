//! Audio decoding: turns local audio files into interleaved `f32`
//! samples for the playback engine.
//!
//! Format support at this milestone is deliberately small and explicit:
//!
//! | Extension | Decoder | License |
//! |---|---|---|
//! | `.wav` | `hound` | Apache-2.0 |
//! | `.flac` | `claxon` | Apache-2.0 |
//!
//! Everything else — including formats the library scanner happily
//! *indexes* (MP3, Ogg, Opus, M4A, …) — returns
//! [`DecodeError::UnsupportedFormat`]. Indexed is not the same as
//! playable, and this module never pretends otherwise. Dispatch is by
//! file extension (case-insensitive); a file whose extension promises
//! WAV/FLAC but whose bytes disagree yields
//! [`DecodeError::Malformed`] from the respective parser.
//!
//! Decoders are pull-based chunks of interleaved `f32` in the file's own
//! sample rate and channel layout; rate/channel conversion to the output
//! device's format happens in the engine, not here.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// File extensions this milestone can actually decode (compared ASCII
/// case-insensitively).
pub const PLAYABLE_EXTENSIONS: &[&str] = &["wav", "flac"];

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
    /// The file's extension is not a format this milestone decodes.
    /// The file is never opened in this case.
    UnsupportedFormat {
        /// The path that was rejected.
        path: PathBuf,
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
            Self::UnsupportedFormat { path } => {
                write!(f, "unsupported audio format: {}", path.display())
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
/// (case-insensitive): `.wav` → hound, `.flac` → claxon.
///
/// # Errors
///
/// [`DecodeError::UnsupportedFormat`] for any other (or missing)
/// extension — the file is not opened in that case;
/// [`DecodeError::Io`] if the file cannot be read;
/// [`DecodeError::Malformed`] if it does not parse as its extension
/// claims.
pub fn open_decoder(path: &Path) -> Result<Box<dyn Decoder>, DecodeError> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("wav") {
        Ok(Box::new(WavDecoder::open(path)?))
    } else if extension.eq_ignore_ascii_case("flac") {
        Ok(Box::new(FlacDecoder::open(path)?))
    } else {
        Err(DecodeError::UnsupportedFormat {
            path: path.to_path_buf(),
        })
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::{DecodeError, is_playable, open_decoder};

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
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - 0.5).abs() < 0.02,
            "peak {peak} should be ≈0.5 after normalization"
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
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01);
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
    fn unsupported_malformed_and_missing_files_are_typed_errors() {
        let dir = TempDir::new("errors");

        // Unsupported by extension: never opened, even if it exists.
        let mp3 = dir.write("song.mp3", b"whatever");
        assert!(matches!(
            open_decoder(&mp3).map(|_| ()),
            Err(DecodeError::UnsupportedFormat { .. })
        ));
        let no_ext = dir.write("noextension", b"whatever");
        assert!(matches!(
            open_decoder(&no_ext).map(|_| ()),
            Err(DecodeError::UnsupportedFormat { .. })
        ));

        // Supported extension, garbage contents -> Malformed.
        let bad_wav = dir.write("bad.wav", b"not a wav file at all");
        assert!(matches!(
            open_decoder(&bad_wav).map(|_| ()),
            Err(DecodeError::Malformed { .. })
        ));
        let bad_flac = dir.write("bad.flac", b"not flac data, no marker here");
        match open_decoder(&bad_flac) {
            Err(DecodeError::Malformed { .. }) => {}
            Err(other) => panic!("expected Malformed for marker-less FLAC, got {other:?}"),
            Ok(_) => panic!("marker-less FLAC must not open"),
        }
        // Truncated right after the marker: still a typed error (the
        // underlying decoder may report it as I/O EOF or as malformed).
        let truncated = dir.write("truncated.flac", b"fLaC but then nonsense");
        assert!(open_decoder(&truncated).map(|_| ()).is_err());

        // Missing file with a playable extension -> Io (NotFound).
        let missing = dir.0.join("missing.flac");
        match open_decoder(&missing) {
            Err(DecodeError::Io(err)) => assert_eq!(err.kind(), std::io::ErrorKind::NotFound),
            Err(other) => panic!("expected Io error, got {other:?}"),
            Ok(_) => panic!("missing file must not open"),
        }
    }

    #[test]
    fn playable_check_is_extension_based_and_case_insensitive() {
        use std::path::Path;
        assert!(is_playable(Path::new("a.wav")));
        assert!(is_playable(Path::new("a.FLAC")));
        assert!(!is_playable(Path::new("a.mp3")));
        assert!(!is_playable(Path::new("a.ogg")));
        assert!(!is_playable(Path::new("noext")));
    }
}
