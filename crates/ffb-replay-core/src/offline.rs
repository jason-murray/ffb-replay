//! Render a whole recording without a device or a clock: the force trace
//! as it would have been plotted, at a chosen rate. Used by the `info` and
//! `export` tools and handy for tests and analysis.

use crate::format::{RecordBody, Recording};
use crate::render::{AxisState, Rendered, Renderer};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TracePoint {
    pub t_us: u64,
    pub rendered: Rendered,
    pub axis: Option<f32>,
}

/// Replay `recording` through a [`Renderer`] at `rate_hz`, returning one
/// point per tick from `t = 0` to the end of the recording.
pub fn render_trace(recording: &Recording, rate_hz: u32) -> Vec<TracePoint> {
    let rate = rate_hz.clamp(1, 100_000) as u64;
    let step_us = 1_000_000 / rate;
    let dt = step_us as f32 / 1e6;
    let end = recording.duration_us();
    let mut records = recording.records.to_vec();
    records.sort_by_key(|r| r.t_us);

    let mut renderer = Renderer::new();
    let mut out = Vec::with_capacity((end / step_us + 2) as usize);
    let mut next = 0usize;
    let mut axis: Option<f32> = None;
    let mut prev_axis: Option<f32> = None;
    let mut velocity = 0.0f32;
    let mut t = 0u64;
    loop {
        while let Some(r) = records.get(next) {
            if r.t_us > t {
                break;
            }
            match r.body {
                RecordBody::Ffb(ev) => renderer.apply(&ev, t),
                RecordBody::Axis(a) => axis = Some(a),
            }
            next += 1;
        }
        let axis_state = axis.map(|p| {
            let raw = match prev_axis {
                Some(q) => (p - q) / dt,
                None => 0.0,
            };
            velocity += 0.2 * (raw - velocity);
            AxisState { position: p, velocity }
        });
        prev_axis = axis;
        out.push(TracePoint { t_us: t, rendered: renderer.sample(t, axis_state), axis });
        if t >= end {
            break;
        }
        t += step_us;
    }
    out
}

/// Per-kind event counts and peak values, for `info`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub uploads: u64,
    pub erases: u64,
    pub plays: u64,
    pub stops: u64,
    pub gains: u64,
    pub autocenters: u64,
    pub controls: u64,
    pub axis_samples: u64,
    /// Effect type names seen in uploads, in first-seen order.
    pub effect_types: Vec<&'static str>,
    pub peak_force: f32,
    pub mean_abs_force: f32,
    /// Share of trace points at or beyond `0.999`.
    pub clipped_fraction: f32,
}

pub fn stats(recording: &Recording, trace_rate_hz: u32) -> Stats {
    use crate::model::FfbEvent::*;
    let mut s = Stats::default();
    for r in &recording.records {
        match r.body {
            RecordBody::Axis(_) => s.axis_samples += 1,
            RecordBody::Ffb(ev) => match ev {
                Upload { effect, .. } => {
                    s.uploads += 1;
                    let name = effect.kind.type_str();
                    if !s.effect_types.contains(&name) {
                        s.effect_types.push(name);
                    }
                }
                Erase { .. } => s.erases += 1,
                Play { .. } => s.plays += 1,
                Stop { .. } => s.stops += 1,
                Gain(_) => s.gains += 1,
                Autocenter(_) => s.autocenters += 1,
                Control(_) => s.controls += 1,
            },
        }
    }
    let trace = render_trace(recording, trace_rate_hz);
    if !trace.is_empty() {
        let mut sum = 0.0f64;
        let mut clipped = 0usize;
        for p in &trace {
            let a = p.rendered.total.abs();
            s.peak_force = s.peak_force.max(a);
            sum += a as f64;
            if a >= 0.999 {
                clipped += 1;
            }
        }
        s.mean_abs_force = (sum / trace.len() as f64) as f32;
        s.clipped_fraction = clipped as f32 / trace.len() as f32;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{Header, Record};
    use crate::model::{Effect, FfbEvent};

    fn recording() -> Recording {
        Recording {
            header: Header::default(),
            records: vec![
                Record {
                    t_us: 0,
                    body: RecordBody::Ffb(FfbEvent::Upload {
                        id: 1,
                        effect: Effect::constant(0.5),
                    }),
                },
                Record { t_us: 0, body: RecordBody::Ffb(FfbEvent::Play { id: 1, count: 1 }) },
                Record { t_us: 5_000, body: RecordBody::Axis(0.25) },
                Record {
                    t_us: 10_000,
                    body: RecordBody::Ffb(FfbEvent::Upload {
                        id: 1,
                        effect: Effect::constant(1.0),
                    }),
                },
                Record { t_us: 20_000, body: RecordBody::Ffb(FfbEvent::Stop { id: 1 }) },
            ],
        }
    }

    #[test]
    fn trace_follows_the_events() {
        let t = render_trace(&recording(), 1000);
        assert_eq!(t.len(), 21);
        assert!((t[0].rendered.total - 0.5).abs() < 1e-6);
        assert_eq!(t[4].axis, None);
        assert_eq!(t[5].axis, Some(0.25));
        assert!((t[10].rendered.total - 1.0).abs() < 1e-6);
        assert_eq!(t[20].rendered.total, 0.0);
    }

    #[test]
    fn stats_count_and_measure() {
        let s = stats(&recording(), 1000);
        assert_eq!(s.uploads, 2);
        assert_eq!(s.plays, 1);
        assert_eq!(s.stops, 1);
        assert_eq!(s.axis_samples, 1);
        assert_eq!(s.effect_types, vec!["constant"]);
        assert!((s.peak_force - 1.0).abs() < 1e-6);
        assert!(s.clipped_fraction > 0.4 && s.clipped_fraction < 0.6, "{}", s.clipped_fraction);
    }
}
