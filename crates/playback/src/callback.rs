use super::{
    Arc, AtomicBool, BLOCK, BTreeMap, Cmd, HashMap, HashSet, MAX_CHANNELS, MeterSnapshot, MixEngine, Ordering, OutputTrouble, Plugin, PluginEditor,
    PluginStateReader, PluginStates, PreparedInstance, Range, Samples, Session, Shared, SpscConsumer, SpscProducer, TrackId, itu_stereo_fold,
    main_channels, record,
};
use craft_audio_rt::{AudioCallback, PanicGuard, StallBeat, TripleAudio};
use soundcraft_mix::ClipLookup;

/// How the audio output is doing, for the UI.
#[derive(Default)]
pub(crate) struct Fault {
    /// A block panicked: output stays silent from then on.
    guard: PanicGuard,
    /// The device went away or the stream was invalidated: only a reconnect can restore audio.
    pub(crate) gone: AtomicBool,
    /// Blocks so far, to tell a device that stopped asking for audio.
    /// The UI has been told.
    reported: AtomicBool,
}

impl Fault {
    /// What stopped the output, once.
    pub(crate) fn trouble(&self, stalled: bool) -> Option<OutputTrouble> {
        let trouble = if self.guard.failed() {
            OutputTrouble::Bug
        } else if self.gone.load(Ordering::Relaxed) {
            OutputTrouble::Gone
        } else if stalled {
            OutputTrouble::Stalled
        } else {
            return None;
        };
        (!self.reported.swap(true, Ordering::Relaxed)).then_some(trouble)
    }
}

/// How long a playing output may go without a block before it counts as stalled.
pub(crate) const STALL: std::time::Duration = std::time::Duration::from_secs(2);

/// Fill one block with `fill`. If it panics, the block and every later one are silent and the
/// transport stops, rather than the panic taking the app down.
pub(crate) fn guarded_block(fault: &Fault, shared: &Shared, out: &mut [f32], fill: impl FnOnce(&mut [f32])) {
    struct Once<F>(Option<F>);
    impl<F: FnOnce(&mut [f32])> AudioCallback for Once<F> {
        fn render(&mut self, output: &mut [f32]) {
            if let Some(fill) = self.0.take() {
                fill(output);
            }
        }
    }
    let mut callback = Once(Some(fill));
    if !fault.guard.render(&mut callback, out) {
        shared.playing.store(false, Ordering::Relaxed);
        out.fill(0.0);
    }
}

/// State owned by the audio callback.
pub(crate) struct AudioState {
    rx: SpscConsumer<Cmd>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) session: Arc<Session>,
    pub(crate) mix: MixEngine,
    pos: Samples,
    playing: bool,
    end: Option<Samples>,
    looped: Option<Range>,
    /// Rendered frames waiting to be consumed (planar, main channels) and the fractional read index.
    pending: Vec<Vec<f32>>,
    /// Stereo fold-down gains per main channel (used when the device has fewer channels).
    fold: [(f32, f32); MAX_CHANNELS],
    pub(crate) pending_len: usize,
    read: f64,
    resampler: super::resample::SincResampler,
    consumed: usize,
    tail_remaining: usize,
    pub(crate) device_rate: f64,
    input: Option<Arc<record::InputRing>>,
    recording: bool,
    /// Retired plugin instances go back to the UI thread to be dropped there.
    garbage: SpscProducer<Vec<Box<dyn Plugin>>>,
    old_sessions: SpscProducer<Arc<Session>>,
    deferred_sessions: Vec<Arc<Session>>,
    deferred_plugins: Vec<Box<dyn Plugin>>,
    meter_tx: SpscProducer<MeterSnapshot>,
    meter_recycle: SpscConsumer<MeterSnapshot>,
    beat: StallBeat,
    pub(crate) index: TripleAudio<ClipLookup>,
    session_adopted: Arc<AtomicBool>,
    #[cfg(test)]
    pub(crate) test_meter_ui: Option<MeterUi>,
    #[cfg(test)]
    pub(crate) test_old: Option<SpscConsumer<Arc<Session>>>,
    /// Meters awaiting a free slot in the UI queue; retain peaks until they can be published.
    pub(crate) unsent: MeterSnapshot,
    forgotten: Vec<TrackId>,
    output_gain: f32,
}

