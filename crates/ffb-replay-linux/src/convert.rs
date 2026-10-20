//! `Effect` ⇄ `struct ff_effect`. Pure and unit-tested; the device code
//! only ever moves bytes.
//!
//! Scales follow what SDL and the kernel's own drivers use:
//! levels `-1.0..=1.0` ⇄ `i16` `-0x7fff..=0x7fff`, envelope levels
//! `0.0..=1.0` ⇄ `0..=0x7fff`, saturations and deadband `0.0..=1.0` ⇄
//! `0..=0xffff`, direction degrees ⇄ `0..=0xffff` turns, phase turns ⇄
//! `0..=0xffff`.

use crate::ffi::*;
use ffb_replay_core::model::*;

fn level_to_i16(v: f32) -> i16 {
    (clamp_signed(v) * 0x7fff as f32).round() as i16
}

fn level_from_i16(v: i16) -> f32 {
    clamp_signed(v as f32 / 0x7fff as f32)
}

fn unit_to_u16_7fff(v: f32) -> u16 {
    (clamp_unit(v) * 0x7fff as f32).round() as u16
}

fn unit_from_u16_7fff(v: u16) -> f32 {
    clamp_unit(v as f32 / 0x7fff as f32)
}

fn unit_to_u16_ffff(v: f32) -> u16 {
    (clamp_unit(v) * 0xffff as f32).round() as u16
}

fn unit_from_u16_ffff(v: u16) -> f32 {
    clamp_unit(v as f32 / 0xffff as f32)
}

fn ms_to_u16(ms: u32) -> u16 {
    ms.min(u16::MAX as u32) as u16
}

pub fn direction_to_u16(deg: f32) -> u16 {
    ((wrap_degrees(deg) / 360.0) * 65536.0).round() as u32 as u16
}

pub fn direction_from_u16(v: u16) -> f32 {
    wrap_degrees(v as f32 * 360.0 / 65536.0)
}

fn envelope_to_ff(e: &Envelope) -> ff_envelope {
    ff_envelope {
        attack_length: ms_to_u16(e.attack_ms),
        attack_level: unit_to_u16_7fff(e.attack_level),
        fade_length: ms_to_u16(e.fade_ms),
        fade_level: unit_to_u16_7fff(e.fade_level),
    }
}

fn envelope_from_ff(e: &ff_envelope) -> Envelope {
    Envelope {
        attack_level: unit_from_u16_7fff(e.attack_level),
        attack_ms: e.attack_length as u32,
        fade_level: unit_from_u16_7fff(e.fade_level),
        fade_ms: e.fade_length as u32,
    }
}

fn put_u16(u: &mut [u8; 32], at: usize, v: u16) {
    u[at..at + 2].copy_from_slice(&v.to_ne_bytes());
}
fn put_i16(u: &mut [u8; 32], at: usize, v: i16) {
    u[at..at + 2].copy_from_slice(&v.to_ne_bytes());
}
fn get_u16(u: &[u8; 32], at: usize) -> u16 {
    u16::from_ne_bytes([u[at], u[at + 1]])
}
fn get_i16(u: &[u8; 32], at: usize) -> i16 {
    i16::from_ne_bytes([u[at], u[at + 1]])
}

fn put_envelope(u: &mut [u8; 32], at: usize, e: &ff_envelope) {
    put_u16(u, at, e.attack_length);
    put_u16(u, at + 2, e.attack_level);
    put_u16(u, at + 4, e.fade_length);
    put_u16(u, at + 6, e.fade_level);
}

fn get_envelope(u: &[u8; 32], at: usize) -> ff_envelope {
    ff_envelope {
        attack_length: get_u16(u, at),
        attack_level: get_u16(u, at + 2),
        fade_length: get_u16(u, at + 4),
        fade_level: get_u16(u, at + 6),
    }
}

fn put_condition(u: &mut [u8; 32], at: usize, c: &Condition) {
    put_u16(u, at, unit_to_u16_ffff(c.right_sat));
    put_u16(u, at + 2, unit_to_u16_ffff(c.left_sat));
    put_i16(u, at + 4, level_to_i16(c.right_coeff));
    put_i16(u, at + 6, level_to_i16(c.left_coeff));
    put_u16(u, at + 8, unit_to_u16_ffff(c.deadband));
    put_i16(u, at + 10, level_to_i16(c.center));
}

fn get_condition(u: &[u8; 32], at: usize) -> Condition {
    Condition {
        right_sat: unit_from_u16_ffff(get_u16(u, at)),
        left_sat: unit_from_u16_ffff(get_u16(u, at + 2)),
        right_coeff: level_from_i16(get_i16(u, at + 4)),
        left_coeff: level_from_i16(get_i16(u, at + 6)),
        deadband: unit_from_u16_ffff(get_u16(u, at + 8)),
        center: level_from_i16(get_i16(u, at + 10)),
    }
}

