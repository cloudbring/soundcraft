use super::*;

fn test_state(
    rx: SpscConsumer<Cmd>,
    shared: Arc<Shared>,
    session: Arc<Session>,
    rate: f64,
    garbage: SpscProducer<Vec<Box<dyn Plugin>>>,
) -> AudioState {
    let (old_tx, old_rx) = Spsc::new(OLD_SESSIONS).split();
    let (meter_tx, meter_rx) = Spsc::new(8).split();
    let (mut recycle_tx, recycle_rx) = Spsc::new(8).split();
    for _ in 0..8 {
        let _ = recycle_tx.push(MeterSnapshot::for_session(&session));
    }
    let (_, beat) = StallDetector::new(STALL, std::time::Instant::now());
    let (_, index) = Triple::new(ClipLookup::default(), ClipLookup::build(&session), ClipLookup::default()).split();
    let mut state = AudioState::new(
        rx,
        shared,
        session,
        rate,
        AudioPorts {
            garbage,
            old_sessions: old_tx,
            meter_tx,
            meter_recycle: recycle_rx,
            beat,
            index,
            session_adopted: Arc::new(AtomicBool::new(true)),
        },
    );
    state.test_meter_ui = Some(MeterUi::new(meter_rx, recycle_tx));
    state.test_old = Some(old_rx);
    state
}

fn test_meters(state: &mut AudioState) -> MeterSnapshot {
    let mut snapshot = MeterSnapshot::default();
    if let Some(meters) = state.test_meter_ui.as_mut() {
        meters.take(&mut snapshot);
    }
    snapshot
}

fn test_side(rate: f32) -> Side {
    let (_, garbage) = Spsc::new(OLD_SESSIONS).split();
    let (_, old) = Spsc::new(OLD_SESSIONS).split();
    Side::new(rate, garbage, old)
}

/// A stream that builds; `starts` says whether the device lets it play (it can refuse after a
/// reconnect).
struct Fake {
    starts: bool,
}

impl cpal::traits::StreamTrait for Fake {
    /// Play reports the device's refusal when `starts` is false.
    fn play(&self) -> Result<(), cpal::Error> {
        if self.starts { Ok(()) } else { Err(cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable)) }
    }
    /// Pause always succeeds on the fake stream.
    fn pause(&self) -> Result<(), cpal::Error> {
        Ok(())
    }
    /// The fake stream has no clock and no fixed buffer; neither is used by these tests.
    fn buffer_size(&self) -> Result<cpal::FrameCount, cpal::Error> {
        Ok(0)
    }
    /// The fake stream has no usable clock; these tests do not read it.
    fn now(&self) -> cpal::StreamInstant {
        cpal::StreamInstant::from_nanos(0)
    }
}

#[test]
fn a_stream_that_will_not_start_is_reported_as_failed() {
    assert!(!start_stream(&Fake { starts: false }));
    assert!(start_stream(&Fake { starts: true }));
}

#[test]
fn the_first_config_that_starts_wins_and_later_ones_are_not_tried() {
    let mut tried = Vec::new();
    let won = first_started(
        [1u32, 2, 3],
        |&n| {
            tried.push(n);
            Ok::<_, String>(Fake { starts: n != 1 })
        },
        start_stream,
    );
    assert_eq!(won.map(|(n, _)| n), Some(2));
    assert_eq!(tried, vec![1, 2]);
}

#[test]
fn a_config_that_fails_to_build_falls_through_to_the_next() {
    let won = first_started([1u32, 2], |&n| if n == 1 { Err("no such buffer size".to_string()) } else { Ok(Fake { starts: true }) }, start_stream);
    assert_eq!(won.map(|(n, _)| n), Some(2));
}

/// Build a cpal configuration range for tests with a 64..1024-frame buffer.
fn range(channels: u16, min: u32, max: u32, format: cpal::SampleFormat) -> cpal::SupportedStreamConfigRange {
    cpal::SupportedStreamConfigRange::new(channels, min, max, cpal::SupportedBufferSize::Range { min: 64, max: 1024 }, format)
}

/// Rates are tried in order, so a stale configured rate falls through to the session rate; when no requested rate
/// is offered the caller uses the device default and resamples.
#[test]
fn output_config_uses_the_first_requested_rate_the_device_offers() {
    let configs = [
        range(2, 44_100, 48_000, cpal::SampleFormat::I16),
        range(2, 44_100, 96_000, cpal::SampleFormat::F32),
        range(8, 44_100, 96_000, cpal::SampleFormat::F32),
    ];
    let (config, buffers, format) = pick_output_config(&configs, &[96_000, 48_000], 8).unwrap();
    assert_eq!((config.sample_rate, config.channels, config.buffer_size), (96_000, 8, cpal::BufferSize::Default));
    assert_eq!(format, cpal::SampleFormat::F32);
    assert_eq!(buffers, cpal::SupportedBufferSize::Range { min: 64, max: 1024 });
    // 192 kHz is not offered: the session's 48 kHz is tried next.
    let (config, _, format) = pick_output_config(&configs, &[192_000, 48_000], 8).unwrap();
    assert_eq!((config.sample_rate, config.channels, format), (48_000, 8, cpal::SampleFormat::F32));
    // No requested rate is offered: the caller uses the device's default config and resamples.
    assert!(pick_output_config(&configs, &[192_000], 8).is_none());
}

