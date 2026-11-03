//! Realtime playback through the system audio device (cpal: CoreAudio, WASAPI, ALSA/Pulse/
//! PipeWire via ALSA, JACK, WebAudio).
//!
//! The UI thread owns a [`Player`]; the audio callback owns the [`MixEngine`] and a snapshot of
//! the session (`Arc<Session>`) that the UI swaps whenever the document changes. Commands and
//! meters travel through bounded SPSC queues; the callback never blocks. Without a device the player
//! falls back to a silent clock thread so the transport, meters and automation still run.
//!
//! Surround: the device is opened with as many output channels as it offers, up to the width of
//! the session's main format. Main channels (SMPTE/WAV order) go to device channels in order; when
//! the device has fewer channels than the main mix, surround is folded down to stereo (ITU-R
//! BS.775). A mono device receives the average of the stereo channels, before output clipping.
//!
//! Rate conversion and varispeed use rubato's windowed-sinc phase tables, with 128 source
//! frames of group delay and preallocated cutoff banks selected at callback boundaries.
//! Transport positions count source frames, not delayed device frames. Natural playback ends
//! flush the filter tail without advancing transport; Stop and Locate discard it. Equal rates
//! at normal speed bypass the filter with no added latency.
//!
//! Plugins and threads: hosted third-party plugins (CLAP/VST3) are created, given their stored
//! state and prepared on the thread that calls [`Player::update_session`] (the UI thread), and
//! travel to the audio thread together with the session snapshot that needs them; the mix
//! engine adopts them (see `soundcraft_mix`'s crate docs). Instances the engine retires, and the
//! session snapshots the callback replaces, come back on return queues and are dropped on the UI
//! thread ([`Player::idle`]). Plugin editors are driven, and plugin states for saving read
//! ([`Player::capture_states`]), on the UI thread
//! through handles taken from each instance before it leaves ([`Player::open_editor`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod devices;
pub mod record;

pub use devices::AudioConfig;

use craft_audio_rt::{AtomicParam, Spsc, SpscConsumer, SpscProducer, StallDetector, Triple, TripleEdit};
use soundcraft_dsp::{Plugin, PluginEditor, PluginStateReader};
use soundcraft_mix::{ClipLookup, MAX_CHANNELS, MixEngine, PreparedInstance, StripMeter, itu_stereo_fold, main_channels};
use soundcraft_model::{Session, TrackId};
use soundcraft_time::{Range, Samples};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

const BLOCK: usize = 512;

/// Room for replaced snapshots on their way back; `update_session` empties it before each send.
const OLD_SESSIONS: usize = 128;
const COMMANDS: usize = 4096;

/// The realtime audio thread marker lives in the mix engine, which carries it into the strips it
/// renders on worker threads; the cpal callbacks here set it.
pub use soundcraft_mix::{mark_audio_thread, on_audio_thread};

mod callback;
mod resample;
mod stream;

#[cfg(not(target_arch = "wasm32"))]
use callback::guarded_block;
use callback::{AudioPorts, AudioState, Fault, MeterUi, Side};
#[cfg(test)]
use callback::{STALL, copy_meter_snapshot, take_meter_snapshot};
#[cfg(test)]
use stream::pick_output_config;
use stream::{Opened, build_output, first_started, open_device, start_stream};

/// Meter values for the UI: the highest peak (and gain reduction) of every block rendered since
/// the UI last took them with [`Player::take_meters`]. The audio thread renders blocks faster than
/// the UI draws (about 94 blocks a second at 512 frames and 48 kHz against 20-60 polls), so
/// keeping only the latest block would miss most transients.
#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub tracks: HashMap<TrackId, StripMeter>,
    pub main: StripMeter,
    /// Something was written since the UI last took the values.
    fresh: bool,
    clear_previous: bool,
}

impl MeterSnapshot {
    /// Fold one rendered block's meters in (max). Allocation-free once the tracks are known.
    fn accumulate(&mut self, main: &StripMeter, tracks: &HashMap<TrackId, StripMeter>) {
        self.main.accumulate(main);
        for (k, v) in tracks {
            match self.tracks.get_mut(k) {
                Some(d) => d.accumulate(v),
                None => {
                    self.tracks.insert(*k, v.clone());
                }
            }
        }
        self.fresh = true;
    }

