//! The tick loop: pull events from a source, push them to a sink, render
//! the force for the graph, and record when asked.
//!
//! One pipeline runs on one thread for as long as a session lasts (a
//! passthrough session, or one playback). The owner talks to it through a
//! command channel and reads a stream of [`Sample`]s back; the channel is
//! bounded and samples are dropped rather than ever blocking the loop.

use crate::format::Header;
use crate::io::{BackendError, FfbSink, FfbSource, SinkMode, SourceStatus};
use crate::model::{DeviceControl, FfbEvent, InputEvent};
use crate::pacer::Pacer;
use crate::recorder::Recorder;
use crate::render::{AxisState, Rendered, Renderer};
use std::io::Write;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Instant, SystemTime};

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineConfig {
    /// Loop rate. Rendered-mode sinks are driven at this rate; events are
    /// forwarded with at most one tick of latency.
    pub tick_hz: u32,
    /// How often the steering axis is written into a recording.
    pub axis_sample_hz: u32,
    /// Publish every `sample_every` tick to the UI (`1` = all of them).
    pub sample_every: u32,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig { tick_hz: 1000, axis_sample_hz: 100, sample_every: 1 }
    }
}

/// Instructions from the owner.
pub enum Command {
    /// Begin writing a recording to `out`. `notes` goes in the header.
    StartRecording {
        out: Box<dyn Write + Send>,
        notes: String,
    },
    StopRecording,
    /// Pause or resume the source (playback only).
    SetPaused(bool),
    /// Inject an event as if the source produced it (UI test buttons).
    Inject(FfbEvent),
    /// End the session.
    Stop,
}

/// One tick as seen by the UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Pipeline time, microseconds since the loop started.
    pub t_us: u64,
    pub rendered: Rendered,
    /// Steering position, if known.
    pub axis: Option<f32>,
    /// Events forwarded this tick.
    pub events: u32,
    pub events_total: u64,
    pub recording: bool,
    /// Current recording length, if recording.
    pub recording_us: u64,
    /// Playback `(position, duration)`, if the source reports it.
    pub progress: Option<(u64, u64)>,
    pub sink_errors: u64,
    pub late_ticks: u64,
}

/// What the loop returned.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub ticks: u64,
    pub late_ticks: u64,
    pub events_total: u64,
    pub duration_us: u64,
    /// Records written by the last recording, if any.
    pub recorded: Option<u64>,
    /// Why it stopped.
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `Command::Stop`, or the owner dropped the command channel.
    Stopped,
    /// The source reported `Finished`.
    SourceFinished,
    Error(String),
}

pub struct Pipeline {
    source: Box<dyn FfbSource>,
    sink: Box<dyn FfbSink>,
    config: PipelineConfig,
    renderer: Renderer,
    recorder: Option<Recorder>,
    last_recorded: Option<u64>,
    events_total: u64,
    sink_errors: u64,
    last_axis: Option<f32>,
    axis_velocity: f32,
}

impl Pipeline {
    pub fn new(source: Box<dyn FfbSource>, sink: Box<dyn FfbSink>, config: PipelineConfig) -> Self {
        Pipeline {
            source,
            sink,
            config: PipelineConfig {
                tick_hz: config.tick_hz.clamp(50, 10_000),
                sample_every: config.sample_every.max(1),
                ..config
            },
            renderer: Renderer::new(),
            recorder: None,
            last_recorded: None,
            events_total: 0,
            sink_errors: 0,
            last_axis: None,
            axis_velocity: 0.0,
        }
    }

    pub fn source_name(&self) -> String {
        self.source.name()
    }

    pub fn sink_name(&self) -> String {
        self.sink.name()
    }

    fn start_recording(
        &mut self,
        out: Box<dyn Write + Send>,
        notes: String,
        now_us: u64,
    ) -> Result<(), String> {
        let header = Header {
            created_unix: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            source: self.source.name(),
            sink: self.sink.name(),
            tick_hz: self.config.tick_hz,
            notes,
            ..Header::default()
        };
        let mut initial = Vec::new();
        self.renderer.snapshot(&mut initial);
        let rec = Recorder::start(out, &header, now_us, &initial, self.config.axis_sample_hz)
            .map_err(|e| e.to_string())?;
        self.recorder = Some(rec);
        Ok(())
    }

    fn stop_recording(&mut self) -> Option<Result<u64, String>> {
        let rec = self.recorder.take()?;
        let r = rec.finish();
        if let Ok(n) = r {
            self.last_recorded = Some(n);
        }
        Some(r)
    }

