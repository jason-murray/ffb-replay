//! The two seams every backend implements.
//!
//! ```text
//!   game ──▶ [FfbSource] ──▶ pipeline ──▶ [FfbSink] ──▶ wheel
//!                              │ └─▶ Renderer ──▶ graph
//!                              └─▶ Recorder ──▶ .ffbr
//!   .ffbr ──▶ [Player: FfbSource] ──▶ pipeline ──▶ [FfbSink] ──▶ wheel
//! ```
//!
//! A source is where effects come from: a virtual device the game is bound
//! to (uinput on Linux, vJoy's FFB receiver on Windows), or a recording. A
//! sink is where they go: the real wheel, or nothing at all when only the
//! graph is wanted.

use crate::model::{FfbEvent, InputEvent};
use std::fmt;

/// A backend failure. `fatal` means the pipeline should stop (device gone);
/// otherwise the error is counted and the loop carries on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub message: String,
    pub fatal: bool,
}

impl BackendError {
    pub fn fatal(message: impl Into<String>) -> Self {
        BackendError { message: message.into(), fatal: true }
    }

    pub fn transient(message: impl Into<String>) -> Self {
        BackendError { message: message.into(), fatal: false }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BackendError {}

impl From<std::io::Error> for BackendError {
    fn from(e: std::io::Error) -> Self {
        BackendError::fatal(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Active,
    /// No more events will ever come (a recording reached its end).
    Finished,
}

/// Where FFB events come from.
pub trait FfbSource: Send {
    /// Human-readable description for headers and the UI.
    fn name(&self) -> String;

    /// Push every event that is ready onto `out` without blocking. Called
    /// once per pipeline tick.
    fn poll(&mut self, out: &mut Vec<FfbEvent>) -> Result<SourceStatus, BackendError>;

    /// Replay inputs read from the downstream device (a virtual device
    /// mirrors the real wheel's steering so the game still sees it move).
    fn inject_inputs(&mut self, _events: &[InputEvent]) -> Result<(), BackendError> {
        Ok(())
    }

    /// Steering position this source knows about (a recording carries axis
    /// samples), used when the sink cannot report one.
    fn axis_hint(&self) -> Option<f32> {
        None
    }

    /// Pause or resume (playback sources).
    fn set_paused(&mut self, _paused: bool) {}

    /// `(position_us, duration_us)` for playback sources.
    fn progress(&self) -> Option<(u64, u64)> {
        None
    }
}

/// Which representation a sink consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkMode {
    /// Forward every [`FfbEvent`]; the device renders effects itself.
    Effects,
    /// Only [`FfbSink::set_force`] is driven, with the renderer's output.
    /// For torque-mode APIs and constant-force-only outputs.
    Rendered,
}

/// Where FFB events go.
pub trait FfbSink: Send {
    fn name(&self) -> String;

    fn mode(&self) -> SinkMode;

    /// Apply one event ([`SinkMode::Effects`]).
    fn apply(&mut self, _event: &FfbEvent) -> Result<(), BackendError> {
        Ok(())
    }

    /// Drive the rendered force, `-1.0..=1.0` ([`SinkMode::Rendered`]).
    fn set_force(&mut self, _force: f32) -> Result<(), BackendError> {
        Ok(())
    }

    /// Current steering position `-1.0..=1.0`, if the device reports one.
    fn axis(&mut self) -> Option<f32> {
        None
    }

    /// Drain input events (axes, buttons) read from the device since the
    /// last call, for mirroring onto a virtual device.
    fn poll_inputs(&mut self, _out: &mut Vec<InputEvent>) {}

    /// Stop every effect and zero the force. Called when the pipeline ends.
    fn release(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

/// A sink that discards everything but remembers the last rendered force.
/// The graph-only configuration.
#[derive(Debug, Default)]
pub struct NullSink {
    pub last_force: f32,
    pub events: u64,
}

impl FfbSink for NullSink {
    fn name(&self) -> String {
        "none".into()
    }

    fn mode(&self) -> SinkMode {
        SinkMode::Rendered
    }

    fn apply(&mut self, _event: &FfbEvent) -> Result<(), BackendError> {
        self.events += 1;
        Ok(())
    }

    fn set_force(&mut self, force: f32) -> Result<(), BackendError> {
        self.last_force = force;
        Ok(())
    }
}

/// A sink that keeps every event it was given. For tests and dry runs.
#[derive(Debug, Default)]
pub struct CaptureSink {
    pub events: Vec<FfbEvent>,
    pub forces: Vec<f32>,
    pub released: bool,
    mode_rendered: bool,
}

impl CaptureSink {
    pub fn effects() -> Self {
        CaptureSink::default()
    }

    pub fn rendered() -> Self {
        CaptureSink { mode_rendered: true, ..Default::default() }
    }
}

impl FfbSink for CaptureSink {
    fn name(&self) -> String {
        "capture".into()
    }

    fn mode(&self) -> SinkMode {
        if self.mode_rendered {
            SinkMode::Rendered
        } else {
            SinkMode::Effects
        }
    }

    fn apply(&mut self, event: &FfbEvent) -> Result<(), BackendError> {
        self.events.push(*event);
        Ok(())
    }

    fn set_force(&mut self, force: f32) -> Result<(), BackendError> {
        self.forces.push(force);
        Ok(())
    }

    fn release(&mut self) -> Result<(), BackendError> {
        self.released = true;
        Ok(())
    }
}

/// A source that is never ready. Useful to drive a sink from the UI alone.
#[derive(Debug, Default)]
pub struct IdleSource;

impl FfbSource for IdleSource {
    fn name(&self) -> String {
        "idle".into()
    }

    fn poll(&mut self, _out: &mut Vec<FfbEvent>) -> Result<SourceStatus, BackendError> {
        Ok(SourceStatus::Active)
    }
}

/// A source that plays a scripted list of events on a schedule, in the
/// pipeline's own clock. Tests and the `demo` subcommand use it; it is
/// labelled as synthetic everywhere so a demo trace is never mistaken for a
/// recording of a real game.
#[derive(Debug)]
pub struct ScriptedSource {
    script: Vec<(u64, FfbEvent)>,
    next: usize,
    clock_us: u64,
    tick_us: u64,
    looped: bool,
}

impl ScriptedSource {
    /// `script` is `(t_us, event)`, sorted by time. `tick_hz` must match the
    /// pipeline so the source's clock advances one tick per poll.
    pub fn new(mut script: Vec<(u64, FfbEvent)>, tick_hz: u32, looped: bool) -> Self {
        script.sort_by_key(|(t, _)| *t);
        ScriptedSource { script, next: 0, clock_us: 0, tick_us: 1_000_000 / tick_hz.max(1) as u64, looped }
    }

    /// A constant force following a slow sine, updated at `update_hz`, for
    /// `seconds`. The classic shape of a game animating one constant effect.
    pub fn sine_demo(tick_hz: u32, update_hz: u32, seconds: f32, looped: bool) -> Self {
        let mut script = vec![(0, FfbEvent::Upload { id: 1, effect: crate::model::Effect::constant(0.0) }), (0, FfbEvent::Play { id: 1, count: 1 })];
        let updates = (seconds * update_hz as f32) as u64;
        for i in 1..=updates {
            let t = i as f32 / update_hz as f32;
            let v = 0.6 * (t * core::f32::consts::TAU * 0.5).sin() + 0.15 * (t * core::f32::consts::TAU * 7.0).sin();
            script.push((i * 1_000_000 / update_hz as u64, FfbEvent::Upload { id: 1, effect: crate::model::Effect::constant(v) }));
        }
        script.push((updates * 1_000_000 / update_hz as u64 + 1, FfbEvent::Stop { id: 1 }));
        ScriptedSource::new(script, tick_hz, looped)
    }
}

impl FfbSource for ScriptedSource {
    fn name(&self) -> String {
        "synthetic script".into()
    }

    fn poll(&mut self, out: &mut Vec<FfbEvent>) -> Result<SourceStatus, BackendError> {
        while let Some((t, ev)) = self.script.get(self.next) {
            if *t > self.clock_us {
                break;
            }
            out.push(*ev);
            self.next += 1;
        }
        self.clock_us += self.tick_us;
        if self.next >= self.script.len() {
            if self.looped && !self.script.is_empty() {
                self.next = 0;
                self.clock_us = 0;
                out.push(FfbEvent::Control(crate::model::DeviceControl::Reset));
                return Ok(SourceStatus::Active);
            }
            return Ok(SourceStatus::Finished);
        }
        Ok(SourceStatus::Active)
    }

    fn progress(&self) -> Option<(u64, u64)> {
        Some((self.clock_us, self.script.last().map(|(t, _)| *t).unwrap_or(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Effect;

    #[test]
    fn scripted_source_releases_events_on_time() {
        let script = vec![
            (0, FfbEvent::Upload { id: 1, effect: Effect::constant(0.5) }),
            (0, FfbEvent::Play { id: 1, count: 1 }),
            (2_000, FfbEvent::Stop { id: 1 }),
        ];
        let mut s = ScriptedSource::new(script, 1000, false);
        let mut out = Vec::new();
        assert_eq!(s.poll(&mut out).unwrap(), SourceStatus::Active);
        assert_eq!(out.len(), 2);
        out.clear();
        assert_eq!(s.poll(&mut out).unwrap(), SourceStatus::Active);
        assert!(out.is_empty());
        assert_eq!(s.poll(&mut out).unwrap(), SourceStatus::Finished);
        assert_eq!(out, vec![FfbEvent::Stop { id: 1 }]);
    }

    #[test]
    fn scripted_source_loops_with_a_reset() {
        let mut s = ScriptedSource::new(vec![(0, FfbEvent::Gain(0.5))], 1000, true);
        let mut out = Vec::new();
        assert_eq!(s.poll(&mut out).unwrap(), SourceStatus::Active);
        assert_eq!(out, vec![FfbEvent::Gain(0.5), FfbEvent::Control(crate::model::DeviceControl::Reset)]);
        out.clear();
        assert_eq!(s.poll(&mut out).unwrap(), SourceStatus::Active);
        assert_eq!(out[0], FfbEvent::Gain(0.5));
    }

    #[test]
    fn sine_demo_has_a_plausible_shape() {
        let s = ScriptedSource::sine_demo(1000, 60, 2.0, false);
        assert_eq!(s.script.len(), 2 + 120 + 1);
        assert_eq!(s.progress(), Some((0, 2_000_001)));
    }
}