/// At a supported rate, a preferred sample format wins over more channels; output stays between stereo and the
/// requested channel cap, and mono-only or wider devices take the default path.
#[test]
fn output_config_prefers_format_over_channels_and_needs_stereo_within_the_cap() {
    let configs = [range(8, 44_100, 48_000, cpal::SampleFormat::I16), range(2, 44_100, 48_000, cpal::SampleFormat::F32)];
    let (config, _, format) = pick_output_config(&configs, &[48_000], 8).unwrap();
    assert_eq!((config.channels, format), (2, cpal::SampleFormat::F32), "a preferred format wins over more channels");
    let (config, _, _) = pick_output_config(&configs, &[48_000], 2).unwrap();
    assert_eq!(config.channels, 2, "the channel cap applies");
    let mono = [range(1, 44_100, 48_000, cpal::SampleFormat::F32)];
    assert!(pick_output_config(&mono, &[48_000], 8).is_none(), "a mono-only device takes the default config path");
    let surround = [range(6, 44_100, 48_000, cpal::SampleFormat::F32)];
    assert!(pick_output_config(&surround, &[48_000], 2).is_none(), "a wider device is not narrowed here");
    assert_eq!(pick_output_config(&surround, &[48_000], 6).map(|(c, _, _)| c.channels), Some(6));
}

#[test]
fn no_config_that_starts_yields_none_so_the_caller_falls_back_to_a_silent_clock() {
    let mut tried = 0;
    let won = first_started(
        [1u32, 2],
        |_| {
            tried += 1;
            Ok::<_, String>(Fake { starts: false })
        },
        start_stream,
    );
    assert!(won.is_none());
    assert_eq!(tried, 2);
}

#[test]
fn reusable_meter_snapshot_updates_and_drops_stale_tracks() {
    let keep = TrackId(1);
    let stale = TrackId(2);
    let mut source = MeterSnapshot::default();
    source.tracks.insert(keep, StripMeter { peak: [0.5, 0.25], peaks: vec![0.5, 0.25], gain_reduction_db: 3.0, ..StripMeter::default() });
    source.main.peaks = vec![0.75, 0.5];
    let mut out = MeterSnapshot::default();
    out.tracks.insert(stale, StripMeter::default());
    copy_meter_snapshot(&source, &mut out);
    assert_eq!(out.tracks.len(), 1);
    assert_eq!(out.tracks[&keep], source.tracks[&keep]);
    assert_eq!(out.main, source.main);
}

/// A mono track playing a quiet signal with one short spike in the block starting at
/// `spike_at`, and an audio thread ready to play it.
fn spike_session(spike_at: usize) -> (Arc<Shared>, AudioState, SpscProducer<Cmd>, TrackId) {
    use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
    let mut s = soundcraft_model::Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    let mut x = vec![0.01f32; 48_000];
    x[spike_at..spike_at + 32].fill(0.9);
    s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![x] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 48_000));
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let st = test_state(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, Spsc::new(OLD_SESSIONS).split().0);
    tx.push(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    (shared, st, tx, t)
}

#[test]
fn a_spike_in_one_block_among_quiet_ones_reaches_the_ui() {
    // Eight 512-frame blocks per callback, as between two UI polls; the spike is in the 3rd.
    let (_shared, mut st, _tx, t) = spike_session(2 * 512 + 100);
    let mut out = vec![0.0f32; 2 * 8 * 512];
    st.fill(&mut out, 2);
    // The last block alone (what the UI used to see) is quiet.
    assert!(st.mix.meters[&t].peak[0] < 0.05);
    let mut ui = MeterSnapshot::default();
    st.test_meter_ui.as_mut().unwrap().take(&mut ui);
    assert!(ui.tracks[&t].peak[0] > 0.85, "{:?}", ui.tracks[&t]);
    assert!(ui.tracks[&t].peaks[0] > 0.85);
    assert!(ui.main.peak[0] > 0.5 && ui.main.peaks.iter().all(|p| *p > 0.5), "{:?}", ui.main);
    // Taking starts a new period: the next quiet blocks read quiet.
    st.fill(&mut out, 2);
    st.test_meter_ui.as_mut().unwrap().take(&mut ui);
    assert!(ui.tracks[&t].peak[0] < 0.05 && ui.main.peak[0] < 0.05);
    // No block since the last take: the reading stays rather than dropping to silence.
    st.test_meter_ui.as_mut().unwrap().take(&mut ui);
    assert!(ui.tracks[&t].peak[0] > 0.005);
}

#[test]
fn a_spike_rendered_before_the_ui_polls_arrives_later() {
    let (_shared, mut st, _tx, t) = spike_session(100);
    let mut out = vec![0.0f32; 2 * 512];
    st.fill(&mut out, 2);
    st.fill(&mut out, 2);
    let mut ui = MeterSnapshot::default();
    st.test_meter_ui.as_mut().unwrap().take(&mut ui);
    assert!(ui.tracks[&t].peak[0] > 0.85, "{:?}", ui.tracks[&t]);
}

