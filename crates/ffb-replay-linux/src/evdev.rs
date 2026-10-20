//! The real wheel: an evdev device with force-feedback support, driven as
//! an [`FfbSink`] in effects mode. Also device enumeration for pickers.
//!
//! Needs read/write access to `/dev/input/event*` (the `input` group on
//! most distributions, or a udev rule).

use crate::convert::*;
use crate::ffi::*;
use ffb_replay_core::io::{BackendError, FfbSink, SinkMode};
use ffb_replay_core::model::*;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::raw::c_int;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsAxis {
    pub code: u16,
    pub info: input_absinfo,
}

/// What `probe` learned about one `/dev/input/event*` node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
    pub id: input_id,
    /// `FF_*` feature bits the device advertises.
    pub ff_features: Vec<u16>,
    /// Simultaneous effects the device can hold.
    pub max_effects: u32,
    pub axes: Vec<AbsAxis>,
    pub buttons: Vec<u16>,
}

impl DeviceInfo {
    pub fn has_ff(&self) -> bool {
        !self.ff_features.is_empty()
    }

    pub fn supports(&self, bit: u16) -> bool {
        self.ff_features.contains(&bit)
    }

    pub fn ff_names(&self) -> Vec<&'static str> {
        self.ff_features.iter().filter_map(|b| ff_bit_name(*b)).collect()
    }

    pub fn axis(&self, code: u16) -> Option<&AbsAxis> {
        self.axes.iter().find(|a| a.code == code)
    }

    /// The axis to treat as steering: `ABS_X` when present, else the first.
    pub fn steering_axis(&self) -> Option<&AbsAxis> {
        self.axis(ABS_X).or_else(|| self.axes.first())
    }

    pub fn event_number(&self) -> Option<u32> {
        self.path.file_name()?.to_str()?.strip_prefix("event")?.parse().ok()
    }
}

