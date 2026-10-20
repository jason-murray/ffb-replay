//! Owns one pipeline at a time and everything the UI and CLI want to know
//! about it: its mode, the sample ring, status lines, and the recording
//! file. Every operation is a method on a cheaply cloneable handle.

use crate::backends::{self, SinkSpec, SourceSpec};
use ffb_replay_core::format::{Recording, EXTENSION};
use ffb_replay_core::json::Json;
use ffb_replay_core::model::{Effect, FfbEvent};
use ffb_replay_core::pipeline::{Command, Outcome, Pipeline, PipelineConfig, Sample, Summary};
use ffb_replay_core::player::{Player, PlayerOptions};
use std::collections::VecDeque;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Samples kept for late-joining UI clients (30 s at 1 kHz).
const RING_CAPACITY: usize = 30_000;
const LOG_CAPACITY: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Idle,
    /// A source feeding a sink; `recording` names the file when on.
    Live {
        recording: Option<PathBuf>,
    },
    Playback {
        file: PathBuf,
        paused: bool,
        speed: f32,
        looped: bool,
    },
}

struct Running {
    commands: Sender<Command>,
    pipeline_thread: JoinHandle<Summary>,
    sampler_thread: JoinHandle<()>,
}

struct State {
    mode: Mode,
    source_name: String,
    sink_name: String,
    running: Option<Running>,
    ring: VecDeque<Sample>,
    /// Sequence number of the newest sample in `ring`.
    seq: u64,
    last_sample: Option<Sample>,
    last_summary: Option<Summary>,
    last_error: Option<String>,
    log: VecDeque<(Instant, String)>,
    started: Instant,
    config: PipelineConfig,
    recordings_dir: PathBuf,
}

#[derive(Clone)]
pub struct Session {
    inner: Arc<Mutex<State>>,
}