#[test]
fn stopping_zeroes_meters_in_place_and_removed_tracks_are_pruned() {
    let (_shared, mut st, mut tx, t) = spike_session(100);
    let mut out = vec![0.0f32; 2 * 512];
    st.fill(&mut out, 2);
    tx.push(Cmd::Stop).unwrap();
    st.fill(&mut out, 2);
    {
        let m = test_meters(&mut st);
        let e = m.tracks.get(&t).expect("kept while stopped, so playing again does not allocate it");
        assert!(e.peak == [0.0; 2] && e.peaks.iter().all(|p| *p == 0.0) && e.peaks.len() == 1, "{e:?}");
        assert!(m.tracks.contains_key(&t), "the UI sees the silence");
    }
    // The track is deleted: the UI side works out which tracks went and says so.
    let mut side = test_side(48_000.0);
    side.gone_tracks(&st.session);
    let empty = soundcraft_model::Session::default();
    let gone = side.gone_tracks(&empty);
    assert_eq!(gone, vec![t]);
    tx.push(Cmd::Session(Arc::new(empty), Vec::new())).unwrap();
    tx.push(Cmd::Forget(gone)).unwrap();
    tx.push(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    st.fill(&mut out, 2);
    assert!(test_meters(&mut st).tracks.get(&t).is_none_or(|m| m.peak[0] == 0.0));
    assert!(!st.unsent.tracks.contains_key(&t) && !st.mix.meters.contains_key(&t));
}

#[test]
fn a_take_without_new_blocks_still_drops_deleted_tracks() {
    let gone = TrackId(7);
    let mut source = MeterSnapshot::default();
    source.accumulate(&StripMeter::default(), &HashMap::from([(gone, StripMeter::default())]));
    let mut ui = MeterSnapshot::default();
    take_meter_snapshot(&mut source, &mut ui);
    source.forget(&[gone]);
    take_meter_snapshot(&mut source, &mut ui);
    assert!(ui.tracks.is_empty());
}

#[test]
fn a_channel_count_change_takes_the_new_stereo_peaks() {
    let mut acc = StripMeter { peak: [0.9, 0.9], peaks: vec![0.9], ..StripMeter::default() };
    acc.accumulate(&StripMeter { peak: [0.9, 0.0], peaks: vec![0.9, 0.0], ..StripMeter::default() });
    assert_eq!(acc.peak, [0.9, 0.0]);
}

#[test]
fn gain_reduction_keeps_its_maximum_until_taken() {
    let mut acc = StripMeter::default();
    for gr in [1.0, 9.0, 2.0, f32::NAN] {
        acc.accumulate(&StripMeter { peak: [0.1, 0.1], peaks: vec![0.1], gain_reduction_db: gr, ..StripMeter::default() });
    }
    assert_eq!(acc.gain_reduction_db, 9.0);
    acc.reset_levels();
    assert_eq!((acc.gain_reduction_db, acc.peak, acc.peaks.len()), (0.0, [0.0; 2], 1));
}

#[test]
fn a_block_that_panics_goes_silent_and_is_reported_once() {
    let shared =
        Shared { position: AtomicI64::new(0), playing: AtomicBool::new(true), speed: AtomicParam::new(1.0), output_gain: AtomicParam::new(1.0) };
    let fault = Fault::default();
    let mut out = [0.5f32; 8];
    guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25));
    assert_eq!((out, fault.trouble(false)), ([0.25; 8], None));
    guarded_block(&fault, &shared, &mut out, |_| std::panic::panic_any("a bug in the mix"));
    guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25));
    assert_eq!(out, [0.0; 8]);
    assert!(!shared.playing.load(Ordering::Relaxed));
    assert_eq!((fault.trouble(false), fault.trouble(false)), (Some(OutputTrouble::Bug), None));
}

#[test]
fn a_lost_or_stalled_device_is_reported_once() {
    let now = std::time::Instant::now();
    let (mut watch, beat) = StallDetector::new(STALL, now);
    assert!(!watch.observe(true, now));
    assert!(watch.observe(true, now + STALL));
    beat.beat();
    assert!(!watch.observe(true, now + STALL), "it asked again");
    assert!(!watch.observe(false, now + 2 * STALL), "stopped, so nothing to ask for");
    assert_eq!(Fault::default().trouble(true), Some(OutputTrouble::Stalled));
    let fault = Fault::default();
    fault.gone.store(true, Ordering::Relaxed);
    assert_eq!((fault.trouble(false), fault.trouble(true)), (Some(OutputTrouble::Gone), None));
}

#[test]
fn audio_state_plays_and_stops_at_end() {
    let s = Arc::new(soundcraft_model::Session::default());
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let mut st = test_state(rx, Arc::clone(&shared), s, 48_000.0, Spsc::new(OLD_SESSIONS).split().0);
    tx.push(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
    let mut out = vec![0.0f32; 4096];
    st.fill(&mut out, 2);
    assert!(shared.position.load(Ordering::Relaxed) >= 1000);
    assert!(!shared.playing.load(Ordering::Relaxed));
}

#[test]
fn short_device_callbacks_render_short_mix_blocks() {
    let s = Arc::new(soundcraft_model::Session::default());
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let mut st = test_state(rx, Arc::clone(&shared), s, 48_000.0, Spsc::new(OLD_SESSIONS).split().0);
    tx.push(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    let mut out = vec![0.0f32; 2 * 64];
    st.fill(&mut out, 2);
    assert_eq!(shared.position.load(Ordering::Relaxed), 64);
}

fn audio_output_state(format: soundcraft_model::ChannelFormat, channels: Vec<Vec<f32>>) -> AudioState {
    audio_output_state_with_end(format, channels, None)
}

fn audio_output_state_with_end(format: soundcraft_model::ChannelFormat, channels: Vec<Vec<f32>>, end: Option<Samples>) -> AudioState {
    use soundcraft_model::{Clip, SourceAudio, SourceId, TrackKind};
    let mut s = Session::default();
    s.edit.click = false;
    s.outputs[0].format = format;
    let t = s.add_track(TrackKind::Audio, format, None);
    let frames = channels[0].len();
    s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "output", SourceId(5), 0, 0, frames as i64));
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let st = test_state(rx, shared, Arc::new(s), 48_000.0, Spsc::new(OLD_SESSIONS).split().0);
    tx.push(Cmd::Play { from: 0, end, looped: None }).unwrap();
    st
}