pub fn waveform_to_ff(w: Waveform) -> u16 {
    match w {
        Waveform::Sine => FF_SINE,
        Waveform::Square => FF_SQUARE,
        Waveform::Triangle => FF_TRIANGLE,
        Waveform::SawUp => FF_SAW_UP,
        Waveform::SawDown => FF_SAW_DOWN,
    }
}

pub fn waveform_from_ff(v: u16) -> Option<Waveform> {
    Some(match v {
        FF_SINE => Waveform::Sine,
        FF_SQUARE => Waveform::Square,
        FF_TRIANGLE => Waveform::Triangle,
        FF_SAW_UP => Waveform::SawUp,
        FF_SAW_DOWN => Waveform::SawDown,
        _ => return None,
    })
}

pub fn condition_to_ff(k: ConditionKind) -> u16 {
    match k {
        ConditionKind::Spring => FF_SPRING,
        ConditionKind::Damper => FF_DAMPER,
        ConditionKind::Inertia => FF_INERTIA,
        ConditionKind::Friction => FF_FRICTION,
    }
}

/// The evdev `FF_*` type code for an effect.
pub fn ff_type_of(e: &Effect) -> u16 {
    match &e.kind {
        EffectKind::Constant { .. } => FF_CONSTANT,
        EffectKind::Ramp { .. } => FF_RAMP,
        EffectKind::Periodic { .. } => FF_PERIODIC,
        EffectKind::Condition { kind, .. } => condition_to_ff(*kind),
        EffectKind::Rumble { .. } => FF_RUMBLE,
    }
}

/// Name of an `FF_*` feature bit, for device listings.
pub fn ff_bit_name(bit: u16) -> Option<&'static str> {
    Some(match bit {
        FF_RUMBLE => "rumble",
        FF_PERIODIC => "periodic",
        FF_CONSTANT => "constant",
        FF_SPRING => "spring",
        FF_FRICTION => "friction",
        FF_DAMPER => "damper",
        FF_INERTIA => "inertia",
        FF_RAMP => "ramp",
        FF_SQUARE => "square",
        FF_TRIANGLE => "triangle",
        FF_SINE => "sine",
        FF_SAW_UP => "saw-up",
        FF_SAW_DOWN => "saw-down",
        FF_CUSTOM => "custom",
        FF_GAIN => "gain",
        FF_AUTOCENTER => "autocenter",
        _ => return None,
    })
}

/// Encode an effect for `EVIOCSFF` / `uinput_ff_upload`. `kernel_id` is
/// the device's handle for the slot (`-1` asks the kernel to allocate).
pub fn to_ff_effect(e: &Effect, kernel_id: i16) -> ff_effect {
    let mut ff = ff_effect {
        type_: ff_type_of(e),
        id: kernel_id,
        direction: direction_to_u16(e.direction_deg),
        trigger: ff_trigger { button: e.trigger_button, interval: e.trigger_interval_ms },
        replay: ff_replay {
            length: e.duration_ms.map(ms_to_u16).unwrap_or(0),
            delay: ms_to_u16(e.delay_ms),
        },
        ..Default::default()
    };
    let u = &mut ff.u;
    match &e.kind {
        EffectKind::Constant { magnitude, envelope } => {
            put_i16(u, 0, level_to_i16(*magnitude));
            put_envelope(u, 2, &envelope_to_ff(envelope));
        }
        EffectKind::Ramp { start, end, envelope } => {
            put_i16(u, 0, level_to_i16(*start));
            put_i16(u, 2, level_to_i16(*end));
            put_envelope(u, 4, &envelope_to_ff(envelope));
        }
        EffectKind::Periodic { waveform, magnitude, offset, phase, period_ms, envelope } => {
            put_u16(u, 0, waveform_to_ff(*waveform));
            put_u16(u, 2, ms_to_u16(*period_ms));
            put_i16(u, 4, level_to_i16(*magnitude));
            put_i16(u, 6, level_to_i16(*offset));
            put_u16(u, 8, (phase.rem_euclid(1.0) * 65536.0).round() as u32 as u16);
            put_envelope(u, 10, &envelope_to_ff(envelope));
            // custom_len (u32 at 20) and custom_data (pointer at 24) stay zero.
        }
        EffectKind::Condition { axes, .. } => {
            put_condition(u, 0, &axes[0]);
            put_condition(u, 12, &axes[1]);
        }
        EffectKind::Rumble { strong, weak } => {
            put_u16(u, 0, unit_to_u16_ffff(*strong));
            put_u16(u, 2, unit_to_u16_ffff(*weak));
        }
    }
    ff
}