fn new_engine(sr: f32) -> MixEngine {
    let mut m = MixEngine::new(sr, BLOCK);
    m.set_external_instances(true);
    m.metronome = true;
    m
}

/// Preallocated, single-owner endpoints handed to the callback when the device opens.
pub(crate) struct AudioPorts {
    pub(crate) garbage: SpscProducer<Vec<Box<dyn Plugin>>>,
    pub(crate) old_sessions: SpscProducer<Arc<Session>>,
    pub(crate) meter_tx: SpscProducer<MeterSnapshot>,
    pub(crate) meter_recycle: SpscConsumer<MeterSnapshot>,
    pub(crate) beat: StallBeat,
    pub(crate) index: TripleAudio<ClipLookup>,
    pub(crate) session_adopted: Arc<AtomicBool>,
}

impl AudioState {
    pub(crate) fn new(rx: SpscConsumer<Cmd>, shared: Arc<Shared>, session: Arc<Session>, device_rate: f64, ports: AudioPorts) -> Self {
        let sr = session.sample_rate.as_f64() as f32;
        let nch = main_channels(&session);
        let unsent = MeterSnapshot::for_session(&session);
        AudioState {
            garbage: ports.garbage,
            old_sessions: ports.old_sessions,
            deferred_sessions: Vec::new(),
            deferred_plugins: Vec::new(),
            meter_tx: ports.meter_tx,
            meter_recycle: ports.meter_recycle,
            beat: ports.beat,
            index: ports.index,
            session_adopted: ports.session_adopted,
            #[cfg(test)]
            test_meter_ui: None,
            #[cfg(test)]
            test_old: None,
            fold: fold_gains(&session),
            rx,
            shared,
            mix: new_engine(sr),
            session,
            pos: 0,
            playing: false,
            end: None,
            looped: None,
            pending: vec![vec![0.0; BLOCK]; nch],
            pending_len: 0,
            read: 0.0,
            resampler: super::resample::SincResampler::new(MAX_CHANNELS),
            consumed: 0,
            tail_remaining: 0,
            device_rate,
            input: None,
            recording: false,
            unsent,
            forgotten: Vec::new(),
            output_gain: 1.0,
        }
    }

    fn drain(&mut self) {
        let mut changed = false;
        while let Some(c) = self.rx.pop() {
            match c {
                Cmd::Input(r) => self.input = Some(r),
                Cmd::Forget(gone) => {
                    self.unsent.forget(&gone);
                    self.forgotten.extend(gone);
                }
                Cmd::Recording(r) => self.recording = r,
                Cmd::Session(s, instances) => {
                    self.index.poll();
                    changed = true;
                    let rate_changed = (s.sample_rate.as_f64() - self.session.sample_rate.as_f64()).abs() > 0.5;
                    if rate_changed {
                        let mut old = std::mem::replace(&mut self.mix, new_engine(s.sample_rate.as_f64() as f32));
                        self.deferred_plugins.extend(old.drain_plugins());
                    }
                    self.mix.adopt(instances);
                    // The main format changed (a document edit, not steady state): resize.
                    let nch = main_channels(&s);
                    let channels_changed = self.pending.len() != nch;
                    if channels_changed {
                        self.pending = vec![vec![0.0; BLOCK]; nch];
                    }
                    if rate_changed || channels_changed {
                        self.pending_len = 0;
                        self.read = 0.0;
                        self.consumed = 0;
                        self.tail_remaining = 0;
                        self.resampler.reset();
                    }
                    self.fold = fold_gains(&s);
                    let old = std::mem::replace(&mut self.session, s);
                    self.deferred_sessions.push(old);
                }
                Cmd::Play { from, end, looped } => {
                    self.pos = from;
                    self.end = end;
                    self.looped = looped.filter(|r| r.len() > 64);
                    self.playing = true;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.consumed = 0;
                    self.tail_remaining = 0;
                    self.resampler.reset();
                    self.mix.reset();
                }
                Cmd::Stop => {
                    self.playing = false;
                    self.end = None;
                    self.looped = None;
                    self.tail_remaining = 0;
                    self.resampler.reset();
                    self.mix.reset();
                }
                Cmd::Locate(at) => {
                    self.pos = at;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.consumed = 0;
                    self.tail_remaining = 0;
                    self.resampler.reset();
                    self.mix.reset();
                }
            }
        }
        if changed {
            // Place adopted instances now (not only once playback starts), so editors see them;
            // `sync` creates no third-party plugins here.
            let lookup = self.index.current();
            self.mix.ensure_synced_with(&self.session, lookup.matches(&self.session).then_some(lookup));
            self.session_adopted.store(true, Ordering::Release);
        }
        self.send_garbage();
        self.return_sessions();
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }

