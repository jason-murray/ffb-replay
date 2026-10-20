//! # ffb-replay-core
//!
//! Record and play back force-feedback (FFB) signals, independently of the
//! device API that carries them.
//!
//! - [`model`]: the effect vocabulary (constant, ramp, periodic, condition
//!   effects, gain, device control), normalised units.
//! - [`io`]: the [`FfbSource`](io::FfbSource) / [`FfbSink`](io::FfbSink)
//!   seams every backend implements.
//! - [`render`]: turns the live effect table into one force value per tick,
//!   for the graph and for sinks that only take a scalar force.
//! - [`format`]: the `.ffbr` JSON Lines recording format.
//! - [`recorder`] / [`player`]: write a recording from the live stream;
//!   play one back as a source.
//! - [`offline`]: render a whole recording to a trace without a device.
//! - [`pipeline`]: the tick loop that joins a source to a sink, with
//!   recording and a sample stream for a UI.
//!
//! The crate has no dependencies and forbids `unsafe`, so it builds and
//! tests on every platform; device backends live in sibling crates.

pub mod format;
pub mod io;
pub mod json;
pub mod model;
pub mod offline;
pub mod pacer;
pub mod pipeline;
pub mod player;
pub mod recorder;
pub mod render;

pub use format::{Header, Record, RecordBody, Recording};
pub use io::{BackendError, FfbSink, FfbSource, SinkMode, SourceStatus};
pub use model::*;
pub use pipeline::{Command, Outcome, Pipeline, PipelineConfig, Sample, Summary};
pub use player::{Player, PlayerOptions};
pub use render::{AxisState, Rendered, Renderer};
