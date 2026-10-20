//! The device-independent force-feedback vocabulary.
//!
//! Every backend (Linux evdev, DirectInput, vJoy's FFB receiver, ...) speaks
//! some dialect of the USB HID *Physical Interface Device* (PID) class: a
//! table of effect slots, each holding one effect definition, plus a handful
//! of device-level commands (gain, reset, pause). This module is that
//! vocabulary with the units normalised:
//!
//! - magnitudes, coefficients and levels are `f32` in `-1.0..=1.0`
//!   (or `0.0..=1.0` where only positive makes sense),
//! - times are whole milliseconds, with `None` for "infinite",
//! - directions are degrees, HID PID convention: `0` is "north" (+Y),
//!   `90` is +X. For a steering wheel (one axis, X) `90` is full positive
//!   force and `270` full negative.
//!
//! Everything here is `Copy` and heap-free so it can cross a real-time loop
//! and sit in a fixed-size effect table without allocating.

/// Index of an effect slot. Backends map it onto their own handle (an evdev
/// effect id, a DirectInput effect object, a vJoy effect block index).
pub type EffectId = u8;

/// Highest effect slot index supported by the renderer's fixed table.
pub const MAX_EFFECTS: usize = 128;

/// Shape of a periodic effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waveform {
    Sine,
    Square,
    Triangle,
    SawUp,
    SawDown,
}

impl Waveform {
    pub fn as_str(self) -> &'static str {
        match self {
            Waveform::Sine => "sine",
            Waveform::Square => "square",
            Waveform::Triangle => "triangle",
            Waveform::SawUp => "saw-up",
            Waveform::SawDown => "saw-down",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "sine" => Waveform::Sine,
            "square" => Waveform::Square,
            "triangle" => Waveform::Triangle,
            "saw-up" => Waveform::SawUp,
            "saw-down" => Waveform::SawDown,
            _ => return None,
        })
    }

    /// Waveform value at `phase` turns (`0.0..1.0`), in `-1.0..=1.0`.
    pub fn eval(self, phase: f32) -> f32 {
        let p = phase.rem_euclid(1.0);
        match self {
            Waveform::Sine => (p * core::f32::consts::TAU).sin(),
            Waveform::Square => {
                if p < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Waveform::Triangle => {
                // 0 → 1 over the first quarter, down to -1 at 3/4, back to 0.
                if p < 0.25 {
                    p * 4.0
                } else if p < 0.75 {
                    2.0 - p * 4.0
                } else {
                    p * 4.0 - 4.0
                }
            }
            Waveform::SawUp => p * 2.0 - 1.0,
            Waveform::SawDown => 1.0 - p * 2.0,
        }
    }
}

/// Which quantity a condition effect reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionKind {
    /// Force proportional to displacement from the centre.
    Spring,
    /// Force proportional to velocity.
    Damper,
    /// Force proportional to acceleration.
    Inertia,
    /// Constant force opposing motion.
    Friction,
}

impl ConditionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConditionKind::Spring => "spring",
            ConditionKind::Damper => "damper",
            ConditionKind::Inertia => "inertia",
            ConditionKind::Friction => "friction",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "spring" => ConditionKind::Spring,
            "damper" => ConditionKind::Damper,
            "inertia" => ConditionKind::Inertia,
            "friction" => ConditionKind::Friction,
            _ => return None,
        })
    }
}

/// Attack/fade envelope applied to an effect's magnitude.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Envelope {
    /// Magnitude at the start of the attack, as a fraction `0.0..=1.0` of
    /// the effect magnitude.
    pub attack_level: f32,
    /// Attack length (ms). `0` disables the attack.
    pub attack_ms: u32,
    /// Magnitude at the end of the fade, as a fraction `0.0..=1.0`.
    pub fade_level: f32,
    /// Fade length (ms) before the effect ends. `0` disables the fade; so
    /// does an infinite duration.
    pub fade_ms: u32,
}

impl Envelope {
    pub fn is_flat(&self) -> bool {
        self.attack_ms == 0 && self.fade_ms == 0
    }
}

/// One axis of a condition effect.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Condition {
    /// Centre point offset, `-1.0..=1.0` of the axis range.
    pub center: f32,
    /// Half-width of the zone around `center` where no force applies,
    /// `0.0..=1.0`.
    pub deadband: f32,
    /// Coefficient on the positive side of the centre, `-1.0..=1.0`.
    pub right_coeff: f32,
    /// Coefficient on the negative side of the centre, `-1.0..=1.0`.
    pub left_coeff: f32,
    /// Maximum force on the positive side, `0.0..=1.0`.
    pub right_sat: f32,
    /// Maximum force on the negative side, `0.0..=1.0`.
    pub left_sat: f32,
}