/// Decode an effect handed to a uinput device by the kernel.
pub fn from_ff_effect(ff: &ff_effect) -> Result<Effect, String> {
    let u = &ff.u;
    let kind = match ff.type_ {
        FF_CONSTANT => EffectKind::Constant {
            magnitude: level_from_i16(get_i16(u, 0)),
            envelope: envelope_from_ff(&get_envelope(u, 2)),
        },
        FF_RAMP => EffectKind::Ramp {
            start: level_from_i16(get_i16(u, 0)),
            end: level_from_i16(get_i16(u, 2)),
            envelope: envelope_from_ff(&get_envelope(u, 4)),
        },
        FF_PERIODIC => EffectKind::Periodic {
            waveform: waveform_from_ff(get_u16(u, 0))
                .ok_or_else(|| format!("unsupported periodic waveform 0x{:x}", get_u16(u, 0)))?,
            period_ms: get_u16(u, 2) as u32,
            magnitude: level_from_i16(get_i16(u, 4)),
            offset: level_from_i16(get_i16(u, 6)),
            phase: get_u16(u, 8) as f32 / 65536.0,
            envelope: envelope_from_ff(&get_envelope(u, 10)),
        },
        FF_SPRING | FF_DAMPER | FF_INERTIA | FF_FRICTION => EffectKind::Condition {
            kind: match ff.type_ {
                FF_SPRING => ConditionKind::Spring,
                FF_DAMPER => ConditionKind::Damper,
                FF_INERTIA => ConditionKind::Inertia,
                _ => ConditionKind::Friction,
            },
            axes: [get_condition(u, 0), get_condition(u, 12)],
        },
        FF_RUMBLE => EffectKind::Rumble {
            strong: unit_from_u16_ffff(get_u16(u, 0)),
            weak: unit_from_u16_ffff(get_u16(u, 2)),
        },
        other => return Err(format!("unsupported effect type 0x{other:x}")),
    };
    Ok(Effect {
        kind,
        duration_ms: if ff.replay.length == 0 { None } else { Some(ff.replay.length as u32) },
        delay_ms: ff.replay.delay as u32,
        direction_deg: direction_from_u16(ff.direction),
        trigger_button: ff.trigger.button,
        trigger_interval_ms: ff.trigger.interval,
    })
}

/// Normalise a raw absolute-axis value into `-1.0..=1.0`.
pub fn abs_to_norm(value: i32, info: &input_absinfo) -> f32 {
    let span = (info.maximum as i64 - info.minimum as i64) as f32;
    if span <= 0.0 {
        return 0.0;
    }
    clamp_signed(((value as i64 - info.minimum as i64) as f32 / span) * 2.0 - 1.0)
}

