//! Renders the live effect table into one force value per tick.
//!
//! The downstream device does this in firmware; we do it again in software
//! for two reasons:
//!
//! 1. the graph: the force a game is asking for is a function of time (and,
//!    for condition effects, of the wheel position), not something a stream
//!    of PID reports shows directly;
//! 2. sinks that only take a scalar force (rMap's constant-force
//!    `FfbOutput`, a torque-mode wheelbase API) can play back any recording
//!    through [`Renderer::sample`].
//!
//! It is an approximation of what firmware does - every base scales
//! condition coefficients its own way - but constant, ramp and periodic
//! effects, which carry a game's actual steering torque, render exactly.
//!
//! ## Real-time contract
//! [`Renderer::apply`] and [`Renderer::sample`] are allocation- and
//! panic-free: the table is a fixed array and all arithmetic is clamped.

use crate::model::*;

/// Wheel axis state fed to condition effects. Position is `-1.0..=1.0` of
/// the axis range; velocity is in axis units per second (so `1.0` crosses
/// half the range in a second).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AxisState {
    pub position: f32,
    pub velocity: f32,
}

/// The rendered force for one tick, with its breakdown by effect family.
/// All values are after device gain; `total` is additionally clamped.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rendered {
    /// Combined force on the X axis, `-1.0..=1.0`. See the module docs of
    /// [`crate::model`] for the sign convention.
    pub total: f32,
    pub constant: f32,
    pub ramp: f32,
    pub periodic: f32,
    pub condition: f32,
    pub rumble: f32,
    /// Device gain in effect, `0.0..=1.0`.
    pub gain: f32,
    /// Effects currently playing (including ones inside their start delay).
    pub playing: u8,
    /// Effects defined in the table.
    pub loaded: u8,
    pub paused: bool,
    pub actuators_on: bool,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    effect: Effect,
    playing: bool,
    /// Time of the last `Play`, in the renderer's clock.
    started_us: i64,
    /// Repetitions requested by `Play` (`0` = loop forever).
    count: u32,
}