/// The type-specific half of an effect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EffectKind {
    Constant {
        /// `-1.0..=1.0`, signed; combined with [`Effect::direction`].
        magnitude: f32,
        envelope: Envelope,
    },
    Ramp {
        /// Magnitude at the start of the effect, `-1.0..=1.0`.
        start: f32,
        /// Magnitude at the end of the (finite) duration, `-1.0..=1.0`.
        end: f32,
        envelope: Envelope,
    },
    Periodic {
        waveform: Waveform,
        /// Peak magnitude `-1.0..=1.0`.
        magnitude: f32,
        /// DC offset `-1.0..=1.0`.
        offset: f32,
        /// Starting phase in turns, `0.0..1.0`.
        phase: f32,
        /// Period in milliseconds; `0` is treated as a hold at `offset`.
        period_ms: u32,
        envelope: Envelope,
    },
    Condition {
        kind: ConditionKind,
        /// One entry per axis; a single-axis wheel uses `axes[0]`.
        axes: [Condition; 2],
    },
    /// Gamepad-style rumble, `0.0..=1.0` per motor. Rendered as a constant
    /// magnitude for the plot.
    Rumble {
        strong: f32,
        weak: f32,
    },
}

impl EffectKind {
    pub fn type_str(&self) -> &'static str {
        match self {
            EffectKind::Constant { .. } => "constant",
            EffectKind::Ramp { .. } => "ramp",
            EffectKind::Periodic { .. } => "periodic",
            EffectKind::Condition { .. } => "condition",
            EffectKind::Rumble { .. } => "rumble",
        }
    }

    pub fn envelope(&self) -> Envelope {
        match self {
            EffectKind::Constant { envelope, .. }
            | EffectKind::Ramp { envelope, .. }
            | EffectKind::Periodic { envelope, .. } => *envelope,
            _ => Envelope::default(),
        }
    }
}

/// A complete effect definition: the common PID header plus its kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effect {
    pub kind: EffectKind,
    /// Total play time in ms; `None` plays until stopped.
    pub duration_ms: Option<u32>,
    /// Delay before the effect starts after `Play`, in ms.
    pub delay_ms: u32,
    /// Direction in degrees, `0.0..360.0` (see module docs).
    pub direction_deg: f32,
    /// Button that triggers the effect (`0` = none) and the retrigger
    /// interval. Carried for faithful passthrough; the renderer ignores it.
    pub trigger_button: u16,
    pub trigger_interval_ms: u16,
}

impl Effect {
    /// A wheel-shaped constant force: direction +X, infinite, no envelope.
    pub fn constant(magnitude: f32) -> Self {
        Effect {
            kind: EffectKind::Constant { magnitude: clamp_signed(magnitude), envelope: Envelope::default() },
            duration_ms: None,
            delay_ms: 0,
            direction_deg: 90.0,
            trigger_button: 0,
            trigger_interval_ms: 0,
        }
    }

    pub fn with_kind(kind: EffectKind) -> Self {
        Effect { kind, ..Effect::constant(0.0) }
    }

    pub fn with_duration(mut self, ms: Option<u32>) -> Self {
        self.duration_ms = ms;
        self
    }

    pub fn with_direction(mut self, deg: f32) -> Self {
        self.direction_deg = wrap_degrees(deg);
        self
    }

    /// Sine of the direction: the share of the force that lands on the X
    /// axis. `1.0` for `90°`, `-1.0` for `270°`.
    pub fn x_component(&self) -> f32 {
        self.direction_deg.to_radians().sin()
    }
}

/// Device-level commands that are not tied to one effect slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceControl {
    /// Stop and erase every effect; gain back to full.
    Reset,
    /// Pause playback, keeping effect timers.
    Pause,
    /// Resume after `Pause`.
    Continue,
    /// Stop every playing effect, keeping their definitions.
    StopAll,
    EnableActuators,
    DisableActuators,
}

impl DeviceControl {
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceControl::Reset => "reset",
            DeviceControl::Pause => "pause",
            DeviceControl::Continue => "continue",
            DeviceControl::StopAll => "stop-all",
            DeviceControl::EnableActuators => "enable-actuators",
            DeviceControl::DisableActuators => "disable-actuators",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "reset" => DeviceControl::Reset,
            "pause" => DeviceControl::Pause,
            "continue" => DeviceControl::Continue,
            "stop-all" => DeviceControl::StopAll,
            "enable-actuators" => DeviceControl::EnableActuators,
            "disable-actuators" => DeviceControl::DisableActuators,
            _ => return None,
        })
    }
}