    fn send_garbage(&mut self) {
        if self.deferred_plugins.is_empty() {
            self.deferred_plugins = self.mix.take_retired();
        } else {
            self.deferred_plugins.extend(self.mix.take_retired());
        }
        if !self.deferred_plugins.is_empty()
            && self.garbage.slots() != 0
            && let Err(plugins) = self.garbage.push(std::mem::take(&mut self.deferred_plugins))
        {
            self.deferred_plugins = plugins;
        }
    }

    fn return_sessions(&mut self) {
        while !self.deferred_sessions.is_empty() && self.old_sessions.slots() != 0 {
            let old = self.deferred_sessions.remove(0);
            if let Err(old) = self.old_sessions.push(old) {
                self.deferred_sessions.insert(0, old);
                break;
            }
        }
    }

    /// Render the next block at the session rate into `pending`.
    fn render_next(&mut self, quantum: usize, resampling: bool) {
        let mut n = quantum.clamp(1, BLOCK);
        if !self.playing && self.tail_remaining > 0 {
            self.pending.iter_mut().for_each(|channel| channel.fill(0.0));
            self.pending_len = n;
            return;
        }
        if let Some(l) = self.looped
            && self.pos >= l.end
        {
            self.pos = l.start;
        }
        if let Some(l) = self.looped {
            n = n.min(usize::try_from((l.end - self.pos).max(1)).unwrap_or(BLOCK));
        } else if let Some(e) = self.end {
            if self.pos >= e {
                self.playing = false;
                self.end = None;
                self.shared.playing.store(false, Ordering::Relaxed);
                self.tail_remaining = if resampling { 2 * super::resample::DELAY } else { 0 };
                n = 0;
            } else {
                n = n.min(usize::try_from(e - self.pos).unwrap_or(BLOCK));
            }
        }
        if n == 0 {
            for c in &mut self.pending {
                c.iter_mut().for_each(|x| *x = 0.0);
            }
            self.pending_len = BLOCK;
            return;
        }
        if let Some(ring) = &self.input {
            let ch = ring.channels.load(Ordering::Relaxed).max(1);
            if self.mix.input.len() != ch {
                self.mix.input = vec![vec![0.0; BLOCK]; ch];
            }
            ring.pop_into(&mut self.mix.input, n, self.session.sample_rate.hz());
        }
        self.mix.monitor_only = !self.playing;
        self.mix.recording = self.recording;
        let lookup = self.index.current();
        self.mix.render_with_index(&self.session, self.pos, n, &mut self.pending, lookup.matches(&self.session).then_some(lookup));
        self.send_garbage();
        self.pending_len = n;
        self.publish_meters();
        if self.playing {
            self.pos += n as i64;
            self.shared.position.store(self.pos, Ordering::Relaxed);
        }
    }

    /// Fold each rendered block into the unpublished snapshot, then hand it to the UI.
    fn publish_meters(&mut self) {
        self.unsent.accumulate(&self.mix.main_meter, &self.mix.meters);
        self.flush_meters();
    }

