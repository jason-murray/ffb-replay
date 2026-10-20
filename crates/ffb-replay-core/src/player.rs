//! Plays a [`Recording`] back as an [`FfbSource`], on its original timeline
//! (or a multiple of it), optionally looping.

use crate::format::{Record, RecordBody, Recording};
use crate::io::{BackendError, FfbSource, SourceStatus};
use crate::model::{DeviceControl, FfbEvent};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerOptions {
    /// Playback speed multiplier (`1.0` = real time).
    pub speed: f32,
    pub looped: bool,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        PlayerOptions { speed: 1.0, looped: false }
    }
}

pub struct Player {
    records: Vec<Record>,
    duration_us: u64,
    name: String,
    options: PlayerOptions,
    next: usize,
    /// Wall-clock origin of the current pass; `None` until the first poll.
    origin: Option<Instant>,
    /// Timeline position accumulated before a pause.
    paused_at_us: Option<u64>,
    last_axis: Option<f32>,
    pub loops_completed: u32,
}

impl Player {
    pub fn new(recording: Recording, name: impl Into<String>, options: PlayerOptions) -> Self {
        let duration_us = recording.duration_us();
        let mut records = recording.records;
        records.sort_by_key(|r| r.t_us);
        Player {
            records,
            duration_us,
            name: name.into(),
            options: PlayerOptions {
                speed: if options.speed.is_finite() && options.speed > 0.0 {
                    options.speed
                } else {
                    1.0
                },
                ..options
            },
            next: 0,
            origin: None,
            paused_at_us: None,
            last_axis: None,
            loops_completed: 0,
        }
    }

    pub fn options(&self) -> PlayerOptions {
        self.options
    }

    pub fn set_looped(&mut self, looped: bool) {
        self.options.looped = looped;
    }

    pub fn duration_us(&self) -> u64 {
        self.duration_us
    }

    /// Recording-timeline position of the current pass.
    pub fn position_us(&self) -> u64 {
        if let Some(p) = self.paused_at_us {
            return p;
        }
        match self.origin {
            None => 0,
            Some(o) => ((o.elapsed().as_secs_f64() * self.options.speed as f64) * 1e6) as u64,
        }
    }

    fn restart(&mut self) {
        self.next = 0;
        self.origin = Some(Instant::now());
        self.paused_at_us = None;
    }
}

impl FfbSource for Player {
    fn name(&self) -> String {
        format!("playback: {}", self.name)
    }

    fn poll(&mut self, out: &mut Vec<FfbEvent>) -> Result<SourceStatus, BackendError> {
        if self.origin.is_none() {
            self.restart();
        }
        if self.paused_at_us.is_some() {
            return Ok(SourceStatus::Active);
        }
        let pos = self.position_us();
        while let Some(r) = self.records.get(self.next) {
            if r.t_us > pos {
                break;
            }
            match r.body {
                RecordBody::Ffb(ev) => out.push(ev),
                RecordBody::Axis(a) => self.last_axis = Some(a),
            }
            self.next += 1;
        }
        if self.next >= self.records.len() && pos >= self.duration_us {
            if self.options.looped {
                self.loops_completed += 1;
                out.push(FfbEvent::Control(DeviceControl::Reset));
                self.restart();
                return Ok(SourceStatus::Active);
            }
            return Ok(SourceStatus::Finished);
        }
        Ok(SourceStatus::Active)
    }

    fn axis_hint(&self) -> Option<f32> {
        self.last_axis
    }

    fn set_paused(&mut self, paused: bool) {
        match (paused, self.paused_at_us) {
            (true, None) => self.paused_at_us = Some(self.position_us()),
            (false, Some(p)) => {
                // Move the origin so the timeline resumes from `p`.
                let back =
                    std::time::Duration::from_secs_f64(p as f64 / 1e6 / self.options.speed as f64);
                self.origin = Instant::now().checked_sub(back);
                if self.origin.is_none() {
                    self.restart();
                }
                self.paused_at_us = None;
            }
            _ => {}
        }
    }

    fn progress(&self) -> Option<(u64, u64)> {
        Some((self.position_us().min(self.duration_us), self.duration_us))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Header;
    use crate::model::Effect;
    use std::thread::sleep;
    use std::time::Duration;

    fn recording() -> Recording {
        Recording {
            header: Header::default(),
            records: vec![
                Record {
                    t_us: 0,
                    body: RecordBody::Ffb(FfbEvent::Upload {
                        id: 1,
                        effect: Effect::constant(0.3),
                    }),
                },
                Record { t_us: 0, body: RecordBody::Ffb(FfbEvent::Play { id: 1, count: 1 }) },
                Record { t_us: 20_000, body: RecordBody::Axis(0.4) },
                Record { t_us: 40_000, body: RecordBody::Ffb(FfbEvent::Stop { id: 1 }) },
            ],
        }
    }

    #[test]
    fn yields_events_as_the_timeline_passes_them() {
        let mut p = Player::new(recording(), "r", PlayerOptions::default());
        let mut out = Vec::new();
        assert_eq!(p.poll(&mut out).unwrap(), SourceStatus::Active);
        assert_eq!(out.len(), 2, "t=0 events come out on the first poll");
        assert_eq!(p.axis_hint(), None);
        out.clear();
        sleep(Duration::from_millis(60));
        assert_eq!(p.poll(&mut out).unwrap(), SourceStatus::Finished);
        assert_eq!(out, vec![FfbEvent::Stop { id: 1 }]);
        assert_eq!(p.axis_hint(), Some(0.4));
        assert_eq!(p.progress(), Some((40_000, 40_000)));
    }

    #[test]
    fn speed_scales_the_timeline() {
        let mut p = Player::new(recording(), "r", PlayerOptions { speed: 4.0, looped: false });
        let mut out = Vec::new();
        p.poll(&mut out).unwrap();
        sleep(Duration::from_millis(15));
        out.clear();
        assert_eq!(
            p.poll(&mut out).unwrap(),
            SourceStatus::Finished,
            "40 ms of recording in ~10 ms wall time"
        );
    }

    #[test]
    fn looping_resets_and_restarts() {
        let mut p = Player::new(recording(), "r", PlayerOptions { speed: 8.0, looped: true });
        let mut out = Vec::new();
        p.poll(&mut out).unwrap();
        sleep(Duration::from_millis(10));
        out.clear();
        assert_eq!(p.poll(&mut out).unwrap(), SourceStatus::Active);
        assert_eq!(out.last(), Some(&FfbEvent::Control(DeviceControl::Reset)));
        assert_eq!(p.loops_completed, 1);
        out.clear();
        p.poll(&mut out).unwrap();
        assert!(matches!(out.first(), Some(FfbEvent::Upload { id: 1, .. })));
    }

    #[test]
    fn pause_holds_position() {
        let mut p = Player::new(recording(), "r", PlayerOptions::default());
        let mut out = Vec::new();
        p.poll(&mut out).unwrap();
        p.set_paused(true);
        let at = p.position_us();
        sleep(Duration::from_millis(30));
        out.clear();
        assert_eq!(p.poll(&mut out).unwrap(), SourceStatus::Active);
        assert!(out.is_empty());
        assert_eq!(p.position_us(), at);
        p.set_paused(false);
        assert!(p.position_us() < at + 5_000, "resumes from where it paused");
    }
}
