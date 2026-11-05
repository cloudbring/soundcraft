//! Decode oracle: every compressed-format fixture decodes with soundcraft and with ffmpeg
//! (`-f f32le -ac N -ar R`), and the sample-aligned PCM is compared with `craft-compare` (SNR and
//! max absolute error). See `docs/verification/audio-oracles.md` for the tolerance table.
//!
//! Alignment is explicit: ffmpeg trims encoder delay and padding (LAME/Xing gapless info, MP4 edit
//! lists, Vorbis granule positions) and symphonia may not, so a lag in `-MAX_LAG..=MAX_LAG` is
//! searched, reported and then asserted against the codec's expected value.

use crate::oracle::{self, f32le_to_vec};
use craft_compare::audio::{max_abs_error, snr_db};
use soundcraft_audio_io::{AudioBuffer, decode};
use std::path::Path;

const MAX_LAG: isize = 3000;

/// Per-fixture expectation. After alignment soundcraft must agree with ffmpeg at `lag` 0.
struct Case {
    file: &'static str,
    /// Minimum SNR (dB) of soundcraft against ffmpeg over the common samples; `None`: bit-exact.
    min_snr_db: Option<f64>,
    /// Maximum absolute sample error over the common samples.
    max_abs: f64,
    /// The source length the encoder was given, when gapless trimming must restore it.
    true_frames: Option<usize>,
    /// Frames ffmpeg returns beyond soundcraft (an ffmpeg demuxer that does not trim).
    ffmpeg_extra: usize,
}

/// Thresholds (also in `docs/verification/audio-oracles.md`):
/// - PCM containers (CAF, MKV, AIFF-C) and the all-zero MPEG-1 Layer I: bit-exact;
/// - float decoders: SNR >= 100 dB; max |err| <= 5e-6 for MP3 and Vorbis (observed 9.2e-7) and
///   <= 2e-5 for AAC (observed 7.9e-6 on a 2 s music-like stereo stream, 112 dB);
/// - MPEG-1 Layer II: ffmpeg's `mp2` decoder rounds to 16 bits, so max |err| <= 1.3 LSB16 = 4e-5
///   and SNR >= 70 dB (observed 75.8 dB, 3.1e-5).
const CASES: &[Case] = &[
    Case { file: "tone.mp3", min_snr_db: Some(100.0), max_abs: 5e-6, true_frames: Some(800), ffmpeg_extra: 0 },
    Case { file: "tone.mp2", min_snr_db: Some(70.0), max_abs: 4e-5, true_frames: None, ffmpeg_extra: 0 },
    Case { file: "silence.mp1", min_snr_db: None, max_abs: 0.0, true_frames: None, ffmpeg_extra: 0 },
    // Ogg: the granule position trims the last page to the true 800 frames, which ffmpeg's ogg
    // demuxer returns padded to the 1024-frame block.
    Case { file: "tone.ogg", min_snr_db: Some(100.0), max_abs: 5e-6, true_frames: Some(800), ffmpeg_extra: 224 },
    // WebM/Matroska carries no sample count for Vorbis: both decoders return all 1024 frames.
    Case { file: "tone.webm", min_snr_db: Some(100.0), max_abs: 5e-6, true_frames: None, ffmpeg_extra: 0 },
    // Raw ADTS has no delay or length information: both decode every frame (2 x 1024).
    Case { file: "tone.aac", min_snr_db: Some(100.0), max_abs: 2e-5, true_frames: None, ffmpeg_extra: 0 },
    // MP4: the edit list trims 1024 priming frames and the padding to the true 800.
    Case { file: "tone.m4a", min_snr_db: Some(100.0), max_abs: 2e-5, true_frames: Some(800), ffmpeg_extra: 0 },
    Case { file: "tone.caf", min_snr_db: None, max_abs: 0.0, true_frames: Some(800), ffmpeg_extra: 0 },
    Case { file: "tone.mkv", min_snr_db: None, max_abs: 0.0, true_frames: Some(800), ffmpeg_extra: 0 },
    Case { file: "tone.aifc", min_snr_db: None, max_abs: 0.0, true_frames: Some(800), ffmpeg_extra: 0 },
];

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Result of aligning ours against ffmpeg's.
#[derive(Debug)]
struct Aligned {
    lag: isize,
    overlap: usize,
    snr: f64,
    max_abs: f64,
}