/// Fixed-size effect table plus device state.
#[derive(Debug, Clone)]
pub struct Renderer {
    slots: [Option<Slot>; MAX_EFFECTS],
    gain: f32,
    autocenter: f32,
    paused: bool,
    paused_at_us: i64,
    actuators_on: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Renderer {
            slots: [None; MAX_EFFECTS],
            gain: 1.0,
            autocenter: 0.0,
            paused: false,
            paused_at_us: 0,
            actuators_on: true,
        }
    }

    /// Forget everything, as a PID device reset does.
    pub fn reset(&mut self) {
        *self = Renderer::new();
    }

    pub fn gain(&self) -> f32 {
        self.gain
    }

    pub fn autocenter(&self) -> f32 {
        self.autocenter
    }

    /// The effect in slot `id`, if one is loaded.
    pub fn effect(&self, id: EffectId) -> Option<&Effect> {
        self.slots.get(id as usize).and_then(|s| s.as_ref()).map(|s| &s.effect)
    }

    pub fn is_playing(&self, id: EffectId) -> bool {
        self.slots.get(id as usize).and_then(|s| s.as_ref()).map(|s| s.playing).unwrap_or(false)
    }

    /// The events that would rebuild the current table on a fresh device:
    /// gain, autocentre, every loaded effect and a `Play` for each playing
    /// one. A recording started mid-session begins with these.
    pub fn snapshot(&self, out: &mut Vec<FfbEvent>) {
        if self.gain != 1.0 {
            out.push(FfbEvent::Gain(self.gain));
        }
        if self.autocenter != 0.0 {
            out.push(FfbEvent::Autocenter(self.autocenter));
        }
        if !self.actuators_on {
            out.push(FfbEvent::Control(DeviceControl::DisableActuators));
        }
        for (i, slot) in self.slots.iter().enumerate() {
            let Some(s) = slot else { continue };
            let id = i as EffectId;
            out.push(FfbEvent::Upload { id, effect: s.effect });
            if s.playing {
                out.push(FfbEvent::Play { id, count: s.count });
            }
        }
    }

    /// Feed one event, stamped with the renderer clock (microseconds).
    pub fn apply(&mut self, event: &FfbEvent, now_us: u64) {
        let now = now_us as i64;
        match *event {
            FfbEvent::Upload { id, effect } => {
                let Some(slot) = self.slots.get_mut(id as usize) else { return };
                match slot {
                    // Parameter update on a live effect keeps its clock.
                    Some(s) => s.effect = effect,
                    None => *slot = Some(Slot { effect, playing: false, started_us: now, count: 1 }),
                }
            }
            FfbEvent::Erase { id } => {
                if let Some(slot) = self.slots.get_mut(id as usize) {
                    *slot = None;
                }
            }
            FfbEvent::Play { id, count } => {
                if let Some(Some(s)) = self.slots.get_mut(id as usize) {
                    s.playing = true;
                    s.started_us = now;
                    s.count = count;
                }
            }
            FfbEvent::Stop { id } => {
                if let Some(Some(s)) = self.slots.get_mut(id as usize) {
                    s.playing = false;
                }
            }
            FfbEvent::Gain(g) => self.gain = clamp_unit(g),
            FfbEvent::Autocenter(a) => self.autocenter = clamp_unit(a),
            FfbEvent::Control(c) => match c {
                DeviceControl::Reset => self.reset(),
                DeviceControl::Pause => {
                    if !self.paused {
                        self.paused = true;
                        self.paused_at_us = now;
                    }
                }
                DeviceControl::Continue => {
                    if self.paused {
                        self.paused = false;
                        let gap = now - self.paused_at_us;
                        for s in self.slots.iter_mut().flatten() {
                            s.started_us += gap;
                        }
                    }
                }
                DeviceControl::StopAll => {
                    for s in self.slots.iter_mut().flatten() {
                        s.playing = false;
                    }
                }
                DeviceControl::EnableActuators => self.actuators_on = true,
                DeviceControl::DisableActuators => self.actuators_on = false,
            },
        }
    }

    /// Render the force at `now_us`. Effects that have run out are retired
    /// here, so callers should sample every tick even when they discard the
    /// value.
    pub fn sample(&mut self, now_us: u64, axis: Option<AxisState>) -> Rendered {
        let now = now_us as i64;
        let mut out = Rendered {
            gain: self.gain,
            paused: self.paused,
            actuators_on: self.actuators_on,
            ..Default::default()
        };

        for slot in self.slots.iter_mut() {
            let Some(s) = slot else { continue };
            out.loaded = out.loaded.saturating_add(1);
            if !s.playing {
                continue;
            }
            if self.paused {
                out.playing = out.playing.saturating_add(1);
                continue;
            }
            let elapsed = now - s.started_us - (s.effect.delay_ms as i64) * 1000;
            if elapsed < 0 {
                // Inside the start delay.
                out.playing = out.playing.saturating_add(1);
                continue;
            }
            // Position within the current repetition, in ms.
            let (local_ms, duration_ms) = match s.effect.duration_ms {
                Some(0) | None => (elapsed as f32 / 1000.0, None),
                Some(d) => {
                    let d_us = (d as i64) * 1000;
                    if s.count != 0 && elapsed >= d_us * (s.count as i64) {
                        s.playing = false;
                        continue;
                    }
                    ((elapsed % d_us) as f32 / 1000.0, Some(d as f32))
                }
            };
            out.playing = out.playing.saturating_add(1);

            let env = envelope_level(&s.effect.kind.envelope(), local_ms, duration_ms);
            let x = s.effect.x_component();
            match s.effect.kind {
                EffectKind::Constant { magnitude, .. } => {
                    out.constant += clamp_signed(magnitude) * env * x;
                }
                EffectKind::Ramp { start, end, .. } => {
                    let v = match duration_ms {
                        Some(d) if d > 0.0 => {
                            let f = (local_ms / d).clamp(0.0, 1.0);
                            clamp_signed(start) + (clamp_signed(end) - clamp_signed(start)) * f
                        }
                        _ => clamp_signed(start),
                    };
                    out.ramp += v * env * x;
                }
                EffectKind::Periodic { waveform, magnitude, offset, phase, period_ms, .. } => {
                    let wave = if period_ms == 0 {
                        0.0
                    } else {
                        waveform.eval(phase + local_ms / period_ms as f32)
                    };
                    out.periodic += (clamp_signed(offset) + clamp_signed(magnitude) * env * wave) * x;
                }
                EffectKind::Condition { kind, axes } => {
                    if let Some(a) = axis {
                        out.condition += condition_force(kind, &axes[0], a);
                    }
                }
                EffectKind::Rumble { strong, weak } => {
                    out.rumble += clamp_unit(strong).max(clamp_unit(weak) * 0.5) * env;
                }
            }
        }

        if self.autocenter > 0.0 {
            if let Some(a) = axis {
                out.condition += -self.autocenter * clamp_signed(a.position);
            }
        }

        let g = if self.paused || !self.actuators_on { 0.0 } else { self.gain };
        out.constant *= g;
        out.ramp *= g;
        out.periodic *= g;
        out.condition *= g;
        out.rumble *= g;
        out.total = clamp_signed(out.constant + out.ramp + out.periodic + out.condition + out.rumble);
        out
    }
}