    fn flush_meters(&mut self) {
        if self.meter_tx.slots() != 0
            && let Some(mut recycled) = self.meter_recycle.pop()
        {
            recycled.reset_levels();
            recycled.forget(&self.forgotten);
            let published = std::mem::replace(&mut self.unsent, recycled);
            if let Err(published) = self.meter_tx.push(published) {
                self.unsent = published;
            }
        }
    }

    /// Fill an interleaved device buffer.
    pub(crate) fn fill(&mut self, out: &mut [f32], channels: usize) {
        self.beat.beat();
        self.drain();
        let channels = channels.max(1);
        let monitoring = self.input.is_some() && self.session.tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm);
        if !self.playing && !monitoring && self.tail_remaining == 0 {
            out.iter_mut().for_each(|x| *x = 0.0);
            self.unsent.silence();
            self.flush_meters();
            return;
        }
        let speed = f64::from(self.shared.speed.load());
        let step = self.session.sample_rate.as_f64() / self.device_rate.max(1.0) * speed.clamp(0.1, 4.0);
        let bypass = step == 1.0;
        self.resampler.set_step(step);
        let callback_frames = out.len() / channels;
        let quantum = ((callback_frames as f64 * step).ceil() as usize).clamp(1, BLOCK);
        let target_gain = self.shared.output_gain.load();
        let gain_step = (target_gain - self.output_gain) / callback_frames.max(1) as f32;
        for frame in out.chunks_mut(channels) {
            self.output_gain += gain_step;
            while self.read >= self.pending_len as f64 {
                self.consume(self.pending_len);
                self.read -= self.pending_len as f64;
                if !self.playing && !monitoring && self.tail_remaining == 0 {
                    break;
                }
                self.render_next(quantum, !bypass);
                self.consumed = 0;
                if self.pending_len == 0 {
                    break;
                }
            }
            let i0 = self.read.floor() as usize;
            let fr = self.read - i0 as f64;
            self.consume(i0.saturating_add(1).min(self.pending_len));
            let sounding = self.playing || monitoring || self.tail_remaining > 0;
            let nch = self.pending.len();
            let sample = |c: usize| {
                if bypass { self.pending.get(c).and_then(|samples| samples.get(i0)).copied().unwrap_or(0.0) } else { self.resampler.sample(c, fr) }
            };
            let gain = self.output_gain;
            write_frame(frame, channels, nch, sounding, &self.fold, |channel| sample(channel) * gain);
            self.read += step;
        }
        self.output_gain = target_gain;
    }

    fn consume(&mut self, end: usize) {
        while self.consumed < end {
            self.resampler.push(&self.pending, self.consumed);
            self.consumed += 1;
            if !self.playing {
                self.tail_remaining = self.tail_remaining.saturating_sub(1);
            }
        }
    }
}

/// Write one device frame from `nch` rendered channels: fold surround to stereo (a mono device
/// averages the two sides), or copy the channels straight through. `sample(c)` is channel `c`.
fn write_frame(frame: &mut [f32], channels: usize, nch: usize, sounding: bool, fold: &[(f32, f32)], sample: impl Fn(usize) -> f32) {
    if channels < nch {
        // Fold surround to stereo first; a mono device averages the two sides.
        let (mut l, mut r) = (0.0f32, 0.0f32);
        if sounding {
            if nch == 2 {
                (l, r) = (sample(0), sample(1));
            } else {
                for (c, (gl, gr)) in fold.iter().enumerate().take(nch) {
                    let v = sample(c);
                    l += v * gl;
                    r += v * gr;
                }
            }
        }
        for (c, o) in frame.iter_mut().enumerate() {
            *o = match c {
                0 if channels == 1 => out_sample(0.5 * l + 0.5 * r),
                0 => out_sample(l),
                1 => out_sample(r),
                _ => 0.0,
            };
        }
    } else {
        for (c, o) in frame.iter_mut().enumerate() {
            *o = if c < nch.max(1) && sounding { out_sample(sample(c)) } else { 0.0 };
        }
    }
}