/// Inverse of [`abs_to_norm`].
pub fn norm_to_abs(norm: f32, info: &input_absinfo) -> i32 {
    let span = (info.maximum as i64 - info.minimum as i64) as f64;
    let v = info.minimum as f64 + ((clamp_signed(norm) as f64 + 1.0) / 2.0) * span;
    v.round().clamp(info.minimum as f64, info.maximum as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 2e-4
    }

    #[test]
    fn constant_round_trip_and_scale() {
        let e = Effect::constant(0.5).with_duration(Some(250));
        let ff = to_ff_effect(&e, 3);
        assert_eq!(ff.type_, FF_CONSTANT);
        assert_eq!(ff.id, 3);
        assert_eq!(ff.direction, 0x4000, "90° is a quarter turn");
        assert_eq!(ff.replay.length, 250);
        assert_eq!(get_i16(&ff.u, 0), 16384);
        let back = from_ff_effect(&ff).unwrap();
        assert_eq!(back.duration_ms, Some(250));
        assert!(close(back.direction_deg, 90.0));
        match back.kind {
            EffectKind::Constant { magnitude, .. } => assert!(close(magnitude, 0.5)),
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn all_kinds_round_trip() {
        let env = Envelope { attack_level: 0.25, attack_ms: 10, fade_level: 0.75, fade_ms: 20 };
        let cond = Condition {
            center: -0.5,
            deadband: 0.1,
            right_coeff: 0.8,
            left_coeff: -0.4,
            right_sat: 0.9,
            left_sat: 0.3,
        };
        let effects = vec![
            Effect::constant(-1.0).with_direction(270.0),
            Effect::with_kind(EffectKind::Ramp { start: -0.25, end: 0.75, envelope: env })
                .with_duration(Some(1000)),
            Effect::with_kind(EffectKind::Periodic {
                waveform: Waveform::Triangle,
                magnitude: 0.5,
                offset: -0.125,
                phase: 0.25,
                period_ms: 33,
                envelope: env,
            }),
            Effect::with_kind(EffectKind::Condition {
                kind: ConditionKind::Damper,
                axes: [cond, Condition::default()],
            }),
            Effect::with_kind(EffectKind::Rumble { strong: 1.0, weak: 0.5 }),
        ];
        for e in effects {
            let ff = to_ff_effect(&e, -1);
            let back = from_ff_effect(&ff).unwrap();
            assert_eq!(ff_type_of(&back), ff.type_);
            assert_eq!(back.duration_ms, e.duration_ms);
            assert!(close(back.direction_deg, e.direction_deg), "{e:?} vs {back:?}");
            match (&e.kind, &back.kind) {
                (
                    EffectKind::Constant { magnitude: a, .. },
                    EffectKind::Constant { magnitude: b, .. },
                ) => assert!(close(*a, *b)),
                (
                    EffectKind::Ramp { start: a, end: b, envelope: ea },
                    EffectKind::Ramp { start: c, end: d, envelope: eb },
                ) => {
                    assert!(close(*a, *c) && close(*b, *d));
                    assert_eq!(ea.attack_ms, eb.attack_ms);
                    assert!(close(ea.fade_level, eb.fade_level));
                }
                (
                    EffectKind::Periodic {
                        waveform: wa,
                        magnitude: ma,
                        offset: oa,
                        phase: pa,
                        period_ms: pma,
                        ..
                    },
                    EffectKind::Periodic {
                        waveform: wb,
                        magnitude: mb,
                        offset: ob,
                        phase: pb,
                        period_ms: pmb,
                        ..
                    },
                ) => {
                    assert_eq!(wa, wb);
                    assert_eq!(pma, pmb);
                    assert!(close(*ma, *mb) && close(*oa, *ob) && close(*pa, *pb));
                }
                (
                    EffectKind::Condition { kind: ka, axes: aa },
                    EffectKind::Condition { kind: kb, axes: ab },
                ) => {
                    assert_eq!(ka, kb);
                    assert!(close(aa[0].center, ab[0].center));
                    assert!(close(aa[0].deadband, ab[0].deadband));
                    assert!(close(aa[0].right_coeff, ab[0].right_coeff));
                    assert!(close(aa[0].left_coeff, ab[0].left_coeff));
                    assert!(close(aa[0].right_sat, ab[0].right_sat));
                    assert!(close(aa[0].left_sat, ab[0].left_sat));
                    assert_eq!(ab[1], Condition::default());
                }
                (
                    EffectKind::Rumble { strong: a, weak: b },
                    EffectKind::Rumble { strong: c, weak: d },
                ) => {
                    assert!(close(*a, *c) && close(*b, *d))
                }
                (a, b) => panic!("kind changed: {a:?} -> {b:?}"),
            }
        }
    }

    #[test]
    fn unsupported_types_are_rejected() {
        let ff = ff_effect { type_: FF_CUSTOM, ..Default::default() };
        assert!(from_ff_effect(&ff).unwrap_err().contains("unsupported"));
        let mut ff = ff_effect { type_: FF_PERIODIC, ..Default::default() };
        put_u16(&mut ff.u, 0, FF_CUSTOM);
        assert!(from_ff_effect(&ff).is_err());
    }

    #[test]
    fn axis_normalisation() {
        let info = input_absinfo { minimum: 0, maximum: 65535, ..Default::default() };
        assert!(close(abs_to_norm(0, &info), -1.0));
        assert!(close(abs_to_norm(65535, &info), 1.0));
        assert!(abs_to_norm(32768, &info).abs() < 1e-3);
        assert_eq!(norm_to_abs(-1.0, &info), 0);
        assert_eq!(norm_to_abs(1.0, &info), 65535);
        assert_eq!(norm_to_abs(0.0, &info), 32768);
        let bad = input_absinfo { minimum: 5, maximum: 5, ..Default::default() };
        assert_eq!(abs_to_norm(9, &bad), 0.0);
        let signed = input_absinfo { minimum: -32768, maximum: 32767, ..Default::default() };
        assert!(abs_to_norm(0, &signed).abs() < 1e-4);
    }

    #[test]
    fn direction_wraps_at_a_full_turn() {
        assert_eq!(direction_to_u16(0.0), 0);
        assert_eq!(direction_to_u16(180.0), 0x8000);
        assert_eq!(direction_to_u16(360.0), 0);
        assert!(close(direction_from_u16(0xC000), 270.0));
    }
}