#[test]
fn mono_device_averages_stereo_before_clipping() {
    use soundcraft_model::ChannelFormat;
    // Right only, left only, identical, opposite polarity, and levels that must be averaged
    // before clipping either side to the device's range.
    let left = [0.0, 0.8, 0.8, 0.8, 2.0, -2.0, 2.0];
    let right = [0.8, 0.0, 0.8, -0.8, -0.5, 0.5, 2.0];
    let expected = [0.4, 0.4, 0.8, 0.0, 0.75, -0.75, 1.0];
    for callback in [1, 37, 512] {
        let channels = [left, right].map(|pattern| (0..2048).map(|i| pattern[i % pattern.len()]).collect());
        let mut st = audio_output_state(ChannelFormat::Stereo, channels.into());
        let mut out = vec![0.0; callback];
        for block in 0..3 {
            st.fill(&mut out, 1);
            for (i, actual) in out.iter().enumerate() {
                let at = block * callback + i;
                assert!((actual - expected[at % expected.len()]).abs() < 1e-6, "callback {callback}, sample {at}: {actual}");
            }
        }
    }
}

#[test]
fn resampling_at_a_short_block_edge_does_not_mix_in_stale_frames() {
    use soundcraft_model::ChannelFormat;
    // DC for the first long block, then silence. The device runs slower than the session, so
    // reads land between frames; a short block leaves the long block's DC past `pending_len`.
    let mut st = audio_output_state(ChannelFormat::Stereo, vec![vec![0.8; BLOCK]; 2]);
    st.device_rate = 44_100.0;
    let mut long = vec![0.0; 2 * 470];
    st.fill(&mut long, 2);
    assert_eq!(st.pending_len, BLOCK);
    let mut short = [0.0; 2 * 8];
    for call in 0..40 {
        st.fill(&mut short, 2);
        assert!(st.pending_len < BLOCK);
        if call >= 32 {
            assert!(short.iter().all(|x| x.abs() < 1e-6), "call {call}: {short:?}");
        }
    }
}

#[test]
fn sinc_callback_is_independent_of_device_block_size() {
    use soundcraft_model::ChannelFormat;
    let source: Vec<_> = (0..12_000).map(|frame| (std::f64::consts::TAU * 1000.0 * frame as f64 / 48_000.0).sin() as f32 * 0.5).collect();
    let render = |blocks: &[usize], speed: u32| {
        let mut state = audio_output_state(ChannelFormat::Stereo, vec![source.clone(), source.clone()]);
        state.device_rate = 44_100.0;
        state.shared.speed.store(speed as f32 / 1000.0);
        let mut result = Vec::new();
        let mut block = 0;
        while result.len() < 4410 {
            let frames = blocks[block % blocks.len()].min(4410 - result.len());
            let mut buffer = vec![0.0; 2 * frames];
            state.fill(&mut buffer, 2);
            result.extend(buffer.as_chunks::<2>().0.iter().map(|frame| frame[0]));
            block += 1;
        }
        result
    };
    for speed in [100, 1000, 4000] {
        let reference = render(&[512], speed);
        let variable = render(&[1, 7, 64, 333, 1024], speed);
        for (frame, (left, right)) in reference.iter().zip(variable).enumerate() {
            assert!((left - right).abs() < 1e-6, "speed {speed}, frame {frame}: {left} vs {right}");
        }
    }
}

#[test]
fn converted_playback_flushes_its_tail_without_advancing_transport() {
    use soundcraft_model::ChannelFormat;
    let mut source = vec![0.0; 1000];
    source[999] = 0.5;
    let mut state = audio_output_state_with_end(ChannelFormat::Stereo, vec![source.clone(), source], Some(1000));
    state.device_rate = 44_100.0;
    let mut output = vec![0.0; 2 * 1500];
    state.fill(&mut output, 2);
    let peak = output.as_chunks::<2>().0.iter().enumerate().max_by(|left, right| left.1[0].abs().total_cmp(&right.1[0].abs())).unwrap();
    let expected = (999 + resample::DELAY) as f64 * 44_100.0 / 48_000.0;
    assert!((peak.0 as f64 - expected).abs() < 1.0);
    assert!(peak.1[0] > 0.2);
    assert_eq!(state.shared.position.load(Ordering::Relaxed), 1000);
    assert!(!state.shared.playing.load(Ordering::Relaxed));
    state.fill(&mut output, 2);
    assert!(output.iter().all(|sample| *sample == 0.0));
}