/// Compare `ours` with `reference` (both interleaved) for a lag `k`: `ours[i + k]` against
/// `reference[i]`, over the samples both have.
fn at_lag(reference: &[f32], ours: &[f32], k: isize) -> (usize, &'static str, Option<(f64, f64)>) {
    let (r0, o0) = if k >= 0 { (0usize, k as usize) } else { ((-k) as usize, 0usize) };
    let n = reference.len().saturating_sub(r0).min(ours.len().saturating_sub(o0));
    if n == 0 {
        return (0, "", None);
    }
    let r = &reference[r0..r0 + n];
    let o = &ours[o0..o0 + n];
    (n, "", Some((snr_db(r, o).unwrap_or(f64::NAN), max_abs_error(r, o).unwrap_or(f64::NAN))))
}

fn align(reference: &[f32], ours: &[f32], channels: usize) -> Aligned {
    // Exact-length, zero-lag first: the common case.
    let mut best: Option<Aligned> = None;
    let silent = reference.iter().all(|v| *v == 0.0) && ours.iter().all(|v| *v == 0.0);
    let span = if silent { 0 } else { MAX_LAG };
    for k in -span..=span {
        // Lags are in frames; interleaved offsets are k * channels.
        let (n, _, m) = at_lag(reference, ours, k * channels as isize);
        let Some((snr, max_abs)) = m else { continue };
        // Require at least half of the shorter signal to overlap, so a huge lag cannot win on a sliver.
        if n * 2 < reference.len().min(ours.len()) {
            continue;
        }
        let better = best.as_ref().is_none_or(|b| snr > b.snr || (snr == b.snr && k.abs() < b.lag.abs()));
        if better {
            best = Some(Aligned { lag: k, overlap: n / channels, snr, max_abs });
        }
    }
    best.expect("no lag with enough overlap")
}

fn ffmpeg_decode(ff: &Path, file: &Path, channels: u16, rate: u32) -> Vec<f32> {
    let out = oracle::run(
        ff,
        &[
            "-i".as_ref(),
            file.as_os_str(),
            "-vn".as_ref(),
            "-f".as_ref(),
            "f32le".as_ref(),
            "-ac".as_ref(),
            channels.to_string().as_ref(),
            "-ar".as_ref(),
            rate.to_string().as_ref(),
            "-".as_ref(),
        ],
        None,
    );
    f32le_to_vec(&out)
}

fn report(name: &str, ours: &AudioBuffer, ff_frames: usize, a: &Aligned) {
    eprintln!(
        "{name}: {} ch @ {} Hz, ours {} frames, ffmpeg {ff_frames} frames, lag {:+}, overlap {}, SNR {:.2} dB, max |err| {:.3e}",
        ours.num_channels(),
        ours.sample_rate,
        ours.frames(),
        a.lag,
        a.overlap,
        a.snr,
        a.max_abs
    );
}