/// Clamp to the device range, mapping NaN and infinities to silence.
fn out_sample(v: f32) -> f32 {
    if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }
}

/// UI-thread bookkeeping of the third-party instances the audio thread holds.
pub(crate) struct Side {
    /// What was last created for each slot: (plugin id, channels, created ok).
    pub(crate) shadow: HashMap<(TrackId, usize), (String, usize, bool)>,
    /// Session sample rate the instances were prepared for.
    rate: f32,
    /// Editor handles of the live instances.
    pub(crate) editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>>,
    /// State readers of the live instances.
    pub(crate) states: BTreeMap<(TrackId, usize), Box<dyn PluginStateReader>>,
    /// Instances that refused their stored state: saves keep that state until the plugin's editor
    /// is opened.
    pub(crate) refused: HashSet<(TrackId, usize)>,
    garbage: SpscConsumer<Vec<Box<dyn Plugin>>>,
    old_sessions: SpscConsumer<Arc<Session>>,
    /// The tracks of the session last sent, to tell the audio thread which ones went.
    pub(crate) tracks: HashSet<TrackId>,
}

impl Side {
    pub(crate) fn new(rate: f32, garbage: SpscConsumer<Vec<Box<dyn Plugin>>>, old_sessions: SpscConsumer<Arc<Session>>) -> Side {
        Side {
            shadow: HashMap::new(),
            rate,
            editors: HashMap::new(),
            states: BTreeMap::new(),
            refused: HashSet::new(),
            garbage,
            old_sessions,
            tracks: HashSet::new(),
        }
    }

    /// Note `s`'s tracks; the ones the previous session had and `s` doesn't.
    pub(crate) fn gone_tracks(&mut self, s: &Session) -> Vec<TrackId> {
        let now: HashSet<TrackId> = s.tracks.iter().map(|t| t.id).collect();
        let gone = self.tracks.difference(&now).copied().collect();
        self.tracks = now;
        gone
    }

    /// Drops retired instances (closing their editors first, inside the plugin's drop) and
    /// replaced session snapshots.
    pub(crate) fn collect_garbage(&mut self) {
        while let Some(g) = self.garbage.pop() {
            for p in g {
                log::debug!("plugin instance released: {}", p.info().name);
                drop(p);
            }
        }
        while let Some(old) = self.old_sessions.pop() {
            drop(old);
        }
    }

    pub(crate) fn forget(&mut self, key: (TrackId, usize)) {
        self.shadow.remove(&key);
        self.states.remove(&key);
        self.refused.remove(&key);
        if let Some(mut e) = self.editors.remove(&key) {
            e.close();
        }
    }
}

pub(crate) fn take_meter_snapshot(source: &mut MeterSnapshot, out: &mut MeterSnapshot) {
    if !source.fresh {
        // No block since the last take: keep the readings, but not those of deleted tracks.
        out.tracks.retain(|id, _| source.tracks.contains_key(id));
        return;
    }
    copy_meter_snapshot(source, out);
    source.reset_levels();
}

pub(crate) fn copy_meter_snapshot(source: &MeterSnapshot, out: &mut MeterSnapshot) {
    out.tracks.retain(|id, _| source.tracks.contains_key(id));
    for (id, meter) in &source.tracks {
        out.tracks.entry(*id).or_default().copy_from(meter);
    }
    out.main.copy_from(&source.main);
}

pub(crate) struct MeterUi {
    rx: SpscConsumer<MeterSnapshot>,
    recycle: SpscProducer<MeterSnapshot>,
}

impl MeterUi {
    pub(crate) fn new(rx: SpscConsumer<MeterSnapshot>, recycle: SpscProducer<MeterSnapshot>) -> Self {
        Self { rx, recycle }
    }