#[test]
fn sinc_callback_speed_changes_allocate_nothing_after_warmup() {
    use soundcraft_model::ChannelFormat;
    let mut state = audio_output_state(ChannelFormat::Stereo, vec![vec![0.25; 48_000]; 2]);
    state.device_rate = 44_100.0;
    let mut output = [0.0; 2 * 1024];
    state.fill(&mut output, 2);
    state.fill(&mut output, 2);
    let allocations = allocation_counter::measure(|| {
        for speed in [100, 1000, 4000, 500, 1000] {
            state.shared.speed.store(speed as f32 / 1000.0);
            for frames in [1, 7, 64, 512, 1024] {
                state.fill(&mut output[..2 * frames], 2);
            }
        }
    });
    assert_eq!(allocations.count_total, 0, "{allocations:?}");
    let allocations = allocation_counter::measure(|| {
        for _ in 0..200 {
            state.fill(&mut output, 2);
        }
    });
    assert_eq!(allocations.count_total, 0, "even with the meter queue full: {allocations:?}");
}

#[test]
fn output_gain_changes_ramp_without_a_step() {
    use soundcraft_model::ChannelFormat;
    let mut state = audio_output_state(ChannelFormat::Stereo, vec![vec![0.25; 4096]; 2]);
    let mut output = [0.0; 64];
    state.fill(&mut output, 2);
    assert_eq!(output, [0.25; 64]);
    state.shared.output_gain.store(0.0);
    let allocations = allocation_counter::measure(|| state.fill(&mut output, 2));
    assert_eq!(allocations.count_total, 0, "{allocations:?}");
    assert!(output[0] > output[62] && output[0] < 0.25);
    assert_eq!(output[62], 0.0);
    state.fill(&mut output, 2);
    assert_eq!(output, [0.0; 64]);
}