/// `ioctl` that reads into `value`. Errors carry the OS error.
///
/// SAFETY contract for callers: `req` must be a request whose argument is a
/// pointer to a `T` of the size the request encodes.
fn ioctl_ptr<T>(fd: c_int, req: std::os::raw::c_ulong, value: &mut T) -> io::Result<c_int> {
    // SAFETY: `value` is a valid, exclusively borrowed `T` for the duration
    // of the call, and every call site pairs `req` with its matching type.
    let r = unsafe { ioctl(fd, req, value as *mut T as *mut std::os::raw::c_void) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// `ioctl` with an integer argument.
fn ioctl_int(fd: c_int, req: std::os::raw::c_ulong, value: c_int) -> io::Result<c_int> {
    // SAFETY: plain integer argument; no memory is handed to the kernel.
    let r = unsafe { ioctl(fd, req, value) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn read_string(fd: c_int, req: std::os::raw::c_ulong) -> String {
    let mut buf = [0u8; 256];
    if ioctl_ptr(fd, req, &mut buf).is_err() {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Inspect one device node.
pub fn probe(path: &Path) -> io::Result<DeviceInfo> {
    let file = OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(path)?;
    let fd = file.as_raw_fd();
    let name = read_string(fd, eviocgname(256));
    let mut id = input_id::default();
    let _ = ioctl_ptr(fd, EVIOCGID, &mut id);

    let mut evbits = [0u8; 8];
    ioctl_ptr(fd, eviocgbit(0, 8), &mut evbits)?;

    let mut ff_features = Vec::new();
    let mut max_effects = 0u32;
    if bit_set(&evbits, EV_FF as usize) {
        let mut ffbits = [0u8; 16];
        if ioctl_ptr(fd, eviocgbit(EV_FF, 16), &mut ffbits).is_ok() {
            for bit in FF_RUMBLE..=FF_AUTOCENTER {
                if bit_set(&ffbits, bit as usize) {
                    ff_features.push(bit);
                }
            }
        }
        let mut n: c_int = 0;
        if ioctl_ptr(fd, EVIOCGEFFECTS, &mut n).is_ok() && n > 0 {
            max_effects = n as u32;
        }
    }

    let mut axes = Vec::new();
    if bit_set(&evbits, EV_ABS as usize) {
        let mut absbits = [0u8; 8];
        if ioctl_ptr(fd, eviocgbit(EV_ABS, 8), &mut absbits).is_ok() {
            for code in 0..ABS_CNT as u16 {
                if bit_set(&absbits, code as usize) {
                    let mut info = input_absinfo::default();
                    if ioctl_ptr(fd, eviocgabs(code), &mut info).is_ok() {
                        axes.push(AbsAxis { code, info });
                    }
                }
            }
        }
    }

    let mut buttons = Vec::new();
    if bit_set(&evbits, EV_KEY as usize) {
        let mut keybits = [0u8; 96];
        if ioctl_ptr(fd, eviocgbit(EV_KEY, 96), &mut keybits).is_ok() {
            for code in 0..KEY_CNT as u16 {
                if bit_set(&keybits, code as usize) {
                    buttons.push(code);
                }
            }
        }
    }

    Ok(DeviceInfo { path: path.to_path_buf(), name, id, ff_features, max_effects, axes, buttons })
}

/// Every readable `/dev/input/event*` node, in number order.
pub fn enumerate() -> Vec<DeviceInfo> {
    let Ok(dir) = std::fs::read_dir("/dev/input") else { return Vec::new() };
    let mut paths: Vec<(u32, PathBuf)> = dir
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let n = p.file_name()?.to_str()?.strip_prefix("event")?.parse().ok()?;
            Some((n, p))
        })
        .collect();
    paths.sort();
    paths.into_iter().filter_map(|(_, p)| probe(&p).ok()).collect()
}

/// Only the devices that can play force feedback.
pub fn enumerate_ff() -> Vec<DeviceInfo> {
    enumerate().into_iter().filter(DeviceInfo::has_ff).collect()
}

/// Resolve a user-supplied device spec: a path, `eventN`, a bare number,
/// or a case-insensitive substring of the device name (first FF-capable
/// match wins).
pub fn find(spec: &str) -> io::Result<DeviceInfo> {
    let spec = spec.trim();
    if spec.starts_with('/') {
        return probe(Path::new(spec));
    }
    if let Ok(n) = spec.strip_prefix("event").unwrap_or(spec).parse::<u32>() {
        return probe(Path::new(&format!("/dev/input/event{n}")));
    }
    let needle = spec.to_lowercase();
    let all = enumerate();
    all.iter()
        .find(|d| d.has_ff() && d.name.to_lowercase().contains(&needle))
        .or_else(|| all.iter().find(|d| d.name.to_lowercase().contains(&needle)))
        .cloned()
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no input device matching '{spec}'"))
        })
}

/// Device-gone errnos end the session; everything else is counted.
fn classify_code(code: Option<i32>, message: String) -> BackendError {
    match code {
        Some(ENODEV) | Some(ENXIO) | Some(EIO) => {
            BackendError::fatal(format!("device gone: {message}"))
        }
        _ => BackendError::transient(message),
    }
}

fn classify(e: io::Error, what: &str) -> BackendError {
    classify_code(e.raw_os_error(), format!("{what}: {e}"))
}

/// An opened force-feedback device.
pub struct Wheel {
    file: File,
    info: DeviceInfo,
    /// Kernel effect id per slot, `-1` when the slot is empty.
    kernel_ids: [i16; MAX_EFFECTS],
    playing: [bool; MAX_EFFECTS],
    /// Slots paused by `DeviceControl::Pause`, to resume on `Continue`.
    paused: [bool; MAX_EFFECTS],
    gain: f32,
    actuators_on: bool,
    steering: Option<AbsAxis>,
    last_axis: Option<f32>,
    inputs: Vec<InputEvent>,
    buf: Vec<u8>,
    grabbed: bool,
    dead: Option<String>,
    pub unsupported: u64,
}

impl Wheel {
    /// Open for effects and input. `grab` takes the device exclusively so
    /// a game only sees the virtual wheel that mirrors it.
    pub fn open(info: DeviceInfo, grab: bool) -> io::Result<Self> {
        let file =
            OpenOptions::new().read(true).write(true).custom_flags(O_NONBLOCK).open(&info.path)?;
        let mut grabbed = false;
        if grab {
            ioctl_int(file.as_raw_fd(), EVIOCGRAB, 1)?;
            grabbed = true;
        }
        let steering = info.steering_axis().cloned();
        let last_axis = steering.as_ref().map(|a| abs_to_norm(a.info.value, &a.info));
        Ok(Wheel {
            file,
            info,
            kernel_ids: [-1; MAX_EFFECTS],
            playing: [false; MAX_EFFECTS],
            paused: [false; MAX_EFFECTS],
            gain: 1.0,
            actuators_on: true,
            steering,
            last_axis,
            inputs: Vec::with_capacity(64),
            buf: vec![0u8; INPUT_EVENT_SIZE * 64],
            grabbed,
            dead: None,
            unsupported: 0,
        })
    }

    pub fn open_spec(spec: &str, grab: bool) -> io::Result<Self> {
        let info = find(spec)?;
        if !info.has_ff() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{} ({}) has no force feedback", info.name, info.path.display()),
            ));
        }
        Wheel::open(info, grab)
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn fd(&self) -> c_int {
        self.file.as_raw_fd()
    }

    /// Drain pending input events into `self.inputs`, tracking steering.
    fn read_events(&mut self) {
        if self.dead.is_some() {
            return;
        }
        loop {
            let n = match self.file.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted =>
                {
                    break
                }
                Err(e) => {
                    self.dead = Some(e.to_string());
                    break;
                }
            };
            for chunk in self.buf[..n].chunks_exact(INPUT_EVENT_SIZE) {
                let Some(ev) = InputEventRaw::decode(chunk) else { continue };
                match ev.type_ {
                    EV_ABS => {
                        if let Some(axis) = self.info.axis(ev.code) {
                            let v = abs_to_norm(ev.value, &axis.info);
                            if self.steering.as_ref().map(|s| s.code) == Some(ev.code) {
                                self.last_axis = Some(v);
                            }
                            self.inputs.push(InputEvent::Axis { code: ev.code, value: v });
                        }
                    }
                    EV_KEY => self
                        .inputs
                        .push(InputEvent::Button { code: ev.code, pressed: ev.value != 0 }),
                    _ => {}
                }
            }
            if n < self.buf.len() {
                break;
            }
        }
    }

    fn write_event(
        &mut self,
        type_: u16,
        code: u16,
        value: i32,
        what: &str,
    ) -> Result<(), BackendError> {
        let bytes = InputEventRaw { type_, code, value }.encode();
        self.file.write_all(&bytes).map_err(|e| classify(e, what))
    }

    fn upload(&mut self, id: EffectId, effect: &Effect) -> Result<(), BackendError> {
        let slot = id as usize;
        if slot >= MAX_EFFECTS {
            return Err(BackendError::transient(format!("effect id {id} out of range")));
        }
        if !self.info.supports(ff_type_of(effect)) {
            self.unsupported += 1;
            return Err(BackendError::transient(format!(
                "{} does not support {} effects",
                self.info.name,
                effect.kind.type_str()
            )));
        }
        let mut ff = to_ff_effect(effect, self.kernel_ids[slot]);
        match ioctl_ptr(self.fd(), EVIOCSFF, &mut ff) {
            Ok(_) => {
                self.kernel_ids[slot] = ff.id;
                Ok(())
            }
            Err(e) => {
                let msg = match e.raw_os_error() {
                    Some(ENOSPC) => format!(
                        "upload: no free effect slot on {} (max {})",
                        self.info.name, self.info.max_effects
                    ),
                    Some(EINVAL) => format!(
                        "upload: {} rejected the {} effect",
                        self.info.name,
                        effect.kind.type_str()
                    ),
                    _ => format!("upload: {e}"),
                };
                Err(classify_code(e.raw_os_error(), msg))
            }
        }
    }

    fn erase(&mut self, id: EffectId) -> Result<(), BackendError> {
        let slot = id as usize;
        let Some(kid) = self.kernel_ids.get(slot).copied() else { return Ok(()) };
        if kid < 0 {
            return Ok(());
        }
        self.kernel_ids[slot] = -1;
        self.playing[slot] = false;
        ioctl_int(self.fd(), EVIOCRMFF, kid as c_int).map(|_| ()).map_err(|e| classify(e, "erase"))
    }

    fn play(&mut self, id: EffectId, count: u32) -> Result<(), BackendError> {
        let slot = id as usize;
        let Some(kid) = self.kernel_ids.get(slot).copied() else { return Ok(()) };
        if kid < 0 {
            return Err(BackendError::transient(format!("play: slot {id} holds no effect")));
        }
        // evdev counts repetitions in `value`; PID's "0 = forever" has no
        // equivalent, so ask for as many as the field holds.
        let value = if count == 0 { i32::MAX } else { count.min(i32::MAX as u32) as i32 };
        self.playing[slot] = true;
        self.write_event(EV_FF, kid as u16, value, "play")
    }

    fn stop(&mut self, id: EffectId) -> Result<(), BackendError> {
        let slot = id as usize;
        let Some(kid) = self.kernel_ids.get(slot).copied() else { return Ok(()) };
        if kid < 0 {
            return Ok(());
        }
        self.playing[slot] = false;
        self.write_event(EV_FF, kid as u16, 0, "stop")
    }

    fn set_gain(&mut self, g: f32) -> Result<(), BackendError> {
        let value = (clamp_unit(g) * 0xffff as f32).round() as i32;
        self.write_event(EV_FF, FF_GAIN, value, "gain")
    }

    fn stop_all(&mut self) -> Result<(), BackendError> {
        for id in 0..MAX_EFFECTS {
            if self.playing[id] {
                self.stop(id as EffectId)?;
            }
        }
        Ok(())
    }

    fn erase_all(&mut self) -> Result<(), BackendError> {
        for id in 0..MAX_EFFECTS {
            if self.kernel_ids[id] >= 0 {
                self.erase(id as EffectId)?;
            }
        }
        Ok(())
    }
}