    /// Start a new accumulation period, keeping every entry and its storage.
    fn reset_levels(&mut self) {
        self.main.reset_levels();
        self.tracks.values_mut().for_each(StripMeter::reset_levels);
        self.fresh = false;
        self.clear_previous = false;
    }

    /// Nothing is playing or monitored: every meter reads silence. Levels are zeroed in place,
    /// so starting again does not allocate the tracks' entries anew.
    fn silence(&mut self) {
        self.reset_levels();
        self.fresh = true;
        self.clear_previous = true;
    }

    fn for_session(session: &Session) -> Self {
        let main = StripMeter { peaks: vec![0.0; main_channels(session)], ..StripMeter::default() };
        let tracks = session
            .tracks
            .iter()
            .map(|track| {
                let channels = match track.kind {
                    soundcraft_model::TrackKind::Master => main_channels(session),
                    soundcraft_model::TrackKind::Instrument => track.channels().clamp(2, MAX_CHANNELS),
                    _ => track.channels().clamp(1, MAX_CHANNELS),
                };
                (track.id, StripMeter { peaks: vec![0.0; channels], ..StripMeter::default() })
            })
            .collect();
        Self { tracks, main, fresh: false, clear_previous: false }
    }

    /// Drop the entries of deleted tracks.
    fn forget(&mut self, gone: &[TrackId]) {
        for id in gone {
            self.tracks.remove(id);
        }
    }
}

/// Plugin states read from the live instances: `(track, slot or INSTRUMENT_SLOT, state)`.
pub type PluginStates = Vec<(TrackId, usize, Vec<u8>)>;

/// A parameter edit made in a plugin's own editor: `(track, slot, param id, value)`.
pub type EditorEdit = (TrackId, usize, String, f32);

enum Cmd {
    /// A new snapshot plus the third-party instances it needs (created on the UI thread).
    Session(Arc<Session>, Vec<PreparedInstance>),
    Input(Arc<record::InputRing>),
    Recording(bool),
    Play {
        from: Samples,
        end: Option<Samples>,
        looped: Option<Range>,
    },
    Stop,
    Locate(Samples),
    /// Tracks the latest session no longer has (worked out on the UI thread): their meters go.
    Forget(Vec<TrackId>),
}

#[cfg(test)]
impl std::fmt::Debug for Cmd {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("playback command")
    }
}

struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    speed: AtomicParam,
    output_gain: AtomicParam,
}

struct Control {
    tx: SpscProducer<Cmd>,
    pending: std::collections::VecDeque<Cmd>,
    index: TripleEdit<ClipLookup>,
    session_adopted: Arc<AtomicBool>,
    session_published: bool,
}

impl Control {
    fn send(&mut self, cmd: Cmd) {
        self.pending.push_back(cmd);
        self.flush();
    }

    fn flush(&mut self) {
        while let Some(cmd) = self.pending.front() {
            if self.tx.slots() == 0 {
                break;
            }
            if let Cmd::Session(session, _) = cmd
                && !self.session_published
            {
                if !self.session_adopted.load(Ordering::Acquire) || self.tx.slots() == 0 {
                    break;
                }
                *self.index.edit_mut() = ClipLookup::build(session);
                if !self.index.publish() {
                    break;
                }
                self.session_adopted.store(false, Ordering::Release);
                self.session_published = true;
            }
            let Some(cmd) = self.pending.pop_front() else { break };
            if let Err(cmd) = self.tx.push(cmd) {
                self.pending.push_front(cmd);
                break;
            }
            self.session_published = false;
        }
    }
}

/// What stopped the audio output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTrouble {
    /// A block panicked; the output stays silent.
    Bug,
    /// The device went away, or the stream was invalidated and must be rebuilt.
    Gone,
    /// The device stopped asking for audio while playing.
    Stalled,
}