impl Session {
    pub fn new(config: PipelineConfig, recordings_dir: PathBuf) -> Self {
        Session {
            inner: Arc::new(Mutex::new(State {
                mode: Mode::Idle,
                source_name: String::new(),
                sink_name: String::new(),
                running: None,
                ring: VecDeque::with_capacity(RING_CAPACITY),
                seq: 0,
                last_sample: None,
                last_summary: None,
                last_error: None,
                log: VecDeque::new(),
                started: Instant::now(),
                config,
                recordings_dir,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn log(&self, line: impl Into<String>) {
        let line = line.into();
        eprintln!("{line}");
        let mut st = self.lock();
        st.log.push_back((Instant::now(), line));
        while st.log.len() > LOG_CAPACITY {
            st.log.pop_front();
        }
    }

    pub fn recordings_dir(&self) -> PathBuf {
        self.lock().recordings_dir.clone()
    }

    pub fn config(&self) -> PipelineConfig {
        self.lock().config.clone()
    }

    pub fn is_running(&self) -> bool {
        self.lock().running.is_some()
    }

    /// Reap a pipeline thread that ended on its own (source finished,
    /// device error). Called before every operation and status read.
    fn reap(&self) {
        let finished = {
            let st = self.lock();
            matches!(&st.running, Some(r) if r.pipeline_thread.is_finished())
        };
        if finished {
            self.finish("ended");
        }
    }

    fn finish(&self, why: &str) {
        let running = self.lock().running.take();
        let Some(r) = running else { return };
        let _ = r.commands.send(Command::Stop);
        let summary = r.pipeline_thread.join().ok();
        let _ = r.sampler_thread.join();
        let mut st = self.lock();
        let mode = std::mem::replace(&mut st.mode, Mode::Idle);
        match &summary {
            Some(s) => {
                let line = match &s.outcome {
                    Outcome::Error(e) => {
                        st.last_error = Some(e.clone());
                        format!("pipeline stopped with an error: {e}")
                    }
                    Outcome::SourceFinished => format!(
                        "{why}: source finished after {:.1} s, {} events",
                        s.duration_us as f64 / 1e6,
                        s.events_total
                    ),
                    Outcome::Stopped => format!(
                        "{why}: stopped after {:.1} s, {} events, {} late ticks",
                        s.duration_us as f64 / 1e6,
                        s.events_total,
                        s.late_ticks
                    ),
                };
                if let Some(n) = s.recorded {
                    if let Mode::Live { recording: Some(p) } = &mode {
                        st.log.push_back((
                            Instant::now(),
                            format!("recording saved: {} ({n} records)", p.display()),
                        ));
                    }
                }
                st.last_summary = summary.clone();
                st.log.push_back((Instant::now(), line));
            }
            None => st.log.push_back((Instant::now(), "pipeline thread panicked".into())),
        }
        while st.log.len() > LOG_CAPACITY {
            st.log.pop_front();
        }
    }

    fn spawn(&self, pipeline: Pipeline, mode: Mode) {
        let (cmd_tx, cmd_rx) = channel();
        let (s_tx, s_rx): (_, Receiver<Sample>) = sync_channel(8192);
        let source_name = pipeline.source_name();
        let sink_name = pipeline.sink_name();
        let pipeline_thread = std::thread::Builder::new()
            .name("ffb-pipeline".into())
            .spawn(move || pipeline.run(cmd_rx, s_tx))
            .expect("spawn pipeline thread");
        let inner = self.inner.clone();
        let sampler_thread = std::thread::Builder::new()
            .name("ffb-sampler".into())
            .spawn(move || {
                // Batch under one lock per ~5 ms rather than per sample.
                loop {
                    let first = match s_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(s) => s,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    };
                    let mut batch = vec![first];
                    batch.extend(s_rx.try_iter().take(64));
                    let mut st = inner.lock().unwrap_or_else(|e| e.into_inner());
                    for s in batch {
                        if st.ring.len() >= RING_CAPACITY {
                            st.ring.pop_front();
                        }
                        st.ring.push_back(s);
                        st.seq += 1;
                        st.last_sample = Some(s);
                    }
                }
            })
            .expect("spawn sampler thread");
        let mut st = self.lock();
        st.mode = mode;
        st.source_name = source_name;
        st.sink_name = sink_name;
        st.last_error = None;
        st.ring.clear();
        st.last_sample = None;
        st.started = Instant::now();
        st.running = Some(Running { commands: cmd_tx, pipeline_thread, sampler_thread });
    }

    /// Start a passthrough (or graph-only) session.
    pub fn start_live(
        &self,
        source: &SourceSpec,
        sink: &SinkSpec,
        grab: bool,
    ) -> Result<(), String> {
        self.reap();
        if self.is_running() {
            return Err("already running; stop first".into());
        }
        let config = self.config();
        let built = backends::build(source, sink, config.tick_hz, grab)?;
        for n in &built.notes {
            self.log(n.clone());
        }
        let pipeline = Pipeline::new(built.source, built.sink, config);
        self.log(format!("live: {} -> {}", pipeline.source_name(), pipeline.sink_name()));
        self.spawn(pipeline, Mode::Live { recording: None });
        Ok(())
    }

    /// Play a recording into a sink.
    pub fn start_playback(
        &self,
        file: &Path,
        sink: &SinkSpec,
        options: PlayerOptions,
        grab: bool,
    ) -> Result<(), String> {
        self.reap();
        if self.is_running() {
            return Err("already running; stop first".into());
        }
        let recording = Recording::load(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let config = self.config();
        let built = backends::build(&SourceSpec::Idle, sink, config.tick_hz, grab)?;
        for n in &built.notes {
            self.log(n.clone());
        }
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.log(format!(
            "playback: {} ({:.1} s, {} events, recorded from {}) -> {}",
            name,
            recording.duration_us() as f64 / 1e6,
            recording.ffb_event_count(),
            recording.header.source,
            built.sink.name()
        ));
        let player = Player::new(recording, name, options);
        let pipeline = Pipeline::new(Box::new(player), built.sink, config);
        self.spawn(
            pipeline,
            Mode::Playback {
                file: file.to_path_buf(),
                paused: false,
                speed: options.speed,
                looped: options.looped,
            },
        );
        Ok(())
    }

    pub fn stop(&self) {
        self.reap();
        if self.is_running() {
            self.finish("stop");
        }
    }

    /// Begin recording the live session to `recordings_dir/<name>.ffbr`
    /// (a name with a directory component is used as given).
    pub fn start_recording(&self, name: &str, notes: &str) -> Result<PathBuf, String> {
        self.reap();
        let path = {
            let st = self.lock();
            match &st.mode {
                Mode::Live { recording: None } => {}
                Mode::Live { recording: Some(p) } => {
                    return Err(format!("already recording to {}", p.display()))
                }
                Mode::Playback { .. } => return Err("cannot record during playback".into()),
                Mode::Idle => return Err("start a live session first".into()),
            }
            recording_path(&st.recordings_dir, name)
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let file = File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        let out: Box<dyn std::io::Write + Send> =
            Box::new(BufWriter::with_capacity(64 * 1024, file));
        {
            let st = self.lock();
            let Some(r) = &st.running else { return Err("not running".into()) };
            r.commands
                .send(Command::StartRecording { out, notes: notes.to_string() })
                .map_err(|_| "pipeline gone".to_string())?;
        }
        self.lock().mode = Mode::Live { recording: Some(path.clone()) };
        self.log(format!("recording to {}", path.display()));
        Ok(path)
    }

    pub fn stop_recording(&self) -> Result<(), String> {
        self.reap();
        let mut st = self.lock();
        match &st.mode {
            Mode::Live { recording: Some(_) } => {}
            _ => return Err("not recording".into()),
        }
        let Some(r) = &st.running else { return Err("not running".into()) };
        r.commands.send(Command::StopRecording).map_err(|_| "pipeline gone".to_string())?;
        let path = match std::mem::replace(&mut st.mode, Mode::Live { recording: None }) {
            Mode::Live { recording: Some(p) } => p,
            _ => unreachable!(),
        };
        st.log.push_back((Instant::now(), format!("recording stopped: {}", path.display())));
        Ok(())
    }

    pub fn set_paused(&self, paused: bool) -> Result<(), String> {
        self.reap();
        let mut st = self.lock();
        if st.running.is_none() {
            return Err("not running".into());
        }
        match &mut st.mode {
            Mode::Playback { paused: p, .. } => *p = paused,
            _ => return Err("not playing back".into()),
        }
        let Some(r) = &st.running else { return Err("not running".into()) };
        r.commands.send(Command::SetPaused(paused)).map_err(|_| "pipeline gone".to_string())
    }

    /// Push an event into the running pipeline as if the source sent it.
    pub fn inject(&self, event: FfbEvent) -> Result<(), String> {
        self.reap();
        let st = self.lock();
        let Some(r) = &st.running else { return Err("not running".into()) };
        r.commands.send(Command::Inject(event)).map_err(|_| "pipeline gone".to_string())
    }

    /// A test pulse: constant force `magnitude` for `ms` in slot 127.
    pub fn inject_pulse(&self, magnitude: f32, ms: u32) -> Result<(), String> {
        let effect = Effect::constant(magnitude).with_duration(Some(ms.max(1)));
        self.inject(FfbEvent::Upload { id: 127, effect })?;
        self.inject(FfbEvent::Play { id: 127, count: 1 })
    }

    /// Samples newer than `since` (a sequence number from a previous call),
    /// and the sequence number to pass next time.
    pub fn samples_since(&self, since: u64, max: usize) -> (Vec<Sample>, u64) {
        let st = self.lock();
        let newest = st.seq;
        if newest <= since {
            return (Vec::new(), newest);
        }
        let want = ((newest - since) as usize).min(st.ring.len()).min(max);
        let skip = st.ring.len() - want;
        (st.ring.iter().skip(skip).copied().collect(), newest)
    }

    pub fn last_summary(&self) -> Option<Summary> {
        self.reap();
        self.lock().last_summary.clone()
    }

    /// Wait for the pipeline to end on its own (source finished or error).
    /// Returns `false` on timeout.
    #[cfg(test)]
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        let start = Instant::now();
        loop {
            self.reap();
            if !self.is_running() {
                return true;
            }
            if timeout.is_some_and(|t| start.elapsed() >= t) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Everything the UI shows apart from the trace.
    pub fn status_json(&self) -> Json {
        self.reap();
        let st = self.lock();
        let (mode, recording, file, paused, speed, looped) = match &st.mode {
            Mode::Idle => ("idle", None, None, false, 1.0, false),
            Mode::Live { recording } => ("live", recording.clone(), None, false, 1.0, false),
            Mode::Playback { file, paused, speed, looped } => {
                ("playback", None, Some(file.clone()), *paused, *speed, *looped)
            }
        };
        let s = st.last_sample;
        let mut j = Json::obj()
            .with("mode", mode)
            .with("source", st.source_name.as_str())
            .with("sink", st.sink_name.as_str())
            .with("recording", recording.map(|p| p.display().to_string()))
            .with("file", file.map(|p| p.display().to_string()))
            .with("paused", paused)
            .with("speed", speed)
            .with("loop", looped)
            .with("tick_hz", st.config.tick_hz)
            .with("uptime_s", st.started.elapsed().as_secs_f64())
            .with("seq", st.seq)
            .with("error", st.last_error.clone())
            .with("recordings_dir", st.recordings_dir.display().to_string());
        j = match s {
            Some(s) => j
                .with("t_s", s.t_us as f64 / 1e6)
                .with("events_total", s.events_total)
                .with("playing", s.rendered.playing)
                .with("loaded", s.rendered.loaded)
                .with("gain", s.rendered.gain)
                .with("force", s.rendered.total)
                .with("axis", s.axis)
                .with("recording_s", s.recording_us as f64 / 1e6)
                .with("position_s", s.progress.map(|(p, _)| p as f64 / 1e6))
                .with("duration_s", s.progress.map(|(_, d)| d as f64 / 1e6))
                .with("sink_errors", s.sink_errors)
                .with("late_ticks", s.late_ticks),
            None => j.with("t_s", 0.0).with("events_total", 0u64),
        };
        let log: Vec<Json> = st
            .log
            .iter()
            .rev()
            .take(40)
            .map(|(t, l)| {
                Json::obj().with("age_s", t.elapsed().as_secs_f64()).with("line", l.as_str())
            })
            .collect();
        j.with("log", Json::Arr(log))
    }

    /// Recordings in the recordings directory, newest first.
    pub fn recordings_json(&self) -> Json {
        let dir = self.recordings_dir();
        let mut items: Vec<(std::time::SystemTime, Json)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some(EXTENSION) {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                let since = modified
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                items.push((
                    modified,
                    Json::obj()
                        .with(
                            "name",
                            p.file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        )
                        .with("path", p.display().to_string())
                        .with("bytes", meta.len())
                        .with("modified_unix", since),
                ));
            }
        }
        items.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        Json::Arr(items.into_iter().map(|(_, j)| j).collect())
    }

    /// Resolve a recording name or path against the recordings directory.
    pub fn resolve_recording(&self, name: &str) -> PathBuf {
        let p = Path::new(name);
        if p.is_absolute() || p.exists() {
            return p.to_path_buf();
        }
        recording_path(&self.recordings_dir(), name)
    }
}

/// `dir/name.ffbr`, adding the extension when missing; a name that already
/// has a directory part is kept as is.
pub fn recording_path(dir: &Path, name: &str) -> PathBuf {
    let name = name.trim();
    let mut p = if name.is_empty() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        dir.join(format!("ffb-{now}"))
    } else if Path::new(name).components().count() > 1 {
        PathBuf::from(name)
    } else {
        dir.join(name)
    };
    if p.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
        let mut s = p.into_os_string();
        s.push(".");
        s.push(EXTENSION);
        p = PathBuf::from(s);
    }
    p
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(r) = self.running.take() {
            let _ = r.commands.send(Command::Stop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_paths() {
        let dir = Path::new("/tmp/rec");
        assert_eq!(recording_path(dir, "lap1"), PathBuf::from("/tmp/rec/lap1.ffbr"));
        assert_eq!(recording_path(dir, "lap1.ffbr"), PathBuf::from("/tmp/rec/lap1.ffbr"));
        assert_eq!(recording_path(dir, "/elsewhere/x"), PathBuf::from("/elsewhere/x.ffbr"));
        assert!(recording_path(dir, "").to_string_lossy().starts_with("/tmp/rec/ffb-"));
    }

    #[test]
    fn demo_session_runs_records_and_stops() {
        let tmp = std::env::temp_dir().join(format!("ffb-replay-test-{}", std::process::id()));
        let s = Session::new(
            PipelineConfig { tick_hz: 1000, axis_sample_hz: 100, sample_every: 1 },
            tmp.clone(),
        );
        assert!(s.start_recording("x", "").is_err(), "needs a live session");
        s.start_live(&SourceSpec::Demo { looped: true }, &SinkSpec::None, false).unwrap();
        assert!(s.start_live(&SourceSpec::Demo { looped: true }, &SinkSpec::None, false).is_err());
        std::thread::sleep(Duration::from_millis(120));
        let path = s.start_recording("take", "notes here").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let (samples, seq) = s.samples_since(0, 100_000);
        assert!(samples.len() >= 100, "{}", samples.len());
        assert!(seq >= 100);
        let (more, _) = s.samples_since(seq, 100);
        assert!(more.len() <= 100);
        s.stop_recording().unwrap();
        s.stop();
        assert!(!s.is_running());
        let rec = Recording::load(&path).unwrap();
        assert_eq!(rec.header.notes, "notes here");
        assert!(rec.ffb_event_count() >= 5, "{}", rec.ffb_event_count());
        let status = s.status_json();
        assert_eq!(status.get("mode").unwrap().as_str(), Some("idle"));

        // And play it back.
        s.start_playback(
            &path,
            &SinkSpec::None,
            PlayerOptions { speed: 50.0, looped: false },
            false,
        )
        .unwrap();
        assert!(s.wait(Some(Duration::from_secs(5))), "playback finishes on its own");
        let summary = s.last_summary().unwrap();
        assert_eq!(summary.outcome, Outcome::SourceFinished);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
