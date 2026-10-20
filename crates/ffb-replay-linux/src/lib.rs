//! # ffb-replay-linux
//!
//! Linux backends for `ffb-replay-core`:
//!
//! - [`uinput::VirtualWheel`]: an [`FfbSource`](ffb_replay_core::FfbSource).
//!   A uinput device with force feedback that a game binds to; the kernel
//!   hands us every effect the game uploads.
//! - [`evdev::Wheel`]: an [`FfbSink`](ffb_replay_core::FfbSink). The real
//!   wheel, driven through the evdev force-feedback interface; also the
//!   origin of the steering position and the inputs mirrored onto the
//!   virtual wheel.
//!
//! The FFI is hand-written against the kernel UAPI headers (see `ffi.rs`,
//! whose tests pin every constant and layout), so the crate has no
//! dependencies beyond the core.
#![cfg(target_os = "linux")]

pub mod convert;
pub mod evdev;
pub mod ffi;
pub mod uinput;

pub use evdev::{enumerate, enumerate_ff, find, probe, DeviceInfo, Wheel};
pub use uinput::{VirtualWheel, VirtualWheelConfig};