/// The transport and audio device.
pub struct Player {
    shared: Arc<Shared>,
    fault: Arc<Fault>,
    /// Observed on the UI thread; the callback only increments the heartbeat.
    #[cfg(not(target_arch = "wasm32"))]
    watch: Mutex<StallDetector>,
    stall_count: std::sync::atomic::AtomicU64,
    stalled_reported: AtomicBool,
    tx: Mutex<Control>,
    side: Mutex<Side>,
    meters: Mutex<MeterUi>,
    #[cfg(not(target_arch = "wasm32"))]
    _stream: Option<cpal::Stream>,
    #[cfg(target_arch = "wasm32")]
    _stream: Option<cpal::Stream>,
    pub device_name: String,
    pub device_rate: u32,
    /// Output channels the device was opened with (0 for the silent clock).
    pub device_channels: u16,
    /// The device buffer in frames when one was chosen (`None`: the device default).
    pub device_buffer: Option<u32>,
    /// True when no audio device could be opened (silent clock).
    pub silent: bool,
}

impl Player {
    /// Open the default output device. Never fails: without a device it runs a silent clock.
    pub fn new(session: Arc<Session>) -> Player {
        Player::with_config(session, &AudioConfig::default())
    }

    /// Open the output device `cfg` chooses (system, device, rate, buffer size), falling back to the
    /// defaults for whatever is unavailable. Never fails: without a device it runs a silent clock.
    pub fn with_config(session: Arc<Session>, cfg: &AudioConfig) -> Player {
        let p = Player::open(Arc::clone(&session), cfg);
        // Hand the initial session's third-party plugins over.
        p.update_session(session);
        p
    }

