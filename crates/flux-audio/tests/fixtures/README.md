# Audio test fixtures

Small, deterministic audio files used by the `flux-audio` decoder and
engine tests and by one `flux-app` controller test. Every file here
contains **original synthetic audio created for this project** (a
440 Hz sine tone or digital silence, generated numerically) — no
third-party recordings, samples, or downloaded media. They are covered
by the repository's MIT license like the rest of the tree.

| File | Container / codec | Content | Size | SHA-256 |
|---|---|---|---|---|
| `silence.mp3` | MP3 (MPEG-1 Layer III, 44.1 kHz stereo) after a minimal ID3v2 tag | 20 silent frames ≈ 0.52 s digital silence | 8 376 B | `aee8e1a769755ce13ad05b4ffcc3b97ff147fc13503ba6c9bb527f2d2bfece2e` |
| `tone.ogg` | Ogg Vorbis (44.1 kHz stereo) | 0.5 s 440 Hz sine, amplitude 0.3 | 5 716 B | `1d9748c9496b3bb67a59b8a700d1d80b7bc2e51477d4bd22334ac62eb9f3bfcd` |
| `tone.opus` | Ogg Opus (48 kHz mono) | ≈0.26 s 440 Hz sine, amplitude 0.3 | 2 513 B | `56209b147c86af3456596c0c782079de1e2c4934afb15a78e00edf442a0bc931` |
| `tone.aac` | ADTS AAC-LC (44.1 kHz stereo) | 13 frames ≈ 0.30 s 440 Hz sine, amplitude 0.3 | 1 837 B | `7793a13a8a4ca7fe794b45298329872cb45a613082b514b104ec152fb22dd0cf` |
| `tone.m4a` | MP4 (`M4A ` brand) with one AAC-LC track | Same 13 AAC frames as `tone.aac`, raw access units in `mdat` | 2 385 B | `51deac24f0754332f7040f8b10c86b43a22b277ca5192d6e624e7e1f77492956` |

## How they were made

The sandbox used for development had no system media encoders
(no ffmpeg/SoX/LAME/…), so the compressed fixtures were produced by a
**one-off generator program outside this workspace** (its crate
dependencies are deliberately *not* workspace dependencies; tests only
depend on the committed bytes and their decoded properties, never on
an encoder):

- **`silence.mp3`** — constructed byte-for-byte in a short script: a
  minimal ID3v2.3 header with one `TIT2` frame ("flux m5 fixture"),
  followed by 20 MPEG-1 Layer III frames of 417 bytes each
  (128 kbit/s, 44.1 kHz, stereo). Every frame is the sync/header
  `FF FB 90 00` followed by zero side information and zero main data,
  which is a spec-valid encoding of 1152 silent samples per channel.
- **`tone.ogg`** — sine PCM encoded with the pure-Rust encoder crate
  `rusty_vorbis` 0.1.1 (Apache-2.0), muxed into Ogg pages with the
  `ogg` 0.9.2 writer (identification header alone on the first page,
  headers flushed before audio, per the Vorbis I spec). It is decoded
  in tests by `lewton`, an independent implementation.
- **`tone.opus`** — sine PCM encoded with `opus-pure` 0.2.2's own
  Opus encoder and Ogg writer (BSD-3-Clause). The test decodes it with
  the same crate's decoder (through this workspace's independent Ogg
  demuxing), so this fixture proves end-to-end integration, not
  cross-implementation compatibility.
- **`tone.aac`** — sine PCM (as i16) encoded with `oxideav-aac`
  0.1.7's `StreamEncoder` (MIT) at a 96 kbit/s target, emitting
  complete ADTS frames. It is decoded in tests by `rusty_aac`, an
  independent implementation.
- **`tone.m4a`** — the ADTS headers of `tone.aac`'s frames were
  stripped and the raw AAC access units were wrapped in a minimal,
  hand-written MP4 box tree (`ftyp`, `mdat`, `moov` with
  `mvhd`/`trak`/`mdia`/`minf`/`stbl`, an `mp4a` sample entry with an
  `esds` whose `AudioSpecificConfig` is AAC-LC/44.1 kHz/stereo =
  `12 10`, one chunk). It is demuxed in tests by the `mp4` crate and
  decoded by `rusty_aac`.

## Regenerating

Committed bytes are the reference; tests assert decoded properties
(sample rate, channels, frame counts within codec-accurate bounds,
peak amplitude) rather than byte equality, so byte-identical
regeneration is not required. If a fixture ever needs replacement,
rebuild an equivalent with any trusted encoder (e.g. ffmpeg:
`-f lavfi -i sine=frequency=440:duration=0.5`), keep it tiny and
synthetic, and update the table above (sizes, hashes, encoder used).