    pub(crate) fn take(&mut self, out: &mut MeterSnapshot) {
        let mut fresh = false;
        while let Some(mut snapshot) = self.rx.pop() {
            if snapshot.clear_previous {
                out.reset_levels();
                fresh = false;
            }
            if !fresh {
                take_meter_snapshot(&mut snapshot, out);
                fresh = true;
            } else {
                out.accumulate(&snapshot.main, &snapshot.tracks);
            }
            snapshot.reset_levels();
            let _ = self.recycle.push(snapshot);
        }
    }
}

impl Side {
    pub(crate) fn capture_states(&mut self) -> PluginStates {
        self.states.iter_mut().filter(|(k, _)| !self.refused.contains(*k)).filter_map(|(&(t, slot), r)| Some((t, slot, r.save_state()?))).collect()
    }

    pub(crate) fn changed_states(&mut self) -> Vec<(TrackId, usize)> {
        self.editors.iter_mut().filter_map(|(k, e)| e.state_changed().then_some(*k)).collect()
    }

    pub(crate) fn capture_states_of(&mut self, slots: &[(TrackId, usize)]) -> PluginStates {
        self.states
            .iter_mut()
            .filter(|(k, _)| !self.refused.contains(*k) && slots.contains(k))
            .filter_map(|(&(t, slot), r)| Some((t, slot, r.save_state()?)))
            .collect()
    }

    /// A plugin that marks its state changed while it loads or restores it hasn't changed.
    pub(crate) fn add_editor(&mut self, key: (TrackId, usize), mut e: Box<dyn PluginEditor>) {
        let _ = e.state_changed();
        self.editors.insert(key, e);
    }

    pub(crate) fn open_editor(&mut self, key: (TrackId, usize)) -> Result<(), String> {
        let e = self.editors.get_mut(&key).ok_or_else(|| "this plugin has no editor (or it is not loaded)".to_string())?;
        e.open()?;
        self.refused.remove(&key);
        Ok(())
    }

    /// Creates the third-party instances `s` needs that the audio thread does not have yet.
    pub(crate) fn prepare(&mut self, s: &Session) -> Vec<PreparedInstance> {
        self.collect_garbage();
        let sr = s.sample_rate.as_f64() as f32;
        if (sr - self.rate).abs() > 0.5 {
            // The audio thread rebuilds its engine: everything is created anew.
            let keys: Vec<(TrackId, usize)> = self.shadow.keys().copied().collect();
            for k in keys {
                self.forget(k);
            }
            self.rate = sr;
        }
        let specs = soundcraft_mix::instance_specs(s);
        let stale: Vec<(TrackId, usize)> = self.shadow.keys().filter(|k| !specs.iter().any(|x| (x.track, x.slot) == **k)).copied().collect();
        for k in stale {
            self.forget(k);
        }
        let mut out = Vec::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            if self.shadow.get(&key).is_some_and(|(id, ch, _)| *id == spec.id && *ch == spec.ch) {
                continue;
            }
            self.forget(key);
            let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), sr, BLOCK, spec.ch);
            log::debug!("plugin instance for {:?} slot {}: {} ({} ch, created: {})", spec.track, spec.slot, spec.id, spec.ch, made.is_some());
            self.shadow.insert(key, (spec.id.clone(), spec.ch, made.is_some()));
            if let Some(soundcraft_mix::NewInstance { mut plugin, refused_state }) = made {
                if let Some(e) = plugin.editor() {
                    self.add_editor(key, e);
                }
                if let Some(r) = plugin.state_reader() {
                    self.states.insert(key, r);
                }
                if refused_state {
                    self.refused.insert(key);
                }
                out.push(PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin });
            }
        }
        out
    }
}

/// ITU stereo fold-down gains for each main channel of a session.
fn fold_gains(s: &Session) -> [(f32, f32); MAX_CHANNELS] {
    let mut g = [(0.0, 0.0); MAX_CHANNELS];
    let f = s.main_format();
    let sp = f.speakers();
    if sp.len() == f.channels() {
        for (o, x) in g.iter_mut().zip(sp.iter()) {
            *o = itu_stereo_fold(*x);
        }
    } else {
        // Ambisonics: W only, at -3 dB per side.
        g[0] = (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2);
    }
    g
}
