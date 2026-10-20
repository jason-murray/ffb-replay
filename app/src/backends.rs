//! Maps the source/sink names used on the command line and in the UI onto
//! backend objects. Platform backends are picked by `cfg`; the synthetic
//! and null ones exist everywhere so the graph and the file tools work on
//! a machine with no wheel.

use ffb_replay_core::io::{FfbSink, FfbSource, IdleSource, NullSink, ScriptedSource};
use ffb_replay_core::json::Json;

/// What a source spec means, parsed from a string such as `virtual`,
/// `mirror`, `demo` or `demo:loop`.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceSpec {
    /// A virtual wheel for the game to bind to (uinput on Linux).
    Virtual,
    /// A virtual wheel shaped like the sink device, with its inputs mirrored.
    Mirror,
    /// Synthetic sine-shaped constant force; marked as such in headers.
    Demo { looped: bool },
    /// Nothing; the sink is driven only by injected events.
    Idle,
}

impl SourceSpec {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s.trim().to_lowercase().as_str() {
            "virtual" | "uinput" => SourceSpec::Virtual,
            "mirror" => SourceSpec::Mirror,
            "demo" => SourceSpec::Demo { looped: false },
            "demo:loop" => SourceSpec::Demo { looped: true },
            "idle" | "none" => SourceSpec::Idle,
            other => {
                return Err(format!(
                    "unknown source '{other}' (virtual, mirror, demo, demo:loop, idle)"
                ))
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SinkSpec {
    None,
    /// A device spec: path, `eventN`, or a name substring.
    Device(String),
}

impl SinkSpec {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "" | "none" | "null" => SinkSpec::None,
            other => SinkSpec::Device(other.to_string()),
        }
    }
}

pub struct Built {
    pub source: Box<dyn FfbSource>,
    pub sink: Box<dyn FfbSink>,
    /// Lines worth showing the user (the virtual device's node, etc.).
    pub notes: Vec<String>,
}

/// Build the pair. `tick_hz` is needed by the demo source's clock; `grab`
/// takes the real device exclusively.
pub fn build(
    source: &SourceSpec,
    sink: &SinkSpec,
    tick_hz: u32,
    grab: bool,
) -> Result<Built, String> {
    let mut notes = Vec::new();
    let sink_box: Box<dyn FfbSink> = match sink {
        SinkSpec::None => Box::new(NullSink::default()),
        SinkSpec::Device(spec) => open_device(spec, grab, &mut notes)?,
    };
    let source_box: Box<dyn FfbSource> = match source {
        SourceSpec::Demo { looped } => {
            Box::new(ScriptedSource::sine_demo(tick_hz, 60, 8.0, *looped))
        }
        SourceSpec::Idle => Box::new(IdleSource),
        SourceSpec::Virtual => virtual_wheel(None, &mut notes)?,
        SourceSpec::Mirror => {
            let info = match sink {
                SinkSpec::Device(spec) => Some(device_info(spec)?),
                SinkSpec::None => None,
            };
            if info.is_none() {
                notes.push(
                    "mirror source without a sink device: using a generic virtual wheel".into(),
                );
            }
            virtual_wheel(info, &mut notes)?
        }
    };
    Ok(Built { source: source_box, sink: sink_box, notes })
}

/// JSON description of the FFB-capable devices on this machine.
pub fn devices_json() -> Json {
    Json::Arr(
        device_list()
            .into_iter()
            .map(|d| {
                Json::obj()
                    .with("path", d.path.as_str())
                    .with("name", d.name.as_str())
                    .with("ff", d.ff)
                    .with("effects", Json::Arr(d.effects.iter().map(|s| Json::from(*s)).collect()))
                    .with("max_effects", d.max_effects)
                    .with("axes", d.axes)
                    .with("buttons", d.buttons)
                    .with("virtual", d.is_virtual)
            })
            .collect(),
    )
}

/// Plain description of one device for listings.
pub struct DeviceSummary {
    pub path: String,
    pub name: String,
    pub ff: bool,
    pub effects: Vec<&'static str>,
    pub max_effects: u32,
    pub axes: u32,
    pub buttons: u32,
    pub is_virtual: bool,
}

// ---- Linux ----------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use ffb_replay_linux::{enumerate, find, DeviceInfo, VirtualWheel, VirtualWheelConfig, Wheel};