    /// What stopped the audio output, once: a bug in the engine, or the device went away or
    /// stopped asking for audio while playing.
    pub fn trouble(&self) -> Option<OutputTrouble> {
        #[cfg(not(target_arch = "wasm32"))]
        let stalled = {
            let playing = self.shared.playing.load(Ordering::Relaxed) && !self.silent;
            self.watch.lock().unwrap_or_else(PoisonError::into_inner).observe(playing, std::time::Instant::now())
        };
        #[cfg(target_arch = "wasm32")]
        let stalled = false;
        if stalled {
            if !self.stalled_reported.swap(true, Ordering::Relaxed) {
                self.stall_count.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            self.stalled_reported.store(false, Ordering::Relaxed);
        }
        self.fault.trouble(stalled)
    }

    pub fn stall_count(&self) -> u64 {
        self.stall_count.load(Ordering::Relaxed)
    }

    fn send(&self, cmd: Cmd) {
        #[cfg(target_arch = "wasm32")]
        if self.silent {
            return;
        }
        self.tx.lock().unwrap_or_else(PoisonError::into_inner).send(cmd);
    }

    fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Open the output device for `session`, trying the requested buffer size before the device default, then build
    /// the shared player state. If no configuration starts or the device cannot open, fall back to a silent clock at
    /// the session rate.
    fn open(session: Arc<Session>, cfg: &AudioConfig) -> Player {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicParam::new(1.0),
            output_gain: AtomicParam::new(1.0),
        });
        let want = session.sample_rate.hz();
        let want_ch = u16::try_from(main_channels(&session)).unwrap_or(2);
        let side_rate = session.sample_rate.as_f64() as f32;
        let new_side = |garbage: SpscConsumer<Vec<Box<dyn Plugin>>>, old: SpscConsumer<Arc<Session>>| {
            let mut side = Side::new(side_rate, garbage, old);
            side.gone_tracks(&session);
            Mutex::new(side)
        };
        match open_device(want, want_ch, cfg) {
            Ok((device, config, format, name)) => {
                // The chosen buffer size first; if the device refuses it, its default.
                let mut attempts = vec![config];
                if config.buffer_size != cpal::BufferSize::Default {
                    attempts.push(cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..config });
                }
                let opened = first_started(
                    attempts,
                    |config: &cpal::StreamConfig| -> Result<Opened, cpal::Error> {
                        let rate = config.sample_rate;
                        let channels = usize::from(config.channels);
                        let (tx, rx) = Spsc::new(COMMANDS).split();
                        let (gtx, grx) = Spsc::new(OLD_SESSIONS).split();
                        let (otx, orx) = Spsc::new(OLD_SESSIONS).split();
                        let (mtx, mrx) = Spsc::new(8).split();
                        let (mut recycle_tx, recycle_rx) = Spsc::new(8).split();
                        for _ in 0..8 {
                            let _ = recycle_tx.push(MeterSnapshot::for_session(&session));
                        }
                        let (watch, beat) = StallDetector::new(callback::STALL, std::time::Instant::now());
                        let (index, audio_index) = Triple::new(ClipLookup::default(), ClipLookup::build(&session), ClipLookup::default()).split();
                        let session_adopted = Arc::new(AtomicBool::new(true));
                        let state = AudioState::new(
                            rx,
                            Arc::clone(&shared),
                            Arc::clone(&session),
                            f64::from(rate),
                            AudioPorts {
                                garbage: gtx,
                                old_sessions: otx,
                                meter_tx: mtx,
                                meter_recycle: recycle_rx,
                                beat,
                                index: audio_index,
                                session_adopted: Arc::clone(&session_adopted),
                            },
                        );
                        let fault = Arc::new(Fault::default());
                        let stream = match format {
                            cpal::SampleFormat::I32 => build_output::<i32>(&device, *config, state, channels, &fault, &shared),
                            cpal::SampleFormat::I16 => build_output::<i16>(&device, *config, state, channels, &fault, &shared),
                            _ => build_output::<f32>(&device, *config, state, channels, &fault, &shared),
                        }?;
                        Ok(Opened { stream, tx, grx, orx, mrx, recycle_tx, watch, index, session_adopted, fault })
                    },
                    |opened| start_stream(&opened.stream),
                );
                match opened {
                    Some((config, Opened { stream, tx, grx, orx, mrx, recycle_tx, watch, index, session_adopted, fault })) => {
                        #[cfg(target_arch = "wasm32")]
                        let _ = &watch;
                        let rate = config.sample_rate;
                        let channels = usize::from(config.channels);
                        let buffer = match config.buffer_size {
                            cpal::BufferSize::Fixed(n) => Some(n),
                            cpal::BufferSize::Default => None,
                        };
                        log::info!(
                            "audio device: {name} @ {rate} Hz, {channels} ch, {format}, buffer {}",
                            buffer.map_or("default".to_string(), |n| n.to_string())
                        );
                        Player {
                            shared,
                            fault,
                            #[cfg(not(target_arch = "wasm32"))]
                            watch: Mutex::new(watch),
                            stall_count: std::sync::atomic::AtomicU64::new(0),
                            stalled_reported: AtomicBool::new(false),
                            tx: Mutex::new(Control { tx, pending: Default::default(), index, session_adopted, session_published: false }),
                            side: new_side(grx, orx),
                            meters: Mutex::new(MeterUi::new(mrx, recycle_tx)),
                            _stream: Some(stream),
                            device_name: name,
                            device_rate: rate,
                            device_channels: config.channels,
                            device_buffer: buffer,
                            silent: false,
                        }
                    }
                    None => {
                        log::warn!("no usable audio stream; using a silent clock");
                        Player::silent_clock(shared, want, new_side)
                    }
                }
            }
            Err(e) => {
                log::warn!("no audio output: {e}; using a silent clock");
                Player::silent_clock_with(shared, Arc::clone(&session), new_side)
            }
        }
    }

    fn silent_clock(
        shared: Arc<Shared>,
        rate: u32,
        new_side: impl FnOnce(SpscConsumer<Vec<Box<dyn Plugin>>>, SpscConsumer<Arc<Session>>) -> Mutex<Side>,
    ) -> Player {
        let s = Arc::new(Session::new("Untitled", soundcraft_time::SampleRate::new(rate).unwrap_or_default()));
        Player::silent_clock_with(shared, s, new_side)
    }

    fn silent_clock_with(
        shared: Arc<Shared>,
        session: Arc<Session>,
        new_side: impl FnOnce(SpscConsumer<Vec<Box<dyn Plugin>>>, SpscConsumer<Arc<Session>>) -> Mutex<Side>,
    ) -> Player {
        let (tx, rx) = Spsc::new(COMMANDS).split();
        let (gtx, grx) = Spsc::new(OLD_SESSIONS).split();
        let (otx, orx) = Spsc::new(OLD_SESSIONS).split();
        let (mtx, mrx) = Spsc::new(8).split();
        let (mut recycle_tx, recycle_rx) = Spsc::new(8).split();
        for _ in 0..8 {
            let _ = recycle_tx.push(MeterSnapshot::for_session(&session));
        }
        let (watch, beat) = StallDetector::new(callback::STALL, std::time::Instant::now());
        let (index, audio_index) = Triple::new(ClipLookup::default(), ClipLookup::build(&session), ClipLookup::default()).split();
        let session_adopted = Arc::new(AtomicBool::new(true));
        let side = new_side(grx, orx);
        let rate = session.sample_rate.hz();
        let state = AudioState::new(
            rx,
            Arc::clone(&shared),
            session,
            f64::from(rate),
            AudioPorts {
                garbage: gtx,
                old_sessions: otx,
                meter_tx: mtx,
                meter_recycle: recycle_rx,
                beat,
                index: audio_index,
                session_adopted: Arc::clone(&session_adopted),
            },
        );
        #[cfg(not(target_arch = "wasm32"))]
        let mut state = state;
        let fault = Arc::new(Fault::default());
        let failed = Arc::clone(&fault);
        #[cfg(not(target_arch = "wasm32"))]
        let spawn = std::thread::Builder::new().name("soundcraft-clock".into()).spawn(move || {
            let mut buf = vec![0.0f32; 4096];
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let now = std::time::Instant::now();
                let frames = ((now - last).as_secs_f64() * state.device_rate) as usize;
                last = now;
                let n = (frames * 2).min(buf.len());
                if let Some(b) = buf.get_mut(..n) {
                    let shared = Arc::clone(&state.shared);
                    guarded_block(&failed, &shared, b, |b| state.fill(b, 2));
                }
                if Arc::strong_count(&state.shared) <= 1 {
                    break;
                }
            }
        });
        #[cfg(not(target_arch = "wasm32"))]
        if let Err(e) = spawn {
            log::warn!("clock thread failed: {e}");
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (state, failed, watch);
        Player {
            shared,
            fault,
            #[cfg(not(target_arch = "wasm32"))]
            watch: Mutex::new(watch),
            stall_count: std::sync::atomic::AtomicU64::new(0),
            stalled_reported: AtomicBool::new(false),
            tx: Mutex::new(Control { tx, pending: Default::default(), index, session_adopted, session_published: false }),
            side,
            meters: Mutex::new(MeterUi::new(mrx, recycle_tx)),
            _stream: None,
            device_name: "No audio device (silent)".into(),
            device_rate: rate,
            device_channels: 0,
            device_buffer: None,
            silent: true,
        }
    }

    /// Sends a new session snapshot to the audio thread, with freshly created (state-restored,
    /// prepared) instances of any third-party plugin it newly needs. Call on the UI thread.
    pub fn update_session(&self, s: Arc<Session>) {
        let (instances, gone) = {
            let mut side = self.side();
            (side.prepare(&s), side.gone_tracks(&s))
        };
        self.send(Cmd::Session(s, instances));
        if !gone.is_empty() {
            self.send(Cmd::Forget(gone));
        }
    }

    /// Reads the state of every live third-party plugin, for saving the session.
    pub fn capture_states(&self) -> PluginStates {
        self.side().capture_states()
    }

    /// The plugins that said their state changed since the last call, so the session can keep
    /// changes made only in plugin editors.
    pub fn changed_states(&self) -> Vec<(TrackId, usize)> {
        self.side().changed_states()
    }

    /// Reads the state of the plugins in `slots`.
    pub fn capture_states_of(&self, slots: &[(TrackId, usize)]) -> PluginStates {
        self.side().capture_states_of(slots)
    }

    /// Per-UI-frame housekeeping: drops retired instances and services plugin editors. Returns
    /// the parameter edits made in open editors since the last call.
    pub fn idle(&self) -> Vec<EditorEdit> {
        self.tx.lock().unwrap_or_else(PoisonError::into_inner).flush();
        let mut side = self.side();
        side.collect_garbage();
        let mut out = Vec::new();
        for (&(t, slot), e) in side.editors.iter_mut() {
            for (id, v) in e.idle() {
                out.push((t, slot, id, v));
            }
        }
        out
    }

    /// True when the plugin in this slot has its own editor.
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.contains_key(&(track, slot))
    }

    /// Opens the plugin's own editor window. Main thread only.
    pub fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
        self.side().open_editor((track, slot))
    }

    pub fn close_editor(&self, track: TrackId, slot: usize) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.close();
        }
    }

    /// Closes every plugin editor window (main thread). Call before replacing the player: its
    /// instances may be dropped on the audio thread, where editor windows cannot be closed.
    pub fn close_editors(&self) {
        for e in self.side().editors.values_mut() {
            e.close();
        }
    }

    pub fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.get(&(track, slot)).is_some_and(|e| e.is_open())
    }

    pub fn any_editor_open(&self) -> bool {
        self.side().editors.values().any(|e| e.is_open())
    }

    /// Shows a parameter change made elsewhere in the plugin's open editor.
    pub fn editor_set_param(&self, track: TrackId, slot: usize, id: &str, value: f32) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.set_param(id, value);
        }
    }

    /// The plugin's own text for a parameter value when SoundCraft's format would be wrong for it
    /// (see `PluginEditor::value_text`). Main thread only.
    pub fn editor_value_text(&self, track: TrackId, slot: usize, id: &str, value: f32) -> Option<String> {
        self.side().editors.get_mut(&(track, slot))?.value_text(id, value)
    }

    /// Start playback at `from`, stopping at `end`, or looping `looped`.
    pub fn play(&self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        self.shared.position.store(from, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        self.send(Cmd::Play { from, end, looped });
    }

    /// Feed live input (from a `record::Recorder`) into the mix for input monitoring.
    pub fn set_input(&self, ring: Arc<record::InputRing>) {
        self.send(Cmd::Input(ring));
    }

    pub fn set_recording(&self, on: bool) {
        self.send(Cmd::Recording(on));
    }

    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.send(Cmd::Stop);
    }

    pub fn locate(&self, at: Samples) {
        self.shared.position.store(at, Ordering::Relaxed);
        self.send(Cmd::Locate(at));
    }

    pub fn set_speed(&self, speed: f32) {
        let v = if speed.is_finite() { (speed.clamp(0.1, 4.0) * 1000.0) as u32 } else { 1000 };
        self.shared.speed.store(v as f32 / 1000.0);
    }

    /// Playback rate last requested, 1.0 at full speed.
    pub fn speed(&self) -> f32 {
        self.shared.speed.load()
    }

    /// Set the final device gain, ramped across the next callback to avoid a step.
    /// Track faders and automation remain part of the session snapshot and mix engine.
    pub fn set_output_gain(&self, gain: f32) {
        if gain.is_finite() {
            self.shared.output_gain.store(gain.clamp(0.0, 4.0));
        }
    }

    pub fn position(&self) -> Samples {
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// A copy of the meters accumulated since the last [`Player::take_meters`], leaving them in
    /// place (for inspection; the UI takes them instead).
    pub fn meters(&self) -> MeterSnapshot {
        let mut snapshot = MeterSnapshot::default();
        self.take_meters(&mut snapshot);
        snapshot
    }

    /// Take the meters accumulated since the last call into reusable UI storage and start a new
    /// period: `out` gets the highest peak of every block rendered in between. If no block was
    /// rendered since (a large device buffer renders several blocks per callback), `out` keeps
    /// the previous values rather than dropping to silence. Call it from one place only (two
    /// readers would split the peaks between them). Once track/channel counts are stable this
    /// performs no allocation. Holding the UI lock never blocks the audio thread; blocks rendered
    /// meanwhile reach the next call through the bounded queue.
    pub fn take_meters(&self, out: &mut MeterSnapshot) {
        self.meters.lock().unwrap_or_else(PoisonError::into_inner).take(out);
        let side = self.side();
        out.tracks.retain(|id, _| side.tracks.contains(id));
    }
}

#[cfg(test)]
mod tests;
