//! The virtual wheel: a uinput device with force-feedback capability that
//! a game binds to. The kernel parses the game's effect uploads and hands
//! them to us as `uinput_ff_upload` requests, which become [`FfbEvent`]s.
//! Input (steering, pedals, buttons) is mirrored onto it from the real
//! wheel so the game sees one device that both moves and has FFB.
//!
//! Needs write access to `/dev/uinput` (root, the `uinput` group, or a
//! udev rule) and the `uinput` kernel module.

use crate::convert::*;
use crate::evdev::{AbsAxis, DeviceInfo};
use crate::ffi::*;
use ffb_replay_core::io::{BackendError, FfbSource, SourceStatus};
use ffb_replay_core::model::*;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::raw::{c_int, c_ulong, c_void};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

/// Everything about the device to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualWheelConfig {
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub axes: Vec<AbsAxis>,
    pub buttons: Vec<u16>,
    /// Effect slots to offer the game (at most [`MAX_EFFECTS`]).
    pub ff_effects_max: u32,
    /// `FF_*` feature bits to advertise.
    pub ff_features: Vec<u16>,
}

/// The full effect set a direct-drive wheel advertises.
pub const ALL_FF_FEATURES: [u16; 15] = [
    FF_CONSTANT,
    FF_RAMP,
    FF_PERIODIC,
    FF_SINE,
    FF_SQUARE,
    FF_TRIANGLE,
    FF_SAW_UP,
    FF_SAW_DOWN,
    FF_SPRING,
    FF_DAMPER,
    FF_INERTIA,
    FF_FRICTION,
    FF_RUMBLE,
    FF_GAIN,
    FF_AUTOCENTER,
];

impl VirtualWheelConfig {
    /// A stand-alone wheel: 16-bit steering on X, three pedal axes, 16
    /// buttons, every effect type. For recording without a real device.
    pub fn generic(name: &str) -> Self {
        let axis = |code: u16, max: i32| AbsAxis {
            code,
            info: input_absinfo {
                value: max / 2,
                minimum: 0,
                maximum: max,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
        };
        VirtualWheelConfig {
            name: name.to_string(),
            vendor: 0x1d50, // OpenMoko pid.codes range, used by hobby projects
            product: 0x6100,
            version: 1,
            axes: vec![axis(ABS_X, 65535), axis(ABS_Y, 255), axis(ABS_Z, 255), axis(ABS_RZ, 255)],
            buttons: (0..16).map(|i| BTN_JOYSTICK + i).collect(),
            ff_effects_max: 16,
            ff_features: ALL_FF_FEATURES.to_vec(),
        }
    }

    /// A copy of a real device's axes, buttons, identity and FFB features,
    /// so the game sees the same wheel it would have bound to directly.
    pub fn mirror_of(info: &DeviceInfo, name: &str) -> Self {
        let mut ff_features: Vec<u16> = info.ff_features.clone();
        if ff_features.is_empty() {
            ff_features = ALL_FF_FEATURES.to_vec();
        }
        VirtualWheelConfig {
            name: name.to_string(),
            vendor: info.id.vendor,
            product: info.id.product,
            version: info.id.version,
            axes: info.axes.clone(),
            buttons: info.buttons.clone(),
            ff_effects_max: info.max_effects.clamp(1, MAX_EFFECTS as u32),
            ff_features,
        }
    }
}

fn ioctl_ptr<T>(fd: c_int, req: c_ulong, value: &mut T) -> io::Result<c_int> {
    // SAFETY: `value` is a live exclusive borrow for the call; each call
    // site pairs `req` with the struct type its number encodes.
    let r = unsafe { ioctl(fd, req, value as *mut T as *mut c_void) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn ioctl_int(fd: c_int, req: c_ulong, value: c_int) -> io::Result<c_int> {
    // SAFETY: integer argument only.
    let r = unsafe { ioctl(fd, req, value) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

pub struct VirtualWheel {
    file: File,
    config: VirtualWheelConfig,
    sys_name: Option<String>,
    buf: Vec<u8>,
    dead: Option<String>,
    /// Uploads the kernel sent that could not be decoded (custom waveforms).
    pub rejected_uploads: u64,
}

impl VirtualWheel {
    pub fn create(config: VirtualWheelConfig) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).custom_flags(O_NONBLOCK).open("/dev/uinput").map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("/dev/uinput: {e} (is the uinput module loaded, and do you have write access?)"),
            )
        })?;
        let fd = file.as_raw_fd();

        ioctl_int(fd, UI_SET_EVBIT, EV_KEY as c_int)?;
        ioctl_int(fd, UI_SET_EVBIT, EV_ABS as c_int)?;
        ioctl_int(fd, UI_SET_EVBIT, EV_FF as c_int)?;
        for b in &config.buttons {
            ioctl_int(fd, UI_SET_KEYBIT, *b as c_int)?;
        }
        for a in &config.axes {
            ioctl_int(fd, UI_SET_ABSBIT, a.code as c_int)?;
            let mut setup = uinput_abs_setup::new(a.code, a.info);
            ioctl_ptr(fd, UI_ABS_SETUP, &mut setup)?;
        }
        for f in &config.ff_features {
            ioctl_int(fd, UI_SET_FFBIT, *f as c_int)?;
        }

        let mut setup = uinput_setup {
            id: input_id {
                bustype: BUS_VIRTUAL,
                vendor: config.vendor,
                product: config.product,
                version: config.version,
            },
            ff_effects_max: config.ff_effects_max.clamp(1, MAX_EFFECTS as u32),
            ..Default::default()
        };
        let name = config.name.as_bytes();
        let n = name.len().min(UINPUT_MAX_NAME_SIZE - 1);
        setup.name[..n].copy_from_slice(&name[..n]);
        ioctl_ptr(fd, UI_DEV_SETUP, &mut setup)?;
        ioctl_int(fd, UI_DEV_CREATE, 0)?;

        let mut sys = [0u8; 64];
        let sys_name = ioctl_ptr(fd, ui_get_sysname(64), &mut sys).ok().map(|_| {
            let end = sys.iter().position(|b| *b == 0).unwrap_or(sys.len());
            String::from_utf8_lossy(&sys[..end]).into_owned()
        });

        Ok(VirtualWheel {
            file,
            config,
            sys_name,
            buf: vec![0u8; INPUT_EVENT_SIZE * 64],
            dead: None,
            rejected_uploads: 0,
        })
    }

