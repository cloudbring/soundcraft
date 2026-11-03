use soundcraft_video::{bitstream, h264};

#[test]
fn public_codec_paths_are_shared_types() {
    let decoder: h264::Decoder = craft_h264::Decoder::new();
    let _: craft_h264::Decoder = decoder;
    let writer: bitstream::BitWriter = craft_bitstream::BitWriter::new();
    let _: craft_bitstream::BitWriter = writer;
}

#[test]
fn decoder_thread_defaults_follow_video_feature() {
    let expected = if cfg!(all(feature = "threads", not(target_arch = "wasm32"))) {
        std::thread::available_parallelism().map_or(1, usize::from).min(16)
    } else {
        1
    };
    assert_eq!(h264::default_threads(), expected);
}
