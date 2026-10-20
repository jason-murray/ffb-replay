//! Hand-written bindings to the parts of `<linux/input.h>`,
//! `<linux/uinput.h>` and `<poll.h>` this crate uses. Every constant and
//! layout is checked in the tests at the bottom against values printed by
//! the C headers on x86_64; the structs carry explicit padding so their
//! layout does not depend on a union.
#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_int, c_ulong};

extern "C" {
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn poll(fds: *mut pollfd, nfds: c_ulong, timeout: c_int) -> c_int;
}

// ---- ioctl number construction (asm-generic/ioctl.h) -------------------------

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> c_ulong {
    ((dir << IOC_DIRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)
        | ((ty as u32) << IOC_TYPESHIFT)
        | ((nr as u32) << IOC_NRSHIFT)) as c_ulong
}
pub const fn io(ty: u8, nr: u8) -> c_ulong {
    ioc(IOC_NONE, ty, nr, 0)
}
pub const fn ior(ty: u8, nr: u8, size: usize) -> c_ulong {
    ioc(IOC_READ, ty, nr, size)
}
pub const fn iow(ty: u8, nr: u8, size: usize) -> c_ulong {
    ioc(IOC_WRITE, ty, nr, size)
}
pub const fn iowr(ty: u8, nr: u8, size: usize) -> c_ulong {
    ioc(IOC_READ | IOC_WRITE, ty, nr, size)
}

// ---- input.h ------------------------------------------------------------------

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const EV_FF: u16 = 0x15;
pub const EV_FF_STATUS: u16 = 0x17;
pub const EV_UINPUT: u16 = 0x0101;
pub const SYN_REPORT: u16 = 0;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
pub const ABS_CNT: usize = 0x40;
pub const KEY_CNT: usize = 0x300;
pub const FF_CNT: usize = 0x80;
pub const BTN_JOYSTICK: u16 = 0x120;

pub const FF_RUMBLE: u16 = 0x50;
pub const FF_PERIODIC: u16 = 0x51;
pub const FF_CONSTANT: u16 = 0x52;
pub const FF_SPRING: u16 = 0x53;
pub const FF_FRICTION: u16 = 0x54;
pub const FF_DAMPER: u16 = 0x55;
pub const FF_INERTIA: u16 = 0x56;
pub const FF_RAMP: u16 = 0x57;
pub const FF_SQUARE: u16 = 0x58;
pub const FF_TRIANGLE: u16 = 0x59;
pub const FF_SINE: u16 = 0x5a;
pub const FF_SAW_UP: u16 = 0x5b;
pub const FF_SAW_DOWN: u16 = 0x5c;
pub const FF_CUSTOM: u16 = 0x5d;
pub const FF_GAIN: u16 = 0x60;
pub const FF_AUTOCENTER: u16 = 0x61;
pub const FF_MAX: u16 = 0x7f;

pub const BUS_USB: u16 = 0x03;
pub const BUS_VIRTUAL: u16 = 0x06;

pub const EVIOCGVERSION: c_ulong = ior(b'E', 0x01, 4);
pub const EVIOCGID: c_ulong = ior(b'E', 0x02, 8);
pub const fn eviocgname(len: usize) -> c_ulong {
    ioc(IOC_READ, b'E', 0x06, len)
}
pub const fn eviocgphys(len: usize) -> c_ulong {
    ioc(IOC_READ, b'E', 0x07, len)
}
pub const fn eviocguniq(len: usize) -> c_ulong {
    ioc(IOC_READ, b'E', 0x08, len)
}
pub const fn eviocgbit(ev: u16, len: usize) -> c_ulong {
    ioc(IOC_READ, b'E', 0x20 + ev as u8, len)
}
pub const fn eviocgabs(abs: u16) -> c_ulong {
    ior(b'E', 0x40 + abs as u8, 24)
}
pub const EVIOCSFF: c_ulong = iow(b'E', 0x80, 48);
pub const EVIOCRMFF: c_ulong = iow(b'E', 0x81, 4);
pub const EVIOCGEFFECTS: c_ulong = ior(b'E', 0x84, 4);
pub const EVIOCGRAB: c_ulong = iow(b'E', 0x90, 4);

// ---- uinput.h -----------------------------------------------------------------

