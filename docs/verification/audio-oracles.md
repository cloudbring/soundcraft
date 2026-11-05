# Audio oracles

> **Last reviewed:** 2026-11-04 · **Last updated:** 2026-11-04 · **Change:** new (ticket cloudbring/artcraft-algorithms#201) · **Oracles:** ffmpeg 9.0.2, `craft-compare` 3b868c0

soundcraft's meters used to be checked only against themselves and its decoder only against
whatever symphonia returned. Two oracle suites now compare both with independent references.
ffmpeg is an external process (never linked); `craft-compare` is a dev-dependency.

| Suite | File | Compares |
|---|---|---|
| Loudness | `crates/dsp/tests/it/loudness_oracle.rs` | `LoudnessMeter` against `craft-compare::loudness` (f64, integrated) and ffmpeg `ebur128=peak=true:metadata=1` (M, S, I, LRA, true peak) |
| Loudness, whole chain | `crates/engine/tests/it/loudness_oracle.rs` | the demo session bounced to float WAV: the loudness the engine reports vs ffmpeg and `craft-compare` on the written file |
| Decode | `crates/audio-io/tests/it/decode_oracle.rs` | `soundcraft_audio_io::decode` vs `ffmpeg -f f32le -ac N -ar R`, sample-aligned, with `craft_compare::audio::{snr_db, max_abs_error}` |
| Support | `crates/audio-io/tests/support/oracle.rs` | ffmpeg discovery and the require switch (included by path into the three test binaries) |

## Running

```
cargo test -p soundcraft-audio-io -p soundcraft-dsp -p soundcraft-engine
SOUNDCRAFT_REQUIRE_ORACLES=1 cargo test ...     # CI: a missing ffmpeg fails
```

- ffmpeg is looked up in `SOUNDCRAFT_FFMPEG` (explicit path; a path that is not a file is an
  error; an empty value declares "no ffmpeg"), then `PATH`, then `/opt/homebrew/bin`,
  `/usr/local/bin`, `/usr/bin`.
- Without ffmpeg an oracle test prints `SKIPPED (<test>): ffmpeg not found ...` and passes.
  With `SOUNDCRAFT_REQUIRE_ORACLES=1` (also `true`, `yes`, `on`) it fails with
  `... forbids skipping oracle tests`. `.github/workflows/ci.yml` installs ffmpeg and sets the
  switch, so CI cannot silently skip.
- Check both modes: `SOUNDCRAFT_FFMPEG= cargo test -p soundcraft-dsp --test it loudness_oracle`
  (skips) and the same with `SOUNDCRAFT_REQUIRE_ORACLES=1` (fails).
- `craft-compare` is pinned at rev `3b868c05` of `cloudbring/craft-libs` (the fork) because the
  compare crate is not yet on `storytold/craft-libs` main. Repoint the workspace dependency
  when it lands.

## Loudness

Tolerances: M, S, I within 0.1 LU of ffmpeg (EBU Tech 3341), I within 0.02 LU of `craft-compare`
(same algorithm, f32 against f64 filters), true peak within 0.2 dB (Tech 3341 allows +0.2 / -0.4),
loudness range within 1 LU of the standard (Tech 3342). Where the standard gives a value, both
soundcraft and the oracle are held to it, so a wrong oracle cannot hide a wrong meter.
M is compared every 100 ms once 400 ms have been seen, S once 3 s have been seen, wherever
ffmpeg reads above -70 LUFS.

Integrated loudness (LUFS) and true peak (dBTP), 48 kHz unless noted:

| Signal | Standard | soundcraft | craft-compare | ffmpeg | TP soundcraft | TP ffmpeg | TP analytic |
|---|---|---|---|---|---|---|---|
| Tech 3341-1 stereo 1 kHz -23 | -23.0 | -22.993 | -22.993 | -23.000 | -23.000 | -22.975 | |
| Tech 3341-2 stereo 1 kHz -33 | -33.0 | -32.993 | -32.993 | -33.000 | -33.000 | -33.152 | |
| Tech 3341-3 -36/-23/-36 (relative gate) | -23.0 | -23.014 | -23.014 | -23.021 | -23.000 | -22.975 | |
| Tech 3341-4 -72/-36/-23/-36/-72 (absolute gate) | -23.0 | -23.014 | -23.014 | -23.021 | -23.000 | -22.975 | |
| Tech 3341-5 -26/-20/-26 | -23.0 | -22.979 | -22.979 | -22.985 | -20.000 | -20.000 | |
| Tech 3341-6 5.1, LFE loaded | -23.0 | -23.016 | -23.016 | -23.020 | -6.000 | -6.003 | |
| Tech 3341-6 as 5.0, default weights | -23.0 | -23.016 | -23.016 | -23.020 | -24.000 | -24.013 | |
| 7.1 with explicit weights, LFE loaded | | -23.013 | -23.013 | -23.020 | -6.000 | -6.003 | |
| 1 kHz sine -6 dBFS | | -5.993 | -5.993 | -6.000 | -6.000 | -6.003 | -6.000 |
| fs/4 at 45 deg, 0.9 (inter-sample peak) | | 2.437 | 2.437 | 2.430 | -0.993 | -0.318 | -0.915 |
| pink noise stereo 15 s | | -17.610 | -17.610 | -17.614 | -6.904 | -6.897 | |
| speech-like stereo 25 s | | -24.970 | -24.970 | -24.975 | -7.804 | -7.787 | |
| gated stereo (silence, -50, -80 dBFS) | | -21.540 | -21.540 | -21.547 | -18.000 | -17.993 | |
| speech-like mono 44.1 kHz | | -34.033 | -34.033 | -34.038 | -10.806 | -10.812 | |
| pink noise stereo 96 kHz | | -18.883 | -18.883 | -18.888 | -8.936 | -9.020 | |
| demo session bounce, 30 s (engine) | | -19.422 | -19.422 | -19.427 | -4.780 | -4.776 | |

Worst differences over all signals: |dM| 0.0026 LU, |dS| 0.0030 LU, |dI| against ffmpeg 0.0071 LU,
against `craft-compare` 0.0005 LU.

Notes:
- soundcraft and `craft-compare` read 0.007 LU above the nominal -23.000 on the 1 kHz cases where ffmpeg reads exactly -23.000; both are far inside 0.1 LU and agree with each other to 0.0005 LU.
- True peak of the fs/4 tone: soundcraft's 4x, 12-tap windowed-sinc estimate reads 0.078 dB under
  the analytic value (inside +0.2 / -0.4); ffmpeg reads 0.6 dB over.

Loudness range (EBU Tech 3342 cases 1 to 4, ffmpeg only; soundcraft has no LRA meter, see
"Open findings"):

| Case | Standard (LU) | ffmpeg (LU) |
|---|---|---|
| 1 -20/-30 LUFS | 10 | 10.00 |
| 2 -20/-15 LUFS | 5 | 5.00 |
| 3 -40/-20 LUFS | 20 | 20.00 |
| 4 -50/-35/-20/-35/-50 LUFS | 15 | 15.00 |

## Decode

Each file is decoded by `soundcraft_audio_io::decode` and by `ffmpeg -i f -vn -f f32le -ac N -ar R -`
(N and R from soundcraft's `AudioInfo`, so no remix or resample). Alignment is explicit: the lag
in -3000..=3000 frames that maximises SNR is searched (needing at least half the shorter signal
to overlap), reported, and asserted to be 0. Frame counts are compared too, and where the source
length is known the gapless decode must restore it.

Thresholds (SNR over the common samples, max absolute sample error):

| Codec | Threshold | Why |
|---|---|---|
| PCM in CAF / MKV / AIFF-C, FLAC, ALAC, all-zero MPEG-1 Layer I | bit-exact | integer-to-float conversion is exact |
| MP3, Vorbis | SNR >= 100 dB, max err <= 5e-6 | float decoders with different rounding |
| AAC (m4a, ADTS) | SNR >= 100 dB, max err <= 2e-5 | same; observed 7.9e-6 on a 2 s music-like stereo stream (filmcraft's docs/testing.md quotes ~1e-7 for its own AAC decoder against ffmpeg, a tighter bound than this decoder meets) |
| MPEG-1 Layer II | SNR >= 70 dB, max err <= 4e-5 | ffmpeg's `mp2` decoder rounds to 16 bits (1 LSB = 3.05e-5) |

Fixtures in `crates/audio-io/tests/fixtures` (0.1 s mono 440 Hz tone unless noted):

| File | Channels / rate | soundcraft frames | ffmpeg frames | Lag | SNR (dB) | Max error |
|---|---|---|---|---|---|---|
| tone.mp3 | 1 / 8000 | 800 | 800 | 0 | 132.66 | 7.5e-8 |
| tone.mp2 | 1 / 24000 | 3456 | 3456 | 0 | 75.83 | 3.1e-5 |
| silence.mp1 | 1 / 32000 | 1536 | 1536 | 0 | exact | 0 |
| tone.ogg | 1 / 8000 | 800 | 1024 | 0 | 139.24 | 3.7e-8 |
| tone.webm | 1 / 8000 | 1024 | 1024 | 0 | 139.16 | 3.7e-8 |
| tone.aac (ADTS) | 1 / 8000 | 2048 | 2048 | 0 | 135.60 | 3.7e-8 |
| tone.m4a | 1 / 8000 | 800 | 800 | 0 (was +1024) | 136.62 | 3.7e-8 |
| tone.caf | 1 / 8000 | 800 | 800 | 0 | exact | 0 |
| tone.mkv | 1 / 8000 | 800 | 800 | 0 | exact | 0 |
| tone.aifc | 1 / 8000 | 800 | 800 | 0 | exact | 0 |

Longer streams encoded by ffmpeg at test time (2 s of stereo 44.1 kHz music-like signal, so there
is real encoder delay and padding):

| Stream | soundcraft frames | ffmpeg frames | Lag | SNR (dB) | Max error |
|---|---|---|---|---|---|
| libmp3lame 192k (.mp3) | 88200 | 88200 | 0 | 125.34 | 9.2e-7 |
| aac 128k (.m4a) | 88200 | 88200 | 0 (was +1024, 90112 frames) | 112.44 | 7.9e-6 |
| aac 128k (ADTS) | 90112 | 90112 | 0 | 112.44 | 7.9e-6 |
| alac (.m4a) | 88200 | 88200 | 0 | exact | 0 |
| mp2 192k | 88704 | 88704 | 0 | 84.01 | 3.2e-5 |
| flac | 88200 | 88200 | 0 | exact | 0 |

ffmpeg here has no Vorbis encoder, so Ogg is covered by the checked-in fixtures only.

Length notes: ffmpeg's Ogg demuxer returns the last 1024-frame block untrimmed (1024 frames) while
soundcraft honours the granule position and returns the true 800; the first 800 frames agree to
139 dB. Raw ADTS and Matroska/WebM Vorbis carry no length or delay information, so both decoders
return every decoded frame (the ADTS count includes the AAC priming).

## Disagreements found and fixed

1. **MP4/M4A edit list ignored.** symphonia's MP4 reader parses the `elst` atom but does not
   apply it, so an imported `.m4a` started 1024 frames (AAC priming) late and ran 1248 frames long
   (2048 against 800 frames for the fixture; 90112 against 88200 for the 2 s stream). The oracle
   reported lag +1024. Fix: `crates/audio-io/src/mp4.rs::edit_range` reads the first sound
   track's edit list (a single plain edit, which AAC encoders write; an empty leading edit is
   skipped) and `symph.rs` trims decode and probe to it. Regression tests:
   `tests/it/m4a_edit_list.rs` (no ffmpeg needed), the oracle's `lag == 0` and length checks, and
   unit tests for the atom parser (including truncated input). Multi-edit lists are left as
   decoded.
2. **5-channel loudness weights.** `LoudnessMeter::new(sr, 5)` weighted the surrounds 1.0 (only
   6 channels knew about 1.41), so Tech 3341 case 6 as 5.0 read -23.389 LUFS against -23.017
   (momentary 0.37 LU off, over the 0.1 LU limit). Fix: 5 channels default to L R C Ls Rs with 1.41
   surrounds (`crates/dsp/src/meter.rs`). Test: `five_zero_uses_the_standard_surround_weights_by_default`.
3. **7.1 (and every layout with an LFE or surrounds beyond 5.0 / 5.1) loudness.** The meter is
   built from a channel count, so the bounce's normalise-to-LUFS and reported `lufs` counted the
   LFE and left the surrounds unweighted: a 7.1 mix with a -6 dBFS LFE read -8.85 LUFS against
   -23.01 (14 LU too loud, so a normalised bounce came out 14 LU too quiet). Fix:
   `crates/engine/src/io.rs::loudness_meter` sets the weights from the `ChannelFormat`'s speakers
   (LFE 0; Ls, Rs, Lss, Rss, Lrs, Rrs, Cs 1.41). Test: `seven_one_leaves_out_the_lfe_and_weights_the_surrounds`.
   The oracle checks 7.1 with explicit weights against `craft-compare` and ffmpeg.

## Open findings

- **No loudness-range meter.** `LoudnessMeter` has momentary, short-term, max-momentary,
  integrated and true peak, but no EBU Tech 3342 LRA, so the ticket's LRA comparison could only
  validate the ffmpeg reference against the standard (table above). Adding LRA is a feature, not a
  fix; once it exists, compare it against `Frame::lra` in `loudness_oracle.rs`.
- **`LoudnessMeter::new(sr, n)` still only knows 5 and 6 channels.** Callers other than the
  engine's bounce (none today) must call `set_channel_weight` per speaker.
- **True peak is slightly low on inter-sample peaks** (-0.078 dB on the fs/4 tone), inside the
  +0.2 / -0.4 dB Tech 3341 window. A longer interpolation filter would close it; not changed.
- **ADTS and WebM/Vorbis are not trimmed** (neither is ffmpeg): no length or delay information in
  the file, so the AAC priming (1024 frames) stays in an ADTS decode.
- **MP4 edit lists with several edits or a non-zero leading empty edit** are not applied.
- `craft-compare` comes from the `cloudbring/craft-libs` fork until its compare crate is on
  `storytold/craft-libs` main.
