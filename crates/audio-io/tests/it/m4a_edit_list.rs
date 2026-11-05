//! AAC in MP4: the edit list (not the packet count) defines the presented audio. Encoders put 1024
//! priming frames and the end padding outside it; ignoring the list shifted every imported .m4a
//! late by the priming and left it longer than the source. Found by the ffmpeg decode oracle
//! (`decode_oracle.rs`); this test needs no ffmpeg.

use soundcraft_audio_io::{decode, probe};

#[test]
fn m4a_decodes_to_the_length_the_edit_list_presents() {
    // tone.m4a: a 0.1 s, 8 kHz tone = 800 frames, encoded as 2 x 1024 AAC frames with an edit list
    // that skips the 1024 priming frames and presents 800.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.m4a");
    let bytes = std::fs::read(path).unwrap();
    let (info, buf) = decode(&bytes, Some("m4a")).unwrap();
    assert_eq!(buf.frames(), 800);
    assert_eq!(info.frames, 800);
    assert_eq!(probe(&bytes, Some("m4a")).unwrap().frames, 800, "probe and decode must agree");
    // The tone starts at once: the first samples after the priming are the tone, not the encoder's ramp-in.
    let early = buf.channels[0].iter().take(40).fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(early > 0.05, "tone should be audible from the first frames, peak {early}");
}