/// One thing the upstream side (a game, or a recording) told the device.
///
/// This is the unit of recording and playback. A passthrough forwards every
/// event to the downstream device unchanged; the [`crate::render::Renderer`]
/// consumes the same stream to produce the plotted force.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FfbEvent {
    /// Create or replace the effect in slot `id`. Updating a playing
    /// effect's parameters keeps it playing (PID "set effect report" while
    /// running), which is how games animate a constant force.
    Upload { id: EffectId, effect: Effect },
    /// Free slot `id`.
    Erase { id: EffectId },
    /// Start slot `id`; `count` repetitions of its duration (`0` = infinite
    /// repeats, meaningful only with a finite duration).
    Play { id: EffectId, count: u32 },
    /// Stop slot `id`.
    Stop { id: EffectId },
    /// Device gain, `0.0..=1.0`.
    Gain(f32),
    /// Autocentre strength, `0.0..=1.0`. Rendered as a spring.
    Autocenter(f32),
    Control(DeviceControl),
}

impl FfbEvent {
    pub fn kind_str(&self) -> &'static str {
        match self {
            FfbEvent::Upload { .. } => "upload",
            FfbEvent::Erase { .. } => "erase",
            FfbEvent::Play { .. } => "play",
            FfbEvent::Stop { .. } => "stop",
            FfbEvent::Gain(_) => "gain",
            FfbEvent::Autocenter(_) => "autocenter",
            FfbEvent::Control(_) => "control",
        }
    }
}

/// Mirrored input from the downstream (real) device, replayed onto the
/// virtual device a game is bound to so that steering still works through a
/// passthrough. Codes are backend-specific (evdev codes on Linux).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Absolute axis, value normalised to `-1.0..=1.0`.
    Axis { code: u16, value: f32 },
    Button { code: u16, pressed: bool },
}

/// Clamp into `-1.0..=1.0`, mapping non-finite values to `0.0`.
#[inline]
pub fn clamp_signed(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Clamp into `0.0..=1.0`, mapping non-finite values to `0.0`.
#[inline]
pub fn clamp_unit(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Wrap degrees into `0.0..360.0`, mapping non-finite values to `0.0`.
#[inline]
pub fn wrap_degrees(v: f32) -> f32 {
    if v.is_finite() {
        v.rem_euclid(360.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveforms_cover_their_range() {
        for w in [Waveform::Sine, Waveform::Square, Waveform::Triangle, Waveform::SawUp, Waveform::SawDown] {
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for i in 0..1000 {
                let v = w.eval(i as f32 / 1000.0);
                assert!((-1.0..=1.0).contains(&v), "{w:?} out of range at {i}: {v}");
                lo = lo.min(v);
                hi = hi.max(v);
            }
            assert!(lo < -0.99 && hi > 0.99, "{w:?} range {lo}..{hi}");
        }
    }

    #[test]
    fn triangle_is_continuous_and_phase_wraps() {
        let w = Waveform::Triangle;
        assert!((w.eval(0.0)).abs() < 1e-6);
        assert!((w.eval(0.25) - 1.0).abs() < 1e-6);
        assert!((w.eval(0.75) + 1.0).abs() < 1e-6);
        assert!((w.eval(1.25) - w.eval(0.25)).abs() < 1e-6);
        assert!((w.eval(-0.75) - w.eval(0.25)).abs() < 1e-6);
    }

    #[test]
    fn direction_projects_onto_x() {
        assert!((Effect::constant(1.0).x_component() - 1.0).abs() < 1e-6);
        assert!((Effect::constant(1.0).with_direction(270.0).x_component() + 1.0).abs() < 1e-6);
        assert!(Effect::constant(1.0).with_direction(0.0).x_component().abs() < 1e-6);
        assert_eq!(Effect::constant(1.0).with_direction(-90.0).direction_deg, 270.0);
    }

    #[test]
    fn clamps_reject_non_finite() {
        assert_eq!(clamp_signed(f32::NAN), 0.0);
        assert_eq!(clamp_signed(-7.0), -1.0);
        assert_eq!(clamp_unit(-0.5), 0.0);
        assert_eq!(clamp_unit(f32::INFINITY), 0.0);
        assert_eq!(wrap_degrees(725.0), 5.0);
    }

    #[test]
    fn string_round_trips() {
        for w in [Waveform::Sine, Waveform::Square, Waveform::Triangle, Waveform::SawUp, Waveform::SawDown] {
            assert_eq!(Waveform::from_str(w.as_str()), Some(w));
        }
        for c in [ConditionKind::Spring, ConditionKind::Damper, ConditionKind::Inertia, ConditionKind::Friction] {
            assert_eq!(ConditionKind::from_str(c.as_str()), Some(c));
        }
        for d in [
            DeviceControl::Reset,
            DeviceControl::Pause,
            DeviceControl::Continue,
            DeviceControl::StopAll,
            DeviceControl::EnableActuators,
            DeviceControl::DisableActuators,
        ] {
            assert_eq!(DeviceControl::from_str(d.as_str()), Some(d));
        }
        assert_eq!(Waveform::from_str("noise"), None);
    }
}