    pub fn device_list() -> Vec<DeviceSummary> {
        enumerate()
            .into_iter()
            .map(|d| DeviceSummary {
                path: d.path.display().to_string(),
                name: d.name.clone(),
                ff: d.has_ff(),
                effects: d.ff_names(),
                max_effects: d.max_effects,
                axes: d.axes.len() as u32,
                buttons: d.buttons.len() as u32,
                is_virtual: d.id.bustype == ffb_replay_linux::ffi::BUS_VIRTUAL,
            })
            .collect()
    }

    pub fn device_info(spec: &str) -> Result<DeviceInfo, String> {
        find(spec).map_err(|e| e.to_string())
    }

    pub fn open_device(
        spec: &str,
        grab: bool,
        notes: &mut Vec<String>,
    ) -> Result<Box<dyn FfbSink>, String> {
        let wheel = Wheel::open_spec(spec, grab).map_err(|e| format!("open '{spec}': {e}"))?;
        let info = wheel.info();
        notes.push(format!(
            "sink: {} ({}), effects: {}, max {}{}",
            info.name,
            info.path.display(),
            info.ff_names().join(" "),
            info.max_effects,
            if grab { ", grabbed" } else { "" }
        ));
        Ok(Box::new(wheel))
    }

    pub fn virtual_wheel(
        mirror: Option<DeviceInfo>,
        notes: &mut Vec<String>,
    ) -> Result<Box<dyn FfbSource>, String> {
        let config = match &mirror {
            Some(info) => {
                VirtualWheelConfig::mirror_of(info, &format!("ffb-replay: {}", info.name))
            }
            None => VirtualWheelConfig::generic("ffb-replay virtual wheel"),
        };
        let vw = VirtualWheel::create(config).map_err(|e| format!("create virtual wheel: {e}"))?;
        // The event node appears a moment after creation.
        let mut node = None;
        for _ in 0..20 {
            node = vw.event_node();
            if node.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        notes.push(match node {
            Some(p) => format!(
                "virtual wheel '{}' is {} - bind the game to it",
                vw.config().name,
                p.display()
            ),
            None => format!(
                "virtual wheel '{}' created ({})",
                vw.config().name,
                vw.sys_name().unwrap_or("?")
            ),
        });
        Ok(Box::new(vw))
    }
}

// ---- other platforms --------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::*;

    pub struct DeviceInfo;

    pub fn device_list() -> Vec<DeviceSummary> {
        Vec::new()
    }

    pub fn device_info(spec: &str) -> Result<DeviceInfo, String> {
        Err(format!(
            "device '{spec}': no device backend for this platform yet (demo and none only)"
        ))
    }

    pub fn open_device(
        spec: &str,
        _grab: bool,
        _notes: &mut Vec<String>,
    ) -> Result<Box<dyn FfbSink>, String> {
        Err(format!("device '{spec}': no device backend for this platform yet (use sink 'none')"))
    }

    pub fn virtual_wheel(
        _mirror: Option<DeviceInfo>,
        _notes: &mut Vec<String>,
    ) -> Result<Box<dyn FfbSource>, String> {
        Err("no virtual-device backend for this platform yet (use source 'demo')".into())
    }
}

use platform::{device_info, device_list, open_device, virtual_wheel};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse() {
        assert_eq!(SourceSpec::parse("Virtual").unwrap(), SourceSpec::Virtual);
        assert_eq!(SourceSpec::parse("demo:loop").unwrap(), SourceSpec::Demo { looped: true });
        assert!(SourceSpec::parse("wat").is_err());
        assert_eq!(SinkSpec::parse(" none "), SinkSpec::None);
        assert_eq!(SinkSpec::parse("event5"), SinkSpec::Device("event5".into()));
    }

    #[test]
    fn demo_builds_everywhere() {
        let b = build(&SourceSpec::Demo { looped: false }, &SinkSpec::None, 1000, false).unwrap();
        assert_eq!(b.source.name(), "synthetic script");
        assert_eq!(b.sink.name(), "none");
    }
}