#[test]
fn edit_thread_publishes_clip_index_before_audio_adopts_session() {
    use soundcraft_model::ChannelFormat;
    let initial = audio_output_state(ChannelFormat::Stereo, vec![vec![0.25; 4096]; 2]);
    let next = Arc::new((*initial.session).clone());
    let (mut edit, audio) = Triple::new(ClipLookup::default(), ClipLookup::build(&initial.session), ClipLookup::default()).split();
    *edit.edit_mut() = ClipLookup::build(&next);
    assert!(edit.publish());
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let (_, beat) = StallDetector::new(STALL, std::time::Instant::now());
    let (meter_tx, _) = Spsc::new(8).split();
    let (_, meter_recycle) = Spsc::new(8).split();
    let mut state = AudioState::new(
        rx,
        initial.shared,
        initial.session,
        48_000.0,
        AudioPorts {
            garbage: Spsc::new(OLD_SESSIONS).split().0,
            old_sessions: Spsc::new(OLD_SESSIONS).split().0,
            meter_tx,
            meter_recycle,
            beat,
            index: audio,
            session_adopted: Arc::new(AtomicBool::new(true)),
        },
    );
    tx.push(Cmd::Session(Arc::clone(&next), Vec::new())).unwrap();
    tx.push(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    let mut output = [0.0; 64];
    state.fill(&mut output, 2);
    assert!(Arc::ptr_eq(&state.session, &next));
    assert!(output.iter().all(|sample| (*sample - 0.25).abs() < 1e-5));
}

#[test]
fn rapid_session_edits_wait_for_their_published_index() {
    let initial = Arc::new(Session::default());
    let first = Arc::new((*initial).clone());
    let second = Arc::new((*initial).clone());
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (tx, rx) = Spsc::new(COMMANDS).split();
    let (index, audio_index) = Triple::new(ClipLookup::default(), ClipLookup::build(&initial), ClipLookup::default()).split();
    let session_adopted = Arc::new(AtomicBool::new(true));
    let (_, beat) = StallDetector::new(STALL, std::time::Instant::now());
    let (meter_tx, _) = Spsc::new(8).split();
    let (_, meter_recycle) = Spsc::new(8).split();
    let mut state = AudioState::new(
        rx,
        shared,
        initial,
        48_000.0,
        AudioPorts {
            garbage: Spsc::new(OLD_SESSIONS).split().0,
            old_sessions: Spsc::new(OLD_SESSIONS).split().0,
            meter_tx,
            meter_recycle,
            beat,
            index: audio_index,
            session_adopted: Arc::clone(&session_adopted),
        },
    );
    let mut control = Control { tx, pending: Default::default(), index, session_adopted, session_published: false };
    control.send(Cmd::Session(Arc::clone(&first), Vec::new()));
    control.send(Cmd::Session(Arc::clone(&second), Vec::new()));
    assert_eq!(control.pending.len(), 1);
    let mut output = [0.0; 64];
    state.fill(&mut output, 2);
    assert!(Arc::ptr_eq(&state.session, &first));
    assert!(state.index.current().matches(&first));
    control.flush();
    assert!(control.pending.is_empty());
    state.fill(&mut output, 2);
    assert!(Arc::ptr_eq(&state.session, &second));
    assert!(state.index.current().matches(&second));
}

#[test]
fn device_callback_contains_no_lock_or_channel_receiver() {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<AudioState>();
    for source in [include_str!("callback.rs"), include_str!("stream.rs")] {
        for forbidden in ["Mutex", "mpsc", "try_lock", ".lock("] {
            assert!(!source.contains(forbidden), "realtime adapter contains {forbidden}");
        }
    }
}

#[test]
fn playback_callback_rejects_rate_conversion_images() {
    use soundcraft_model::ChannelFormat;
    let source: Vec<_> = (0..12_000).map(|frame| (std::f64::consts::TAU * 1000.0 * frame as f64 / 48_000.0).sin() as f32 * 0.5).collect();
    let mut state = audio_output_state(ChannelFormat::Stereo, vec![source.clone(), source]);
    state.device_rate = 44_100.0;
    let mut output = vec![0.0; 2 * 6000];
    state.fill(&mut output, 2);
    let amplitude = |frequency: f64| {
        let (mut real, mut imaginary) = (0.0, 0.0);
        for (frame, sample) in output.as_chunks::<2>().0.iter().skip(1000).take(4410).enumerate() {
            let angle = std::f64::consts::TAU * frequency * frame as f64 / 44_100.0;
            real += f64::from(sample[0]) * angle.cos();
            imaginary += f64::from(sample[0]) * angle.sin();
        }
        real.hypot(imaginary)
    };
    let rejection = 20.0 * (amplitude(1000.0) / amplitude(2900.0).max(amplitude(4900.0))).log10();
    println!("Playback callback: {rejection:.2} dB image rejection");
    assert!(rejection > 60.0, "{rejection}");
}

#[test]
fn mono_device_preserves_centered_mono_level_and_includes_surround_right_channels() {
    use soundcraft_model::ChannelFormat;
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let mut mono = audio_output_state(ChannelFormat::Mono, vec![vec![0.8; 512]]);
    let mut out = [0.0; 128];
    mono.fill(&mut out, 1);
    let centered = 0.8 * soundcraft_dsp::db_to_gain(-3.0);
    assert!(out.iter().all(|x| (*x - centered).abs() < 1e-6), "a centered mono track keeps its rendered level under the -3 dB pan law");

    // Independently exercise each SMPTE 5.1 channel: L, R, C, LFE, Ls, Rs.
    let channels = (0..6).map(|c| (0..512).map(|i| if i % 6 == c { 0.8 } else { 0.0 }).collect()).collect();
    let mut surround = audio_output_state(ChannelFormat::Surround51, channels);
    surround.fill(&mut out, 1);
    let expected = [0.4, 0.4, 0.8 * h, 0.0, 0.4 * h, 0.4 * h];
    for (i, actual) in out.iter().enumerate() {
        assert!((actual - expected[i % expected.len()]).abs() < 1e-6, "surround sample {i}: {actual}");
    }
}

#[test]
fn mono_device_contains_overflow_from_opposite_surround_sides() {
    use soundcraft_model::ChannelFormat;
    let channels = [f32::MAX, -f32::MAX, 0.0, 0.0, f32::MAX, -f32::MAX].map(|v| vec![v; 128]);
    let mut st = audio_output_state(ChannelFormat::Surround51, channels.into());
    let mut out = [0.5; 128];
    st.fill(&mut out, 1);
    assert!(out.iter().all(|x| *x == 0.0), "overflow in the stereo fold-down must not send NaN to a mono device");
}

#[test]
fn interpolation_overflow_between_huge_samples_never_reaches_the_device() {
    use soundcraft_model::ChannelFormat;
    let alternating: Vec<f32> = (0..512).map(|i| if i % 2 == 0 { -3.4e38 } else { 3.4e38 }).collect();
    for (format, nch, device) in [(ChannelFormat::Stereo, 2, 2), (ChannelFormat::Stereo, 2, 8), (ChannelFormat::Mono, 1, 1)] {
        let mut st = audio_output_state(format, vec![alternating.clone(); nch]);
        let mut out = vec![0.5; 64 * device];
        st.fill(&mut out, device);
        assert!(out.iter().all(|x| x.is_finite() && (-1.0..=1.0).contains(x)), "{format:?} on {device} channels: {out:?}");
    }
}

#[test]
fn surround_main_maps_to_device_channels_or_folds_down() {
    use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
    let mut s = soundcraft_model::Session::default();
    s.outputs[0].format = ChannelFormat::Surround51;
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Surround51, None);
    let chans: Vec<Vec<f32>> = (0..6).map(|c| vec![0.1 * (c + 1) as f32; 48_000]).collect();
    s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: chans })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 48_000));
    let s = Arc::new(s);
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let mut st = test_state(rx, Arc::clone(&shared), Arc::clone(&s), 48_000.0, Spsc::new(OLD_SESSIONS).split().0);
    tx.push(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    // An 8-channel device: channels in order, the rest silent.
    let mut out = vec![0.0f32; 8 * 256];
    st.fill(&mut out, 8);
    let f = &out[8 * 100..8 * 101];
    for c in 0..6 {
        assert!((f[c] - 0.1 * (c + 1) as f32).abs() < 1e-5, "{f:?}");
    }
    assert!(f[6] == 0.0 && f[7] == 0.0);
    // A stereo device: ITU fold-down.
    let mut out = vec![0.0f32; 2 * 256];
    st.fill(&mut out, 2);
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let l = (0.1 + h * 0.3 + h * 0.5f32).min(1.0);
    assert!((out[200] - l).abs() < 1e-4, "{} vs {l}", out[200]);
    assert_eq!(test_meters(&mut st).main.peaks.len(), 6);
}

