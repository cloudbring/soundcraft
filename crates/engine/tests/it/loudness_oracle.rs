//! Soundcraft's own program material through the whole chain: the demo session is bounced to a
//! 32-bit float WAV, the engine reports the loudness it measured while writing, and the same file
//! is measured by ffmpeg's `ebur128` filter and by `craft-compare`. Without ffmpeg this is a
//! declared skip (a failure with `SOUNDCRAFT_REQUIRE_ORACLES=1`).

use crate::oracle;
use craft_compare::loudness::{STEREO, integrated_lufs};

/// (integrated LUFS, true peak dBTP) from ffmpeg's last 100 ms frame.
fn ffmpeg_measure(ff: &std::path::Path, wav: &std::path::Path) -> (f64, f64) {
    let out = oracle::run(
        ff,
        &[
            "-i".as_ref(),
            wav.as_os_str(),
            "-af".as_ref(),
            "ebur128=peak=true:metadata=1,ametadata=mode=print:file=-".as_ref(),
            "-f".as_ref(),
            "null".as_ref(),
            "-".as_ref(),
        ],
        None,
    );
    let (mut i, mut tp) = (f64::NAN, f64::NAN);
    for line in String::from_utf8_lossy(&out).lines() {
        let Some((k, v)) = line.strip_prefix("lavfi.r128.").and_then(|l| l.split_once('=')) else { continue };
        match k {
            "I" => i = v.trim().parse().unwrap_or(f64::NAN),
            "true_peak" => tp = 20.0 * v.trim().parse::<f64>().unwrap_or(f64::NAN).log10(),
            _ => {}
        }
    }
    (i, tp)
}

#[test]
fn demo_bounce_loudness_matches_ffmpeg_and_craft_compare() {
    let Some(ff) = oracle::ffmpeg("demo_bounce_loudness_matches_ffmpeg_and_craft_compare") else { return };
    let mut e = soundcraft_engine::demo::demo_engine();
    let sr = e.session().sample_rate.hz();
    let dir = oracle::scratch("engine-demo");
    let path = dir.join("demo.wav");
    let result = e
        .execute(
            "file.bounce_mix",
            &serde_json::json!({"path": path, "bit_depth": "32f", "dither": false, "normalize": false, "start": 0, "end": i64::from(sr) * 30}),
        )
        .unwrap();
    let ours_i = result["lufs"].as_f64().unwrap();
    let ours_tp = result["true_peak_db"].as_f64().unwrap();
    let (_, buf) = soundcraft_audio_io::decode(&std::fs::read(&path).unwrap(), Some("wav")).unwrap();
    assert_eq!(buf.num_channels(), 2);
    let cc = integrated_lufs(&buf.interleaved(), 2, sr, &STEREO).unwrap().expect("demo has measurable loudness");
    let (ff_i, ff_tp) = ffmpeg_measure(&ff, &path);
    eprintln!("ROW | demo session 30 s | {ours_i:.3} | {cc:.3} | {ff_i:.3} | {ours_tp:.3} | {ff_tp:.3}");
    assert!(ours_i > -60.0, "the demo bounce should not be silent: {ours_i}");
    assert!((ours_i - cc).abs() <= 0.02, "ours {ours_i:.3} vs craft-compare {cc:.3}");
    assert!((ours_i - ff_i).abs() <= 0.1, "ours {ours_i:.3} vs ffmpeg {ff_i:.3}");
    assert!((ours_tp - ff_tp).abs() <= 0.2, "true peak ours {ours_tp:.3} vs ffmpeg {ff_tp:.3}");
    let _ = std::fs::remove_dir_all(&dir);
}