impl FfbSink for Wheel {
    fn name(&self) -> String {
        format!("{} ({})", self.info.name, self.info.path.display())
    }

    fn mode(&self) -> SinkMode {
        SinkMode::Effects
    }

    fn apply(&mut self, event: &FfbEvent) -> Result<(), BackendError> {
        if let Some(e) = &self.dead {
            return Err(BackendError::fatal(format!("{}: {e}", self.info.name)));
        }
        match *event {
            FfbEvent::Upload { id, effect } => self.upload(id, &effect),
            FfbEvent::Erase { id } => self.erase(id),
            FfbEvent::Play { id, count } => self.play(id, count),
            FfbEvent::Stop { id } => self.stop(id),
            FfbEvent::Gain(g) => {
                self.gain = clamp_unit(g);
                if self.actuators_on {
                    self.set_gain(self.gain)
                } else {
                    Ok(())
                }
            }
            FfbEvent::Autocenter(a) => {
                if !self.info.supports(FF_AUTOCENTER) {
                    self.unsupported += 1;
                    return Ok(());
                }
                let value = (clamp_unit(a) * 0xffff as f32).round() as i32;
                self.write_event(EV_FF, FF_AUTOCENTER, value, "autocenter")
            }
            FfbEvent::Control(c) => match c {
                DeviceControl::Reset => {
                    self.stop_all()?;
                    self.erase_all()?;
                    self.gain = 1.0;
                    self.actuators_on = true;
                    self.paused = [false; MAX_EFFECTS];
                    self.set_gain(1.0)
                }
                DeviceControl::StopAll => self.stop_all(),
                // evdev has no pause; stop what is playing and remember it.
                DeviceControl::Pause => {
                    self.paused = self.playing;
                    self.stop_all()
                }
                DeviceControl::Continue => {
                    let resume = self.paused;
                    self.paused = [false; MAX_EFFECTS];
                    for (id, p) in resume.iter().enumerate() {
                        if *p {
                            self.play(id as EffectId, 1)?;
                        }
                    }
                    Ok(())
                }
                // No actuator switch either; emulate with the gain.
                DeviceControl::DisableActuators => {
                    self.actuators_on = false;
                    self.set_gain(0.0)
                }
                DeviceControl::EnableActuators => {
                    self.actuators_on = true;
                    self.set_gain(self.gain)
                }
            },
        }
    }

    fn axis(&mut self) -> Option<f32> {
        self.read_events();
        self.last_axis
    }

    fn poll_inputs(&mut self, out: &mut Vec<InputEvent>) {
        self.read_events();
        out.append(&mut self.inputs);
    }

    fn release(&mut self) -> Result<(), BackendError> {
        let a = self.stop_all();
        let b = self.erase_all();
        let c = self.set_gain(1.0);
        a.and(b).and(c)
    }
}

impl Drop for Wheel {
    fn drop(&mut self) {
        let _ = self.stop_all();
        let _ = self.erase_all();
        if self.grabbed {
            let _ = ioctl_int(self.fd(), EVIOCGRAB, 0);
        }
    }
}