#[test]
fn instances_arrive_with_the_session_and_leave_through_the_return_channel() {
    use soundcraft_model::{ChannelFormat, Insert, TrackKind};
    let mut s = soundcraft_model::Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.fake"));
    let s = Arc::new(s);
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let (gtx, mut grx) = Spsc::new(OLD_SESSIONS).split();
    let mut st = test_state(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
    let plugin = soundcraft_dsp::create("gain").unwrap();
    tx.push(Cmd::Session(Arc::clone(&s), vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin }])).unwrap();
    let mut out = vec![0.0f32; 256];
    st.fill(&mut out, 2);
    assert!(grx.pop().is_none(), "nothing retired yet");
    // A session without the insert retires the instance to the return channel.
    tx.push(Cmd::Session(Arc::new(soundcraft_model::Session::default()), Vec::new())).unwrap();
    st.fill(&mut out, 2);
    assert_eq!(grx.pop().unwrap().len(), 1);
    // A sample-rate change rebuilds the engine without dropping plugins on this thread.
    tx.push(Cmd::Session(
        Arc::clone(&s),
        vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin: soundcraft_dsp::create("gain").unwrap() }],
    ))
    .unwrap();
    st.fill(&mut out, 2);
    let mut s44 = (*s).clone();
    s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
    tx.push(Cmd::Session(Arc::new(s44), Vec::new())).unwrap();
    st.fill(&mut out, 2);
    assert_eq!(grx.pop().unwrap().len(), 1);
}

#[test]
fn plugin_states_are_read_without_the_audio_thread() {
    let mut side = test_side(48_000.0);
    let (a, b) = (TrackId(1), TrackId(2));
    side.states.insert((b, 0), Box::new(|| Some(b"b0".to_vec())));
    side.states.insert((a, soundcraft_mix::INSTRUMENT_SLOT), Box::new(|| Some(b"synth".to_vec())));
    side.states.insert((a, 3), Box::new(|| Some(b"a3".to_vec())));
    side.states.insert((a, 4), Box::new(|| None));
    let want = vec![(a, 3, b"a3".to_vec()), (a, soundcraft_mix::INSTRUMENT_SLOT, b"synth".to_vec()), (b, 0, b"b0".to_vec())];
    assert_eq!(side.capture_states(), want);
    side.forget((a, 3));
    assert_eq!(side.capture_states(), want[1..]);
}

struct Window;