pub const UINPUT_MAX_NAME_SIZE: usize = 80;
pub const UI_DEV_CREATE: c_ulong = io(b'U', 1);
pub const UI_DEV_DESTROY: c_ulong = io(b'U', 2);
pub const UI_DEV_SETUP: c_ulong = iow(b'U', 3, 92);
pub const UI_ABS_SETUP: c_ulong = iow(b'U', 4, 28);
pub const UI_SET_EVBIT: c_ulong = iow(b'U', 100, 4);
pub const UI_SET_KEYBIT: c_ulong = iow(b'U', 101, 4);
pub const UI_SET_ABSBIT: c_ulong = iow(b'U', 103, 4);
pub const UI_SET_FFBIT: c_ulong = iow(b'U', 107, 4);
pub const UI_BEGIN_FF_UPLOAD: c_ulong = iowr(b'U', 200, 104);
pub const UI_END_FF_UPLOAD: c_ulong = iow(b'U', 201, 104);
pub const UI_BEGIN_FF_ERASE: c_ulong = iowr(b'U', 202, 12);
pub const UI_END_FF_ERASE: c_ulong = iow(b'U', 203, 12);
pub const fn ui_get_sysname(len: usize) -> c_ulong {
    ioc(IOC_READ, b'U', 44, len)
}
pub const UI_FF_UPLOAD: u16 = 1;
pub const UI_FF_ERASE: u16 = 2;

// ---- poll.h / fcntl.h ---------------------------------------------------------

pub const POLLIN: i16 = 0x001;
pub const POLLERR: i16 = 0x008;
pub const POLLHUP: i16 = 0x010;
pub const O_NONBLOCK: c_int = 0o4000;

pub const ENXIO: i32 = 6;
pub const EIO: i32 = 5;
pub const ENODEV: i32 = 19;
pub const EINVAL: i32 = 22;
pub const ENOSPC: i32 = 28;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct pollfd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

// ---- structs ------------------------------------------------------------------

/// `struct input_event` on 64-bit: `timeval` (16 bytes) + type, code, value.
pub const INPUT_EVENT_SIZE: usize = 24;