/// Envelope multiplier at `local_ms` into a repetition of length
/// `duration_ms` (`None` = infinite: no fade).
fn envelope_level(env: &Envelope, local_ms: f32, duration_ms: Option<f32>) -> f32 {
    let mut level = 1.0_f32;
    if env.attack_ms > 0 && local_ms < env.attack_ms as f32 {
        let a = clamp_unit(env.attack_level);
        level = level.min(a + (1.0 - a) * (local_ms / env.attack_ms as f32));
    }
    if let Some(d) = duration_ms {
        let remaining = d - local_ms;
        if env.fade_ms > 0 && remaining < env.fade_ms as f32 {
            let f = clamp_unit(env.fade_level);
            level = level.min(f + (1.0 - f) * (remaining.max(0.0) / env.fade_ms as f32));
        }
    }
    clamp_unit(level)
}

/// Force from one condition axis. Positive coefficients resist motion
/// (spring back to centre, damp velocity), which is what every wheel does
/// with a positive-coefficient spring from a game.
fn condition_force(kind: ConditionKind, c: &Condition, axis: AxisState) -> f32 {
    let q = match kind {
        ConditionKind::Spring => {
            let disp = clamp_signed(axis.position) - clamp_signed(c.center);
            let dead = clamp_unit(c.deadband);
            if disp.abs() <= dead {
                return 0.0;
            }
            disp - dead * disp.signum()
        }
        ConditionKind::Damper => {
            if !axis.velocity.is_finite() {
                return 0.0;
            }
            axis.velocity
        }
        // No acceleration estimate is available; inertia renders as zero.
        ConditionKind::Inertia => return 0.0,
        ConditionKind::Friction => {
            if !axis.velocity.is_finite() || axis.velocity.abs() < 1e-3 {
                return 0.0;
            }
            axis.velocity.signum()
        }
    };
    let (coeff, sat) = if q > 0.0 {
        (clamp_signed(c.right_coeff), clamp_unit(c.right_sat))
    } else {
        (clamp_signed(c.left_coeff), clamp_unit(c.left_sat))
    };
    let f = -coeff * q;
    if sat > 0.0 {
        f.clamp(-sat, sat)
    } else {
        clamp_signed(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant_playing(mag: f32) -> Renderer {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Upload { id: 1, effect: Effect::constant(mag) }, 0);
        r.apply(&FfbEvent::Play { id: 1, count: 1 }, 0);
        r
    }

    #[test]
    fn constant_force_renders_its_magnitude() {
        let mut r = constant_playing(0.5);
        let s = r.sample(10_000, None);
        assert!((s.total - 0.5).abs() < 1e-6);
        assert!((s.constant - 0.5).abs() < 1e-6);
        assert_eq!(s.playing, 1);
        assert_eq!(s.loaded, 1);
    }

    #[test]
    fn loaded_but_not_started_is_silent() {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Upload { id: 3, effect: Effect::constant(1.0) }, 0);
        let s = r.sample(1000, None);
        assert_eq!(s.total, 0.0);
        assert_eq!(s.loaded, 1);
        assert_eq!(s.playing, 0);
    }

    #[test]
    fn upload_while_playing_updates_without_restart() {
        let mut r = constant_playing(0.2);
        r.apply(&FfbEvent::Upload { id: 1, effect: Effect::constant(-0.7) }, 5_000);
        let s = r.sample(6_000, None);
        assert!((s.total + 0.7).abs() < 1e-6);
        assert!(r.is_playing(1));
    }

    #[test]
    fn direction_flips_sign() {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Upload { id: 0, effect: Effect::constant(1.0).with_direction(270.0) }, 0);
        r.apply(&FfbEvent::Play { id: 0, count: 1 }, 0);
        assert!((r.sample(100, None).total + 1.0).abs() < 1e-6);
    }

    #[test]
    fn gain_scales_and_clamps() {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Upload { id: 1, effect: Effect::constant(0.8) }, 0);
        r.apply(&FfbEvent::Upload { id: 2, effect: Effect::constant(0.8) }, 0);
        r.apply(&FfbEvent::Play { id: 1, count: 1 }, 0);
        r.apply(&FfbEvent::Play { id: 2, count: 1 }, 0);
        assert!((r.sample(0, None).total - 1.0).abs() < 1e-6, "sum clamps to 1");
        r.apply(&FfbEvent::Gain(0.5), 0);
        assert!((r.sample(0, None).total - 0.8).abs() < 1e-6);
    }

    #[test]
    fn finite_duration_retires_the_effect() {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Upload { id: 1, effect: Effect::constant(1.0).with_duration(Some(100)) }, 0);
        r.apply(&FfbEvent::Play { id: 1, count: 1 }, 0);
        assert!(r.sample(50_000, None).total > 0.99);
        assert_eq!(r.sample(100_000, None).total, 0.0);
        assert!(!r.is_playing(1));
        // Two repetitions: still playing at 150 ms.
        r.apply(&FfbEvent::Play { id: 1, count: 2 }, 0);
        assert!(r.sample(150_000, None).total > 0.99);
        assert_eq!(r.sample(200_000, None).total, 0.0);
        // Infinite repeats never retire.
        r.apply(&FfbEvent::Play { id: 1, count: 0 }, 0);
        assert!(r.sample(5_000_000, None).total > 0.99);
    }

    #[test]
    fn start_delay_holds_zero() {
        let mut r = Renderer::new();
        let mut e = Effect::constant(1.0);
        e.delay_ms = 20;
        r.apply(&FfbEvent::Upload { id: 1, effect: e }, 0);
        r.apply(&FfbEvent::Play { id: 1, count: 1 }, 0);
        let s = r.sample(10_000, None);
        assert_eq!(s.total, 0.0);
        assert_eq!(s.playing, 1);
        assert!(r.sample(25_000, None).total > 0.99);
    }

    #[test]
    fn envelope_attack_and_fade() {
        let env = Envelope { attack_level: 0.0, attack_ms: 100, fade_level: 0.5, fade_ms: 100 };
        assert!((envelope_level(&env, 0.0, Some(1000.0)) - 0.0).abs() < 1e-6);
        assert!((envelope_level(&env, 50.0, Some(1000.0)) - 0.5).abs() < 1e-6);
        assert!((envelope_level(&env, 500.0, Some(1000.0)) - 1.0).abs() < 1e-6);
        assert!((envelope_level(&env, 950.0, Some(1000.0)) - 0.75).abs() < 1e-6);
        assert!((envelope_level(&env, 1000.0, Some(1000.0)) - 0.5).abs() < 1e-6);
        // Infinite duration: no fade.
        assert!((envelope_level(&env, 1e9, None) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ramp_interpolates_over_duration() {
        let mut r = Renderer::new();
        let e = Effect::with_kind(EffectKind::Ramp { start: -1.0, end: 1.0, envelope: Envelope::default() })
            .with_duration(Some(1000));
        r.apply(&FfbEvent::Upload { id: 1, effect: e }, 0);
        r.apply(&FfbEvent::Play { id: 1, count: 1 }, 0);
        assert!((r.sample(0, None).total + 1.0).abs() < 1e-6);
        assert!(r.sample(500_000, None).total.abs() < 1e-3);
        assert!((r.sample(999_000, None).total - 0.998).abs() < 1e-3);
    }

    #[test]
    fn periodic_sine_follows_its_period() {
        let mut r = Renderer::new();
        let e = Effect::with_kind(EffectKind::Periodic {
            waveform: Waveform::Sine,
            magnitude: 0.5,
            offset: 0.25,
            phase: 0.0,
            period_ms: 100,
            envelope: Envelope::default(),
        });
        r.apply(&FfbEvent::Upload { id: 1, effect: e }, 0);
        r.apply(&FfbEvent::Play { id: 1, count: 1 }, 0);
        assert!((r.sample(0, None).total - 0.25).abs() < 1e-5);
        assert!((r.sample(25_000, None).total - 0.75).abs() < 1e-5);
        assert!((r.sample(75_000, None).total + 0.25).abs() < 1e-5);
        assert!((r.sample(100_000, None).total - 0.25).abs() < 1e-4);
    }

    #[test]
    fn spring_restores_towards_centre() {
        let mut r = Renderer::new();
        let axis = Condition { right_coeff: 1.0, left_coeff: 1.0, right_sat: 1.0, left_sat: 1.0, ..Default::default() };
        let e = Effect::with_kind(EffectKind::Condition { kind: ConditionKind::Spring, axes: [axis, axis] });
        r.apply(&FfbEvent::Upload { id: 1, effect: e }, 0);
        r.apply(&FfbEvent::Play { id: 1, count: 1 }, 0);
        let right = r.sample(0, Some(AxisState { position: 0.5, velocity: 0.0 }));
        assert!((right.condition + 0.5).abs() < 1e-6, "{right:?}");
        let left = r.sample(0, Some(AxisState { position: -0.25, velocity: 0.0 }));
        assert!((left.condition - 0.25).abs() < 1e-6);
        // Without an axis reading, conditions contribute nothing.
        assert_eq!(r.sample(0, None).condition, 0.0);
    }

    #[test]
    fn spring_deadband_and_saturation() {
        let c = Condition { deadband: 0.1, right_coeff: 1.0, left_coeff: 1.0, right_sat: 0.3, left_sat: 0.3, ..Default::default() };
        assert_eq!(condition_force(ConditionKind::Spring, &c, AxisState { position: 0.05, velocity: 0.0 }), 0.0);
        let f = condition_force(ConditionKind::Spring, &c, AxisState { position: 0.2, velocity: 0.0 });
        assert!((f + 0.1).abs() < 1e-6, "{f}");
        let f = condition_force(ConditionKind::Spring, &c, AxisState { position: 1.0, velocity: 0.0 });
        assert!((f + 0.3).abs() < 1e-6, "saturates: {f}");
    }

    #[test]
    fn damper_and_friction_oppose_velocity() {
        let c = Condition { right_coeff: 0.5, left_coeff: 0.5, ..Default::default() };
        let f = condition_force(ConditionKind::Damper, &c, AxisState { position: 0.0, velocity: 1.0 });
        assert!((f + 0.5).abs() < 1e-6);
        let f = condition_force(ConditionKind::Friction, &c, AxisState { position: 0.0, velocity: -3.0 });
        assert!((f - 0.5).abs() < 1e-6);
        assert_eq!(condition_force(ConditionKind::Inertia, &c, AxisState { position: 0.0, velocity: 1.0 }), 0.0);
    }

    #[test]
    fn pause_freezes_and_continue_resumes_timeline() {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Upload { id: 1, effect: Effect::constant(1.0).with_duration(Some(100)) }, 0);
        r.apply(&FfbEvent::Play { id: 1, count: 1 }, 0);
        r.apply(&FfbEvent::Control(DeviceControl::Pause), 50_000);
        let s = r.sample(500_000, None);
        assert_eq!(s.total, 0.0);
        assert!(s.paused);
        r.apply(&FfbEvent::Control(DeviceControl::Continue), 1_000_000);
        // 50 ms consumed before the pause; 40 ms more keeps it alive.
        assert!(r.sample(1_040_000, None).total > 0.99);
        assert_eq!(r.sample(1_060_000, None).total, 0.0);
    }

    #[test]
    fn reset_and_stop_all() {
        let mut r = constant_playing(1.0);
        r.apply(&FfbEvent::Control(DeviceControl::StopAll), 0);
        let s = r.sample(0, None);
        assert_eq!(s.total, 0.0);
        assert_eq!(s.loaded, 1);
        r.apply(&FfbEvent::Gain(0.2), 0);
        r.apply(&FfbEvent::Control(DeviceControl::Reset), 0);
        let s = r.sample(0, None);
        assert_eq!(s.loaded, 0);
        assert_eq!(s.gain, 1.0);
    }

    #[test]
    fn actuators_off_silences_output() {
        let mut r = constant_playing(1.0);
        r.apply(&FfbEvent::Control(DeviceControl::DisableActuators), 0);
        let s = r.sample(0, None);
        assert_eq!(s.total, 0.0);
        assert!(!s.actuators_on);
        r.apply(&FfbEvent::Control(DeviceControl::EnableActuators), 0);
        assert!(r.sample(0, None).total > 0.99);
    }

    #[test]
    fn out_of_range_ids_are_ignored() {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Play { id: 200, count: 1 }, 0);
        r.apply(&FfbEvent::Erase { id: 255 }, 0);
        assert_eq!(r.sample(0, None).loaded, 0);
    }

    #[test]
    fn snapshot_rebuilds_the_table() {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Gain(0.5), 0);
        r.apply(&FfbEvent::Upload { id: 2, effect: Effect::constant(0.3) }, 0);
        r.apply(&FfbEvent::Upload { id: 5, effect: Effect::constant(-0.3) }, 0);
        r.apply(&FfbEvent::Play { id: 5, count: 3 }, 0);
        let mut out = Vec::new();
        r.snapshot(&mut out);
        assert_eq!(
            out,
            vec![
                FfbEvent::Gain(0.5),
                FfbEvent::Upload { id: 2, effect: Effect::constant(0.3) },
                FfbEvent::Upload { id: 5, effect: Effect::constant(-0.3) },
                FfbEvent::Play { id: 5, count: 3 },
            ]
        );
        let mut r2 = Renderer::new();
        for ev in &out {
            r2.apply(ev, 0);
        }
        assert_eq!(r2.sample(0, None), r.sample(0, None));
    }

    #[test]
    fn autocenter_renders_as_a_spring() {
        let mut r = Renderer::new();
        r.apply(&FfbEvent::Autocenter(0.5), 0);
        let s = r.sample(0, Some(AxisState { position: 1.0, velocity: 0.0 }));
        assert!((s.condition + 0.5).abs() < 1e-6);
    }
}