fn check(name: &str, ff: &Path, path: &Path, case: &Case) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let ext = path.extension().and_then(|e| e.to_str());
    let (info, buf) = decode(&bytes, ext).unwrap_or_else(|e| panic!("{name}: {e}"));
    let reference = ffmpeg_decode(ff, path, info.channels, info.sample_rate);
    let ours = buf.interleaved();
    let ch = buf.num_channels();
    let a = align(&reference, &ours, ch);
    let ff_frames = reference.len() / ch;
    report(name, &buf, ff_frames, &a);
    assert_eq!(a.lag, 0, "{name}: soundcraft's output is offset from ffmpeg's by {} frames (encoder delay not trimmed?)", a.lag);
    assert_eq!(ff_frames, buf.frames() + case.ffmpeg_extra, "{name}: ffmpeg returned {ff_frames} frames, soundcraft {}", buf.frames());
    if let Some(n) = case.true_frames {
        assert_eq!(buf.frames(), n, "{name}: gapless decode must restore the encoder's input length");
    }
    match case.min_snr_db {
        None => assert!(a.max_abs == 0.0, "{name}: expected bit-exact, max |err| {:.3e}", a.max_abs),
        Some(min) => assert!(a.snr >= min, "{name}: SNR {:.2} dB < {min} dB", a.snr),
    }
    assert!(a.max_abs <= case.max_abs, "{name}: max |err| {:.3e} > {:.1e}", a.max_abs, case.max_abs);
}

#[test]
fn fixtures_decode_like_ffmpeg() {
    let Some(ff) = oracle::ffmpeg("fixtures_decode_like_ffmpeg") else { return };
    for case in CASES {
        check(case.file, &ff, &fixture_path(case.file), case);
    }
}

/// Deterministic stereo program: two detuned tones with a slow envelope plus noise.
fn program(rate: u32, secs: f64) -> Vec<f32> {
    let n = (f64::from(rate) * secs) as usize;
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    (0..n)
        .flat_map(|i| {
            let t = i as f64 / f64::from(rate);
            let env = 0.5 + 0.5 * (std::f64::consts::TAU * 1.5 * t).sin();
            let mut noise = || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
            };
            let l = 0.4 * env * (std::f64::consts::TAU * 440.0 * t).sin() + 0.05 * noise();
            let r = 0.4 * (1.0 - 0.5 * env) * (std::f64::consts::TAU * 660.0 * t).sin() + 0.05 * noise();
            [l as f32, r as f32]
        })
        .collect()
}

/// Longer stereo 44.1 kHz streams encoded by ffmpeg at test time: the same checks as the fixtures,
/// on files with real encoder delay and padding.
#[test]
fn ffmpeg_encoded_streams_decode_like_ffmpeg() {
    let Some(ff) = oracle::ffmpeg("ffmpeg_encoded_streams_decode_like_ffmpeg") else { return };
    let (rate, secs) = (44_100u32, 2.0);
    let pcm = oracle::vec_to_f32le(&program(rate, secs));
    let true_frames = (f64::from(rate) * secs) as usize;
    let dir = oracle::scratch("decode");
    // (file, ffmpeg args after the input, min SNR, max |err|, gapless restores the input length)
    let encodes: &[(&str, &[&str], Option<f64>, f64, bool)] = &[
        ("lame.mp3", &["-c:a", "libmp3lame", "-b:a", "192k"], Some(100.0), 5e-6, true),
        ("aac.m4a", &["-c:a", "aac", "-b:a", "128k"], Some(100.0), 2e-5, true),
        ("alac.m4a", &["-c:a", "alac"], None, 0.0, true),
        ("aac.aac", &["-c:a", "aac", "-b:a", "128k", "-f", "adts"], Some(100.0), 2e-5, false),
        ("twolame.mp2", &["-c:a", "mp2", "-b:a", "192k"], Some(70.0), 4e-5, false),
        ("lossless.flac", &["-c:a", "flac"], None, 0.0, true),
    ];
    for &(file, args, min_snr_db, max_abs, gapless) in encodes {
        let path = dir.join(file);
        let mut a: Vec<&std::ffi::OsStr> = ["-y", "-f", "f32le", "-ar", "44100", "-ac", "2", "-i", "-"].iter().map(std::ffi::OsStr::new).collect();
        a.extend(args.iter().map(std::ffi::OsStr::new));
        a.push(path.as_os_str());
        oracle::run(&ff, &a, Some(&pcm));
        let case = Case { file, min_snr_db, max_abs, true_frames: gapless.then_some(true_frames), ffmpeg_extra: 0 };
        check(file, &ff, &path, &case);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