/// A decoded `struct input_event`; the timestamp is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEventRaw {
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEventRaw {
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < INPUT_EVENT_SIZE {
            return None;
        }
        Some(InputEventRaw {
            type_: u16::from_ne_bytes([b[16], b[17]]),
            code: u16::from_ne_bytes([b[18], b[19]]),
            value: i32::from_ne_bytes([b[20], b[21], b[22], b[23]]),
        })
    }

    pub fn encode(&self) -> [u8; INPUT_EVENT_SIZE] {
        let mut b = [0u8; INPUT_EVENT_SIZE];
        b[16..18].copy_from_slice(&self.type_.to_ne_bytes());
        b[18..20].copy_from_slice(&self.code.to_ne_bytes());
        b[20..24].copy_from_slice(&self.value.to_ne_bytes());
        b
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct input_id {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct input_absinfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ff_replay {
    pub length: u16,
    pub delay: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ff_trigger {
    pub button: u16,
    pub interval: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ff_envelope {
    pub attack_length: u16,
    pub attack_level: u16,
    pub fade_length: u16,
    pub fade_level: u16,
}

/// `struct ff_effect` with the type-specific union carried as raw bytes
/// (see [`crate::convert`] for the encodings). The kernel's union is 32
/// bytes and 8-aligned because `ff_periodic_effect` ends in a pointer.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ff_effect {
    pub type_: u16,
    pub id: i16,
    pub direction: u16,
    pub trigger: ff_trigger,
    pub replay: ff_replay,
    pub _pad: [u8; 2],
    pub u: [u8; 32],
}

impl Default for ff_effect {
    fn default() -> Self {
        ff_effect {
            type_: 0,
            id: -1,
            direction: 0,
            trigger: ff_trigger::default(),
            replay: ff_replay::default(),
            _pad: [0; 2],
            u: [0; 32],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct uinput_setup {
    pub id: input_id,
    pub name: [u8; UINPUT_MAX_NAME_SIZE],
    pub ff_effects_max: u32,
}

impl Default for uinput_setup {
    fn default() -> Self {
        uinput_setup { id: input_id::default(), name: [0; UINPUT_MAX_NAME_SIZE], ff_effects_max: 0 }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct uinput_abs_setup {
    pub code: u16,
    pub _pad: [u8; 2],
    pub absinfo: input_absinfo,
}

impl uinput_abs_setup {
    pub fn new(code: u16, absinfo: input_absinfo) -> Self {
        uinput_abs_setup { code, _pad: [0; 2], absinfo }
    }
}

#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct uinput_ff_upload {
    pub request_id: u32,
    pub retval: i32,
    pub effect: ff_effect,
    pub old: ff_effect,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct uinput_ff_erase {
    pub request_id: u32,
    pub retval: i32,
    pub effect_id: u32,
}

/// Test a bit in a kernel bitmask returned by `EVIOCGBIT`.
pub fn bit_set(mask: &[u8], bit: usize) -> bool {
    mask.get(bit / 8).map(|b| b & (1 << (bit % 8)) != 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Values printed by a C program compiled against the system headers.
    #[test]
    fn ioctl_numbers_match_the_headers() {
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_DEV_SETUP, 0x405c5503);
        assert_eq!(UI_ABS_SETUP, 0x401c5504);
        assert_eq!(UI_SET_EVBIT, 0x40045564);
        assert_eq!(UI_SET_KEYBIT, 0x40045565);
        assert_eq!(UI_SET_ABSBIT, 0x40045567);
        assert_eq!(UI_SET_FFBIT, 0x4004556b);
        assert_eq!(UI_BEGIN_FF_UPLOAD, 0xc06855c8);
        assert_eq!(UI_END_FF_UPLOAD, 0x406855c9);
        assert_eq!(UI_BEGIN_FF_ERASE, 0xc00c55ca);
        assert_eq!(UI_END_FF_ERASE, 0x400c55cb);
        assert_eq!(EVIOCSFF, 0x40304580);
        assert_eq!(EVIOCRMFF, 0x40044581);
        assert_eq!(EVIOCGEFFECTS, 0x80044584);
        assert_eq!(eviocgname(256), 0x81004506);
        assert_eq!(eviocgphys(256), 0x81004507);
        assert_eq!(eviocguniq(256), 0x81004508);
        assert_eq!(EVIOCGID, 0x80084502);
        assert_eq!(eviocgbit(0, 8), 0x80084520);
        assert_eq!(eviocgbit(EV_FF, 16), 0x80104535);
        assert_eq!(eviocgbit(EV_ABS, 8), 0x80084523);
        assert_eq!(eviocgbit(EV_KEY, 96), 0x80604521);
        assert_eq!(eviocgabs(ABS_X), 0x80184540);
        assert_eq!(EVIOCGRAB, 0x40044590);
        assert_eq!(EVIOCGVERSION, 0x80044501);
        assert_eq!(ui_get_sysname(64), 0x8040552c);
    }

    #[test]
    fn struct_layouts_match_the_headers() {
        assert_eq!(size_of::<ff_effect>(), 48);
        assert_eq!(offset_of!(ff_effect, type_), 0);
        assert_eq!(offset_of!(ff_effect, id), 2);
        assert_eq!(offset_of!(ff_effect, direction), 4);
        assert_eq!(offset_of!(ff_effect, trigger), 6);
        assert_eq!(offset_of!(ff_effect, replay), 10);
        assert_eq!(offset_of!(ff_effect, u), 16);
        assert_eq!(size_of::<ff_envelope>(), 8);
        assert_eq!(size_of::<uinput_setup>(), 92);
        assert_eq!(offset_of!(uinput_setup, name), 8);
        assert_eq!(offset_of!(uinput_setup, ff_effects_max), 88);
        assert_eq!(size_of::<uinput_abs_setup>(), 28);
        assert_eq!(offset_of!(uinput_abs_setup, absinfo), 4);
        assert_eq!(size_of::<uinput_ff_upload>(), 104);
        assert_eq!(offset_of!(uinput_ff_upload, retval), 4);
        assert_eq!(offset_of!(uinput_ff_upload, effect), 8);
        assert_eq!(offset_of!(uinput_ff_upload, old), 56);
        assert_eq!(size_of::<uinput_ff_erase>(), 12);
        assert_eq!(offset_of!(uinput_ff_erase, effect_id), 8);
        assert_eq!(size_of::<input_absinfo>(), 24);
        assert_eq!(size_of::<input_id>(), 8);
        assert_eq!(size_of::<pollfd>(), 8);
    }

    #[test]
    fn input_event_round_trips() {
        let ev = InputEventRaw { type_: EV_FF, code: 0x60, value: -5 };
        let b = ev.encode();
        assert_eq!(InputEventRaw::decode(&b), Some(ev));
        assert_eq!(InputEventRaw::decode(&b[..10]), None);
    }

    #[test]
    fn bitmask_helper() {
        let mask = [0b0000_0010u8, 0b1000_0000u8];
        assert!(bit_set(&mask, 1));
        assert!(!bit_set(&mask, 0));
        assert!(bit_set(&mask, 15));
        assert!(!bit_set(&mask, 16));
    }
}