    pub fn config(&self) -> &VirtualWheelConfig {
        &self.config
    }

    /// The kernel's name for the device (`inputNN`).
    pub fn sys_name(&self) -> Option<&str> {
        self.sys_name.as_deref()
    }

    /// The `/dev/input/eventN` node the game should bind to.
    pub fn event_node(&self) -> Option<PathBuf> {
        let sys = self.sys_name.as_ref()?;
        let dir = std::fs::read_dir(format!("/sys/devices/virtual/input/{sys}")).ok()?;
        dir.flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.starts_with("event"))
            .map(|n| PathBuf::from("/dev/input").join(n))
    }

    fn fd(&self) -> c_int {
        self.file.as_raw_fd()
    }

    fn handle_upload(
        &mut self,
        request_id: i32,
        out: &mut Vec<FfbEvent>,
    ) -> Result<(), BackendError> {
        let mut up = uinput_ff_upload { request_id: request_id as u32, ..Default::default() };
        ioctl_ptr(self.fd(), UI_BEGIN_FF_UPLOAD, &mut up)
            .map_err(|e| BackendError::fatal(format!("UI_BEGIN_FF_UPLOAD: {e}")))?;
        match from_ff_effect(&up.effect) {
            Ok(effect) if up.effect.id >= 0 && (up.effect.id as usize) < MAX_EFFECTS => {
                out.push(FfbEvent::Upload { id: up.effect.id as EffectId, effect });
                up.retval = 0;
            }
            Ok(_) => {
                self.rejected_uploads += 1;
                up.retval = -EINVAL;
            }
            Err(_) => {
                self.rejected_uploads += 1;
                up.retval = -EINVAL;
            }
        }
        ioctl_ptr(self.fd(), UI_END_FF_UPLOAD, &mut up)
            .map_err(|e| BackendError::fatal(format!("UI_END_FF_UPLOAD: {e}")))?;
        Ok(())
    }

    fn handle_erase(
        &mut self,
        request_id: i32,
        out: &mut Vec<FfbEvent>,
    ) -> Result<(), BackendError> {
        let mut er = uinput_ff_erase { request_id: request_id as u32, ..Default::default() };
        ioctl_ptr(self.fd(), UI_BEGIN_FF_ERASE, &mut er)
            .map_err(|e| BackendError::fatal(format!("UI_BEGIN_FF_ERASE: {e}")))?;
        if (er.effect_id as usize) < MAX_EFFECTS {
            out.push(FfbEvent::Erase { id: er.effect_id as EffectId });
        }
        er.retval = 0;
        ioctl_ptr(self.fd(), UI_END_FF_ERASE, &mut er)
            .map_err(|e| BackendError::fatal(format!("UI_END_FF_ERASE: {e}")))?;
        Ok(())
    }

    /// Translate one event read from the uinput fd. Returns `Err` only for
    /// a dead device.
    fn handle_event(
        &mut self,
        ev: InputEventRaw,
        out: &mut Vec<FfbEvent>,
    ) -> Result<(), BackendError> {
        match (ev.type_, ev.code) {
            (EV_UINPUT, UI_FF_UPLOAD) => self.handle_upload(ev.value, out),
            (EV_UINPUT, UI_FF_ERASE) => self.handle_erase(ev.value, out),
            (EV_FF, FF_GAIN) => {
                out.push(FfbEvent::Gain((ev.value.clamp(0, 0xffff) as f32) / 0xffff as f32));
                Ok(())
            }
            (EV_FF, FF_AUTOCENTER) => {
                out.push(FfbEvent::Autocenter((ev.value.clamp(0, 0xffff) as f32) / 0xffff as f32));
                Ok(())
            }
            (EV_FF, id) if (id as usize) < MAX_EFFECTS => {
                if ev.value == 0 {
                    out.push(FfbEvent::Stop { id: id as EffectId });
                } else {
                    // i32::MAX is how our own sink spells "forever"; SDL
                    // sends it for SDL_HAPTIC_INFINITY too.
                    let count = if ev.value == i32::MAX { 0 } else { ev.value as u32 };
                    out.push(FfbEvent::Play { id: id as EffectId, count });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl FfbSource for VirtualWheel {
    fn name(&self) -> String {
        match &self.sys_name {
            Some(s) => format!("virtual wheel '{}' ({s})", self.config.name),
            None => format!("virtual wheel '{}'", self.config.name),
        }
    }

    fn poll(&mut self, out: &mut Vec<FfbEvent>) -> Result<SourceStatus, BackendError> {
        if let Some(e) = &self.dead {
            return Err(BackendError::fatal(e.clone()));
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
                    let msg = format!("uinput read: {e}");
                    self.dead = Some(msg.clone());
                    return Err(BackendError::fatal(msg));
                }
            };
            let full = n == self.buf.len();
            let mut events = Vec::with_capacity(n / INPUT_EVENT_SIZE);
            events.extend(
                self.buf[..n].chunks_exact(INPUT_EVENT_SIZE).filter_map(InputEventRaw::decode),
            );
            for ev in events {
                self.handle_event(ev, out)?;
            }
            if !full {
                break;
            }
        }
        Ok(SourceStatus::Active)
    }

    fn inject_inputs(&mut self, events: &[InputEvent]) -> Result<(), BackendError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity((events.len() + 1) * INPUT_EVENT_SIZE);
        for ev in events {
            match *ev {
                InputEvent::Axis { code, value } => {
                    if let Some(a) = self.config.axes.iter().find(|a| a.code == code) {
                        bytes.extend_from_slice(
                            &InputEventRaw {
                                type_: EV_ABS,
                                code,
                                value: norm_to_abs(value, &a.info),
                            }
                            .encode(),
                        );
                    }
                }
                InputEvent::Button { code, pressed } => {
                    if self.config.buttons.contains(&code) {
                        bytes.extend_from_slice(
                            &InputEventRaw { type_: EV_KEY, code, value: pressed as i32 }.encode(),
                        );
                    }
                }
            }
        }
        if bytes.is_empty() {
            return Ok(());
        }
        bytes.extend_from_slice(
            &InputEventRaw { type_: EV_SYN, code: SYN_REPORT, value: 0 }.encode(),
        );
        self.file
            .write_all(&bytes)
            .map_err(|e| BackendError::transient(format!("uinput write: {e}")))
    }
}

impl Drop for VirtualWheel {
    fn drop(&mut self) {
        let _ = ioctl_int(self.fd(), UI_DEV_DESTROY, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_config_is_wheel_shaped() {
        let c = VirtualWheelConfig::generic("test");
        assert_eq!(c.axes[0].code, ABS_X);
        assert_eq!(c.axes[0].info.maximum, 65535);
        assert_eq!(c.buttons.len(), 16);
        assert!(c.ff_features.contains(&FF_CONSTANT));
        assert!(c.ff_effects_max <= MAX_EFFECTS as u32);
    }

    #[test]
    fn mirror_copies_identity_and_capabilities() {
        let info = DeviceInfo {
            path: "/dev/input/event9".into(),
            name: "Real Wheel".into(),
            id: input_id { bustype: BUS_USB, vendor: 0x1234, product: 0x5678, version: 3 },
            ff_features: vec![FF_CONSTANT, FF_GAIN],
            max_effects: 200,
            axes: vec![AbsAxis {
                code: ABS_RX,
                info: input_absinfo { minimum: -100, maximum: 100, ..Default::default() },
            }],
            buttons: vec![BTN_JOYSTICK],
        };
        let c = VirtualWheelConfig::mirror_of(&info, "ffb-replay: Real Wheel");
        assert_eq!(c.vendor, 0x1234);
        assert_eq!(c.product, 0x5678);
        assert_eq!(c.axes, info.axes);
        assert_eq!(c.ff_features, vec![FF_CONSTANT, FF_GAIN]);
        assert_eq!(c.ff_effects_max, MAX_EFFECTS as u32, "clamped to the renderer table");
    }
}