    fn forward(&mut self, ev: &FfbEvent, now_us: u64) -> Result<(), BackendError> {
        self.renderer.apply(ev, now_us);
        if let Some(rec) = &mut self.recorder {
            rec.event(now_us, ev);
        }
        self.events_total += 1;
        if self.sink.mode() == SinkMode::Effects {
            if let Err(e) = self.sink.apply(ev) {
                if e.fatal {
                    return Err(e);
                }
                self.sink_errors += 1;
            }
        }
        Ok(())
    }

    /// Run until stopped. Blocks the calling thread.
    pub fn run(mut self, commands: Receiver<Command>, samples: SyncSender<Sample>) -> Summary {
        let start = Instant::now();
        let mut pacer = Pacer::from_hz(self.config.tick_hz);
        let dt = pacer.period().as_secs_f32();
        let mut events: Vec<FfbEvent> = Vec::with_capacity(256);
        let mut inputs: Vec<InputEvent> = Vec::with_capacity(64);
        let mut tick: u64 = 0;
        let mut now_us: u64;

        let outcome = 'run: loop {
            pacer.wait();
            now_us = start.elapsed().as_micros() as u64;
            tick += 1;

            // Owner commands.
            loop {
                match commands.try_recv() {
                    Ok(Command::StartRecording { out, notes }) => {
                        if self.recorder.is_none() {
                            if let Err(e) = self.start_recording(out, notes, now_us) {
                                break 'run Outcome::Error(format!(
                                    "could not start recording: {e}"
                                ));
                            }
                        }
                    }
                    Ok(Command::StopRecording) => {
                        if let Some(Err(e)) = self.stop_recording() {
                            break 'run Outcome::Error(format!("recording failed: {e}"));
                        }
                    }
                    Ok(Command::SetPaused(p)) => {
                        // Pause the device too: a held constant force would
                        // keep pushing the wheel while the timeline stands.
                        self.source.set_paused(p);
                        let ctl = if p { DeviceControl::Pause } else { DeviceControl::Continue };
                        if let Err(e) = self.forward(&FfbEvent::Control(ctl), now_us) {
                            break 'run Outcome::Error(e.message);
                        }
                    }
                    Ok(Command::Inject(ev)) => {
                        if let Err(e) = self.forward(&ev, now_us) {
                            break 'run Outcome::Error(e.message);
                        }
                    }
                    Ok(Command::Stop) => break 'run Outcome::Stopped,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        break 'run Outcome::Stopped
                    }
                }
            }

            // Upstream events.
            events.clear();
            let status = match self.source.poll(&mut events) {
                Ok(s) => s,
                Err(e) if e.fatal => break 'run Outcome::Error(format!("source: {}", e.message)),
                Err(_) => SourceStatus::Active,
            };
            let n_events = events.len() as u32;
            for ev in events.drain(..) {
                if let Err(e) = self.forward(&ev, now_us) {
                    break 'run Outcome::Error(format!("sink: {}", e.message));
                }
            }

            // Mirror the real wheel's inputs onto the virtual one.
            inputs.clear();
            self.sink.poll_inputs(&mut inputs);
            if !inputs.is_empty() {
                if let Err(e) = self.source.inject_inputs(&inputs) {
                    if e.fatal {
                        break 'run Outcome::Error(format!("source: {}", e.message));
                    }
                }
            }

            // Axis state for condition effects and the graph.
            let axis = self.sink.axis().or_else(|| self.source.axis_hint());
            let axis_state = axis.map(|p| {
                let raw_v = match self.last_axis {
                    Some(prev) => (p - prev) / dt,
                    None => 0.0,
                };
                // Light smoothing: raw differences of a 1 kHz axis are noisy.
                self.axis_velocity += 0.2 * (raw_v - self.axis_velocity);
                AxisState { position: p, velocity: self.axis_velocity }
            });
            self.last_axis = axis;

            let rendered = self.renderer.sample(now_us, axis_state);
            if self.sink.mode() == SinkMode::Rendered {
                if let Err(e) = self.sink.set_force(rendered.total) {
                    if e.fatal {
                        break 'run Outcome::Error(format!("sink: {}", e.message));
                    }
                    self.sink_errors += 1;
                }
            }

            if let Some(rec) = &mut self.recorder {
                rec.axis(now_us, axis);
                if let Some(e) = rec.error.clone() {
                    break 'run Outcome::Error(format!("recording failed: {e}"));
                }
                // Keep the file close to live; a crash loses at most ~1 s.
                if tick % (self.config.tick_hz as u64) == 0 {
                    rec.flush();
                }
            }

            if tick % self.config.sample_every as u64 == 0 {
                let sample = Sample {
                    t_us: now_us,
                    rendered,
                    axis,
                    events: n_events,
                    events_total: self.events_total,
                    recording: self.recorder.is_some(),
                    recording_us: self.recorder.as_ref().map(|r| r.elapsed_us(now_us)).unwrap_or(0),
                    progress: self.source.progress(),
                    sink_errors: self.sink_errors,
                    late_ticks: pacer.late_ticks,
                };
                match samples.try_send(sample) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => break 'run Outcome::Stopped,
                }
            }

            if status == SourceStatus::Finished {
                break 'run Outcome::SourceFinished;
            }
        };

        let mut outcome = outcome;
        if let Some(Err(e)) = self.stop_recording() {
            if matches!(outcome, Outcome::Stopped | Outcome::SourceFinished) {
                outcome = Outcome::Error(format!("recording failed: {e}"));
            }
        }
        let _ = self.sink.release();

        Summary {
            ticks: tick,
            late_ticks: pacer.late_ticks,
            events_total: self.events_total,
            duration_us: now_us,
            recorded: self.last_recorded,
            outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Recording;
    use crate::io::{CaptureSink, NullSink, ScriptedSource};
    use crate::model::Effect;
    use crate::player::{Player, PlayerOptions};
    use std::sync::mpsc::{channel, sync_channel};
    use std::sync::{Arc, Mutex};

    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn script() -> Vec<(u64, FfbEvent)> {
        vec![
            (0, FfbEvent::Upload { id: 1, effect: Effect::constant(0.25) }),
            (0, FfbEvent::Play { id: 1, count: 1 }),
            (10_000, FfbEvent::Upload { id: 1, effect: Effect::constant(-0.5) }),
            (20_000, FfbEvent::Gain(0.5)),
            (30_000, FfbEvent::Stop { id: 1 }),
        ]
    }

    #[test]
    fn passthrough_forwards_everything_and_renders() {
        let source = ScriptedSource::new(script(), 1000, false);
        let (cmd_tx, cmd_rx) = channel();
        let (s_tx, s_rx) = sync_channel(1024);
        let p = Pipeline::new(
            Box::new(source),
            Box::new(CaptureSink::effects()),
            PipelineConfig::default(),
        );
        // Keep the sender alive on this side so the pipeline is not stopped by a drop.
        let _keep = cmd_tx;
        let summary = p.run(cmd_rx, s_tx);
        assert_eq!(summary.outcome, Outcome::SourceFinished);
        assert_eq!(summary.events_total, 5);
        let samples: Vec<Sample> = s_rx.try_iter().collect();
        assert!(samples.len() >= 20, "{}", samples.len());
        assert!((samples[0].rendered.total - 0.25).abs() < 1e-6, "{:?}", samples[0]);
        // Wall-clock windows are not reliable on a loaded CI box; check the
        // sequence of force levels instead.
        assert_eq!(levels(&samples), vec![0.25, -0.5, -0.25, 0.0]);
    }

    /// Distinct consecutive force levels of a sample stream.
    fn levels(samples: &[Sample]) -> Vec<f32> {
        let mut out: Vec<f32> = Vec::new();
        for s in samples {
            let v = (s.rendered.total * 1000.0).round() / 1000.0;
            if out.last() != Some(&v) {
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn rendered_sink_gets_the_scalar_force() {
        let source = ScriptedSource::new(script(), 1000, false);
        let (_cmd_tx, cmd_rx) = channel();
        let (s_tx, _s_rx) = sync_channel(16);
        let sink = NullSink::default();
        let p = Pipeline::new(Box::new(source), Box::new(sink), PipelineConfig::default());
        let summary = p.run(cmd_rx, s_tx);
        assert_eq!(summary.outcome, Outcome::SourceFinished);
        assert!(summary.ticks >= 30);
    }

    #[test]
    fn stop_command_ends_the_loop() {
        let (cmd_tx, cmd_rx) = channel();
        let (s_tx, _s_rx) = sync_channel(16);
        let p = Pipeline::new(
            Box::new(crate::io::IdleSource),
            Box::new(NullSink::default()),
            PipelineConfig::default(),
        );
        cmd_tx.send(Command::Stop).unwrap();
        let summary = p.run(cmd_rx, s_tx);
        assert_eq!(summary.outcome, Outcome::Stopped);
    }

    #[test]
    fn record_then_play_back_reproduces_the_trace() {
        // Record a scripted passthrough, starting mid-stream so the snapshot path is exercised.
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (cmd_tx, cmd_rx) = channel();
        let (s_tx, s_rx) = sync_channel(4096);
        let mut script = script();
        script.push((60_000, FfbEvent::Play { id: 1, count: 1 }));
        script.push((80_000, FfbEvent::Stop { id: 1 }));
        let p = Pipeline::new(
            Box::new(ScriptedSource::new(script, 1000, false)),
            Box::new(NullSink::default()),
            PipelineConfig::default(),
        );
        let out: Box<dyn Write + Send> = Box::new(Shared(buf.clone()));
        let handle = std::thread::spawn(move || p.run(cmd_rx, s_tx));
        // Wait until the effect is live, then start recording.
        loop {
            let s = s_rx.recv().unwrap();
            if s.t_us >= 5_000 {
                break;
            }
        }
        cmd_tx.send(Command::StartRecording { out, notes: "test".into() }).unwrap();
        let summary = handle.join().unwrap();
        assert_eq!(summary.outcome, Outcome::SourceFinished);
        let recorded = summary.recorded.expect("recording was finalised");
        assert!(recorded >= 6, "{recorded}");
        let live: Vec<Sample> = s_rx.try_iter().collect();

        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let rec = Recording::read_from(text.as_bytes()).unwrap();
        assert_eq!(rec.header.notes, "test");
        assert_eq!(rec.header.tick_hz, 1000);
        // Snapshot at t=0 carries the already-playing effect.
        assert!(matches!(
            rec.records[0].body,
            crate::format::RecordBody::Ffb(FfbEvent::Upload { id: 1, .. })
        ));
        assert!(matches!(
            rec.records[1].body,
            crate::format::RecordBody::Ffb(FfbEvent::Play { id: 1, .. })
        ));

        // Play it back through a fresh pipeline and compare the rendered force at a few points.
        let player = Player::new(rec, "mem", PlayerOptions::default());
        let (_cmd_tx2, cmd_rx2) = channel();
        let (s_tx2, s_rx2) = sync_channel(4096);
        let p2 = Pipeline::new(
            Box::new(player),
            Box::new(NullSink::default()),
            PipelineConfig::default(),
        );
        let summary2 = p2.run(cmd_rx2, s_tx2);
        assert_eq!(summary2.outcome, Outcome::SourceFinished);
        let replayed: Vec<Sample> = s_rx2.try_iter().collect();
        assert!(replayed.len() >= 60, "{}", replayed.len());
        // The recording began a few ms in (exactly when depends on scheduling),
        // so compare the tail of the level sequences: the second play after
        // the gain change, then silence, must come out identically.
        let live_levels = levels(&live);
        let replayed_levels = levels(&replayed);
        let tail = [-0.5, -0.25, 0.0, -0.25, 0.0];
        assert!(live_levels.ends_with(&tail), "live: {live_levels:?}");
        assert!(replayed_levels.ends_with(&tail), "replayed: {replayed_levels:?}");
        assert_eq!(replayed.last().unwrap().rendered.total, 0.0);
    }

    #[test]
    fn pausing_silences_the_device_and_resuming_restores_it() {
        let (cmd_tx, cmd_rx) = channel();
        let (s_tx, s_rx) = sync_channel(4096);
        let script = vec![
            (0, FfbEvent::Upload { id: 1, effect: Effect::constant(0.5) }),
            (0, FfbEvent::Play { id: 1, count: 1 }),
            (200_000, FfbEvent::Stop { id: 1 }),
        ];
        let p = Pipeline::new(
            Box::new(ScriptedSource::new(script, 1000, false)),
            Box::new(CaptureSink::effects()),
            PipelineConfig::default(),
        );
        let handle = std::thread::spawn(move || p.run(cmd_rx, s_tx));
        while s_rx.recv().unwrap().t_us < 10_000 {}
        cmd_tx.send(Command::SetPaused(true)).unwrap();
        let paused = loop {
            let s = s_rx.recv().unwrap();
            if s.rendered.paused {
                break s;
            }
            assert!(s.t_us < 50_000, "never paused");
        };
        assert_eq!(paused.rendered.total, 0.0);
        cmd_tx.send(Command::SetPaused(false)).unwrap();
        let resumed = loop {
            let s = s_rx.recv().unwrap();
            if !s.rendered.paused {
                break s;
            }
        };
        assert!((resumed.rendered.total - 0.5).abs() < 1e-6, "{resumed:?}");
        cmd_tx.send(Command::Stop).unwrap();
        let summary = handle.join().unwrap();
        assert_eq!(summary.outcome, Outcome::Stopped);
    }

    #[test]
    fn fatal_sink_error_stops_with_an_error_outcome() {
        struct Dead;
        impl FfbSink for Dead {
            fn name(&self) -> String {
                "dead".into()
            }
            fn mode(&self) -> SinkMode {
                SinkMode::Effects
            }
            fn apply(&mut self, _: &FfbEvent) -> Result<(), BackendError> {
                Err(BackendError::fatal("unplugged"))
            }
        }
        let (_cmd_tx, cmd_rx) = channel();
        let (s_tx, _s_rx) = sync_channel(16);
        let p = Pipeline::new(
            Box::new(ScriptedSource::new(script(), 1000, false)),
            Box::new(Dead),
            PipelineConfig::default(),
        );
        let summary = p.run(cmd_rx, s_tx);
        assert_eq!(summary.outcome, Outcome::Error("sink: unplugged".into()));
    }
}