impl PluginEditor for Window {
    fn open(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn close(&mut self) {}
    fn is_open(&self) -> bool {
        false
    }
    fn idle(&mut self) -> Vec<(String, f32)> {
        Vec::new()
    }
}

#[test]
fn a_refused_state_is_kept_until_the_editor_opens() {
    let mut side = test_side(48_000.0);
    let k = (TrackId(1), 0);
    side.states.insert(k, Box::new(|| Some(b"defaults".to_vec())));
    side.editors.insert(k, Box::new(Window));
    side.refused.insert(k);
    assert!(side.capture_states().is_empty());
    side.open_editor(k).unwrap();
    assert_eq!(side.capture_states(), vec![(TrackId(1), 0, b"defaults".to_vec())]);
}

/// An editor whose plugin said its state changed (CLAP `state.mark_dirty`).
struct Dirty(bool);

impl PluginEditor for Dirty {
    fn open(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn close(&mut self) {}
    fn is_open(&self) -> bool {
        true
    }
    fn idle(&mut self) -> Vec<(String, f32)> {
        Vec::new()
    }
    fn state_changed(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }
}

#[test]
fn changed_states_are_reported_once_and_read_for_the_session() {
    let mut side = test_side(48_000.0);
    let (a, b) = (TrackId(1), TrackId(2));
    let synth = (a, soundcraft_mix::INSTRUMENT_SLOT);
    side.states.insert((a, 0), Box::new(|| Some(b"a0".to_vec())));
    side.states.insert((a, 1), Box::new(|| Some(b"a1".to_vec())));
    side.states.insert(synth, Box::new(|| Some(b"synth".to_vec())));
    side.states.insert((b, 0), Box::new(|| Some(b"b0".to_vec())));
    side.editors.insert((a, 0), Box::new(Dirty(true)));
    side.editors.insert((a, 1), Box::new(Dirty(false)));
    side.editors.insert((b, 0), Box::new(Dirty(true)));
    side.refused.insert((b, 0));
    let mut changed = side.changed_states();
    changed.sort();
    assert_eq!(changed, vec![(a, 0), (b, 0)]);
    assert!(side.changed_states().is_empty(), "each change is reported once");
    let want = vec![(a, 0, b"a0".to_vec()), (a, soundcraft_mix::INSTRUMENT_SLOT, b"synth".to_vec())];
    assert_eq!(side.capture_states_of(&[(a, 0), synth, (b, 0)]), want);
}

#[test]
fn a_state_marked_changed_while_loading_is_not_a_change() {
    let mut side = test_side(48_000.0);
    let k = (TrackId(1), 0);
    side.states.insert(k, Box::new(|| Some(b"loaded".to_vec())));
    side.add_editor(k, Box::new(Dirty(true)));
    assert!(side.changed_states().is_empty());
}

#[test]
fn side_creates_each_missing_instance_once() {
    use soundcraft_model::{ChannelFormat, Insert, TrackKind};
    let mut side = test_side(48_000.0);
    let mut s = soundcraft_model::Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
    s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("gain"));
    assert!(side.prepare(&s).is_empty(), "the plugin is not installed; built-ins stay with the engine");
    assert_eq!(side.shadow.get(&(t, 0)), Some(&("clap:does.not.exist".to_string(), 1, false)));
    assert!(side.prepare(&s).is_empty());
    assert_eq!(side.shadow.len(), 1, "failures are remembered, not retried every update");
    s.track_mut(t).unwrap().mixer.inserts[0] = None;
    side.prepare(&s);
    assert!(side.shadow.is_empty());
}

#[test]
fn replaced_sessions_are_freed_on_the_ui_thread() {
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let (gtx, grx) = Spsc::new(OLD_SESSIONS).split();
    let mut st = test_state(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
    let mut side = Side::new(48_000.0, grx, st.test_old.take().unwrap());
    let mut out = vec![0.0f32; 256];
    let mut watched = Vec::new();
    for _ in 0..3 * OLD_SESSIONS {
        let s = Arc::new(soundcraft_model::Session::default());
        watched.push(Arc::downgrade(&s));
        side.collect_garbage();
        tx.push(Cmd::Session(s, Vec::new())).unwrap();
        st.fill(&mut out, 2);
        let replaced = watched.len().checked_sub(2).and_then(|i| watched.get(i));
        assert!(replaced.is_none_or(|w| w.strong_count() == 1), "freed on the audio thread");
    }
    side.collect_garbage();
    assert_eq!(watched.iter().filter(|w| w.strong_count() > 0).count(), 1, "only the live snapshot is left");
}

#[test]
fn looping_wraps() {
    let s = Arc::new(soundcraft_model::Session::default());
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let mut st = test_state(rx, Arc::clone(&shared), s, 48_000.0, Spsc::new(OLD_SESSIONS).split().0);
    tx.push(Cmd::Play { from: 0, end: None, looped: Some(Range::new(0, 1000)) }).unwrap();
    let mut out = vec![0.0f32; 20_000];
    st.fill(&mut out, 2);
    let p = shared.position.load(Ordering::Relaxed);
    assert!(p <= 1000, "position {p} should stay inside the loop");
    assert!(shared.playing.load(Ordering::Relaxed));
}

#[test]
fn locate_while_playing_drops_the_old_position_audio() {
    use soundcraft_model::{ChannelFormat, Clip, Insert, SourceAudio, SourceId, TrackKind};
    for (plugin, params) in [("compressor", &[("lookahead", 5.0), ("ratio", 1.0)][..]), ("room_reverb", &[][..])] {
        let mut s = soundcraft_model::Session::default();
        s.edit.click = false;
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.pool.insert(
            SourceId(5),
            Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 4800]] })),
        );
        let id = s.new_clip_id();
        let tr = s.track_mut(t).unwrap();
        tr.playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 4800));
        let mut ins = Insert::new(plugin);
        for (k, v) in params {
            ins.params.insert((*k).into(), *v);
        }
        tr.mixer.inserts[0] = Some(ins);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicParam::new(1.0),
            output_gain: AtomicParam::new(1.0),
        });
        let (mut tx, rx) = Spsc::new(COMMANDS).split();
        let mut st = test_state(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, Spsc::new(OLD_SESSIONS).split().0);
        tx.push(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 2 * 512];
        let mut before = 0.0f32;
        for _ in 0..9 {
            st.fill(&mut out, 2);
            before = out.iter().fold(before, |m, x| m.max(x.abs()));
        }
        assert!(before > 0.1, "{plugin}: the clip plays before the locate");
        tx.push(Cmd::Locate(1_000_000)).unwrap();
        for _ in 0..8 {
            st.fill(&mut out, 2);
            assert!(out.iter().all(|x| x.abs() < 1e-6), "{plugin}: audio from before the locate");
        }
    }
}

#[test]
fn armed_tracks_are_heard_and_metered_after_playback_stops() {
    use soundcraft_model::{ChannelFormat, TrackKind};
    let mut s = soundcraft_model::Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    s.track_mut(t).unwrap().mixer.record_arm = true;
    let shared = Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicParam::new(1.0),
        output_gain: AtomicParam::new(1.0),
    });
    let (mut tx, rx) = Spsc::new(COMMANDS).split();
    let mut st = test_state(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, Spsc::new(OLD_SESSIONS).split().0);
    let ring = record::InputRing::new(1, 48_000);
    tx.push(Cmd::Input(Arc::clone(&ring))).unwrap();
    tx.push(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
    let mut out = vec![0.0f32; 4096];
    st.fill(&mut out, 2);
    assert!(!shared.playing.load(Ordering::Relaxed));
    ring.push(&[0.5; 4096]);
    st.fill(&mut out, 2);
    assert!(out.iter().any(|x| x.abs() > 0.1), "the input is heard");
    assert!(test_meters(&mut st).tracks.get(&t).is_some_and(|m| m.peak[0] > 0.1), "and metered");
}
