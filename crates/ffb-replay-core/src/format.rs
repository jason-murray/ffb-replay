//! The `.ffbr` recording format: JSON Lines.
//!
//! ```text
//! {"format":"ffb-replay","version":1,"created_unix":1760000000,"source":"...","sink":"...","tick_hz":1000}
//! {"t":0,"ev":{"k":"upload","id":1,"fx":{"type":"constant","mag":0.12,...}}}
//! {"t":0,"ev":{"k":"play","id":1,"n":1}}
//! {"t":1000,"axis":0.031}
//! {"t":1000,"ev":{"k":"upload","id":1,"fx":{...}}}
//! ```
//!
//! The first line is the [`Header`]; every following line is one
//! [`Record`]: either an FFB event from the upstream side or a sample of the
//! steering axis (so a recording can be reviewed offline with the wheel
//! position alongside). `t` is microseconds since the recording started.
//!
//! Line-oriented on purpose: a recorder that dies mid-session leaves a
//! readable file, and `tail -f` works on a live one.

use crate::json::{Json, JsonError};
use crate::model::*;
use std::io::{self, BufRead, Write};

pub const FORMAT_NAME: &str = "ffb-replay";
pub const FORMAT_VERSION: u32 = 1;
/// Conventional file extension.
pub const EXTENSION: &str = "ffbr";

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub version: u32,
    /// Wall-clock creation time (seconds since the Unix epoch), `0` if unknown.
    pub created_unix: u64,
    /// Human-readable description of where the events came from.
    pub source: String,
    /// Human-readable description of the downstream device, if any.
    pub sink: String,
    /// Tick rate of the pipeline that made the recording.
    pub tick_hz: u32,
    pub notes: String,
}

impl Default for Header {
    fn default() -> Self {
        Header {
            version: FORMAT_VERSION,
            created_unix: 0,
            source: String::new(),
            sink: String::new(),
            tick_hz: 0,
            notes: String::new(),
        }
    }
}

/// One line of a recording.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Record {
    /// Microseconds since the start of the recording.
    pub t_us: u64,
    pub body: RecordBody,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecordBody {
    Ffb(FfbEvent),
    /// Steering axis position `-1.0..=1.0`, sampled from the downstream
    /// device at a throttled rate.
    Axis(f32),
}

#[derive(Debug)]
pub enum FormatError {
    Io(io::Error),
    Json {
        line: usize,
        error: JsonError,
    },
    /// A well-formed JSON line that is not a valid record.
    Schema {
        line: usize,
        message: String,
    },
    NotARecording,
    UnsupportedVersion(u32),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Io(e) => write!(f, "I/O error: {e}"),
            FormatError::Json { line, error } => write!(f, "line {line}: invalid JSON: {error}"),
            FormatError::Schema { line, message } => write!(f, "line {line}: {message}"),
            FormatError::NotARecording => write!(f, "not an ffb-replay recording (bad header)"),
            FormatError::UnsupportedVersion(v) => write!(f, "unsupported recording version {v}"),
        }
    }
}

impl std::error::Error for FormatError {}

impl From<io::Error> for FormatError {
    fn from(e: io::Error) -> Self {
        FormatError::Io(e)
    }
}

// ---- encoding ---------------------------------------------------------------

pub fn header_to_json(h: &Header) -> Json {
    Json::obj()
        .with("format", FORMAT_NAME)
        .with("version", h.version)
        .with("created_unix", h.created_unix)
        .with("source", h.source.as_str())
        .with("sink", h.sink.as_str())
        .with("tick_hz", h.tick_hz)
        .with("notes", h.notes.as_str())
}

fn envelope_to_json(e: &Envelope) -> Json {
    Json::obj()
        .with("al", e.attack_level)
        .with("at", e.attack_ms)
        .with("fl", e.fade_level)
        .with("ft", e.fade_ms)
}

fn condition_to_json(c: &Condition) -> Json {
    Json::obj()
        .with("center", c.center)
        .with("dead", c.deadband)
        .with("rc", c.right_coeff)
        .with("lc", c.left_coeff)
        .with("rs", c.right_sat)
        .with("ls", c.left_sat)
}

pub fn effect_to_json(e: &Effect) -> Json {
    let mut j = Json::obj().with("type", e.kind.type_str());
    j = match &e.kind {
        EffectKind::Constant { magnitude, envelope } => {
            j.with("mag", *magnitude).with("env", envelope_to_json(envelope))
        }
        EffectKind::Ramp { start, end, envelope } => {
            j.with("start", *start).with("end", *end).with("env", envelope_to_json(envelope))
        }
        EffectKind::Periodic { waveform, magnitude, offset, phase, period_ms, envelope } => j
            .with("wave", waveform.as_str())
            .with("mag", *magnitude)
            .with("off", *offset)
            .with("phase", *phase)
            .with("period", *period_ms)
            .with("env", envelope_to_json(envelope)),
        EffectKind::Condition { kind, axes } => j
            .with("cond", kind.as_str())
            .with("axes", vec![condition_to_json(&axes[0]), condition_to_json(&axes[1])]),
        EffectKind::Rumble { strong, weak } => j.with("strong", *strong).with("weak", *weak),
    };
    j = j.with("dur", e.duration_ms).with("delay", e.delay_ms).with("dir", e.direction_deg);
    if e.trigger_button != 0 || e.trigger_interval_ms != 0 {
        j = j.with(
            "trig",
            Json::obj().with("btn", e.trigger_button).with("int", e.trigger_interval_ms),
        );
    }
    j
}

pub fn event_to_json(ev: &FfbEvent) -> Json {
    let j = Json::obj().with("k", ev.kind_str());
    match ev {
        FfbEvent::Upload { id, effect } => j.with("id", *id).with("fx", effect_to_json(effect)),
        FfbEvent::Erase { id } | FfbEvent::Stop { id } => j.with("id", *id),
        FfbEvent::Play { id, count } => j.with("id", *id).with("n", *count),
        FfbEvent::Gain(v) | FfbEvent::Autocenter(v) => j.with("v", *v),
        FfbEvent::Control(c) => j.with("v", c.as_str()),
    }
}

pub fn record_to_json(r: &Record) -> Json {
    let j = Json::obj().with("t", r.t_us);
    match &r.body {
        RecordBody::Ffb(ev) => j.with("ev", event_to_json(ev)),
        RecordBody::Axis(v) => j.with("axis", *v),
    }
}

/// Serialise one record as a line (with trailing newline).
pub fn record_line(r: &Record) -> String {
    let mut s = record_to_json(r).to_string();
    s.push('\n');
    s
}

/// Serialise the header as a line (with trailing newline).
pub fn header_line(h: &Header) -> String {
    let mut s = header_to_json(h).to_string();
    s.push('\n');
    s
}

// ---- decoding ---------------------------------------------------------------

fn schema(line: usize, message: impl Into<String>) -> FormatError {
    FormatError::Schema { line, message: message.into() }
}

fn num_f32(j: &Json, key: &str, line: usize) -> Result<f32, FormatError> {
    j.get(key).and_then(Json::as_f32).ok_or_else(|| schema(line, format!("missing number '{key}'")))
}

fn num_u32(j: &Json, key: &str, line: usize) -> Result<u32, FormatError> {
    j.get(key)
        .and_then(Json::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| schema(line, format!("missing integer '{key}'")))
}

fn opt_f32(j: &Json, key: &str) -> f32 {
    j.get(key).and_then(Json::as_f32).unwrap_or(0.0)
}

fn opt_u32(j: &Json, key: &str) -> u32 {
    j.get(key).and_then(Json::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(0)
}

pub fn header_from_json(j: &Json) -> Result<Header, FormatError> {
    if j.get("format").and_then(Json::as_str) != Some(FORMAT_NAME) {
        return Err(FormatError::NotARecording);
    }
    let version = j.get("version").and_then(Json::as_u64).ok_or(FormatError::NotARecording)? as u32;
    if version > FORMAT_VERSION {
        return Err(FormatError::UnsupportedVersion(version));
    }
    Ok(Header {
        version,
        created_unix: j.get("created_unix").and_then(Json::as_u64).unwrap_or(0),
        source: j.get("source").and_then(Json::as_str).unwrap_or("").to_string(),
        sink: j.get("sink").and_then(Json::as_str).unwrap_or("").to_string(),
        tick_hz: opt_u32(j, "tick_hz"),
        notes: j.get("notes").and_then(Json::as_str).unwrap_or("").to_string(),
    })
}

fn envelope_from_json(j: Option<&Json>) -> Envelope {
    let Some(j) = j else { return Envelope::default() };
    Envelope {
        attack_level: clamp_unit(opt_f32(j, "al")),
        attack_ms: opt_u32(j, "at"),
        fade_level: clamp_unit(opt_f32(j, "fl")),
        fade_ms: opt_u32(j, "ft"),
    }
}

fn condition_from_json(j: &Json) -> Condition {
    Condition {
        center: clamp_signed(opt_f32(j, "center")),
        deadband: clamp_unit(opt_f32(j, "dead")),
        right_coeff: clamp_signed(opt_f32(j, "rc")),
        left_coeff: clamp_signed(opt_f32(j, "lc")),
        right_sat: clamp_unit(opt_f32(j, "rs")),
        left_sat: clamp_unit(opt_f32(j, "ls")),
    }
}

pub fn effect_from_json(j: &Json, line: usize) -> Result<Effect, FormatError> {
    let ty = j
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| schema(line, "effect without 'type'"))?;
    let kind = match ty {
        "constant" => EffectKind::Constant {
            magnitude: clamp_signed(num_f32(j, "mag", line)?),
            envelope: envelope_from_json(j.get("env")),
        },
        "ramp" => EffectKind::Ramp {
            start: clamp_signed(num_f32(j, "start", line)?),
            end: clamp_signed(num_f32(j, "end", line)?),
            envelope: envelope_from_json(j.get("env")),
        },
        "periodic" => EffectKind::Periodic {
            waveform: j
                .get("wave")
                .and_then(Json::as_str)
                .and_then(Waveform::from_name)
                .ok_or_else(|| schema(line, "periodic effect without a known 'wave'"))?,
            magnitude: clamp_signed(num_f32(j, "mag", line)?),
            offset: clamp_signed(opt_f32(j, "off")),
            phase: opt_f32(j, "phase").rem_euclid(1.0),
            period_ms: num_u32(j, "period", line)?,
            envelope: envelope_from_json(j.get("env")),
        },
        "condition" => {
            let kind = j
                .get("cond")
                .and_then(Json::as_str)
                .and_then(ConditionKind::from_name)
                .ok_or_else(|| schema(line, "condition effect without a known 'cond'"))?;
            let axes = j.get("axes").and_then(Json::as_arr).unwrap_or(&[]);
            let a0 = axes.first().map(condition_from_json).unwrap_or_default();
            let a1 = axes.get(1).map(condition_from_json).unwrap_or(a0);
            EffectKind::Condition { kind, axes: [a0, a1] }
        }
        "rumble" => EffectKind::Rumble {
            strong: clamp_unit(opt_f32(j, "strong")),
            weak: clamp_unit(opt_f32(j, "weak")),
        },
        other => return Err(schema(line, format!("unknown effect type '{other}'"))),
    };
    let duration_ms = match j.get("dur") {
        None | Some(Json::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| schema(line, "'dur' must be an integer or null"))?,
        ),
    };
    let trig = j.get("trig");
    Ok(Effect {
        kind,
        duration_ms,
        delay_ms: opt_u32(j, "delay"),
        direction_deg: wrap_degrees(opt_f32(j, "dir")),
        trigger_button: trig.map(|t| opt_u32(t, "btn")).unwrap_or(0).min(u16::MAX as u32) as u16,
        trigger_interval_ms: trig.map(|t| opt_u32(t, "int")).unwrap_or(0).min(u16::MAX as u32)
            as u16,
    })
}

fn id_from_json(j: &Json, line: usize) -> Result<EffectId, FormatError> {
    j.get("id")
        .and_then(Json::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| schema(line, "event without a valid 'id'"))
}

pub fn event_from_json(j: &Json, line: usize) -> Result<FfbEvent, FormatError> {
    let k = j.get("k").and_then(Json::as_str).ok_or_else(|| schema(line, "event without 'k'"))?;
    Ok(match k {
        "upload" => FfbEvent::Upload {
            id: id_from_json(j, line)?,
            effect: effect_from_json(
                j.get("fx").ok_or_else(|| schema(line, "upload without 'fx'"))?,
                line,
            )?,
        },
        "erase" => FfbEvent::Erase { id: id_from_json(j, line)? },
        "play" => FfbEvent::Play { id: id_from_json(j, line)?, count: opt_u32(j, "n") },
        "stop" => FfbEvent::Stop { id: id_from_json(j, line)? },
        "gain" => FfbEvent::Gain(clamp_unit(num_f32(j, "v", line)?)),
        "autocenter" => FfbEvent::Autocenter(clamp_unit(num_f32(j, "v", line)?)),
        "control" => FfbEvent::Control(
            j.get("v")
                .and_then(Json::as_str)
                .and_then(DeviceControl::from_name)
                .ok_or_else(|| schema(line, "control event without a known 'v'"))?,
        ),
        other => return Err(schema(line, format!("unknown event kind '{other}'"))),
    })
}

pub fn record_from_json(j: &Json, line: usize) -> Result<Record, FormatError> {
    let t_us =
        j.get("t").and_then(Json::as_u64).ok_or_else(|| schema(line, "record without 't'"))?;
    let body = if let Some(ev) = j.get("ev") {
        RecordBody::Ffb(event_from_json(ev, line)?)
    } else if let Some(a) = j.get("axis").and_then(Json::as_f32) {
        RecordBody::Axis(clamp_signed(a))
    } else {
        return Err(schema(line, "record with neither 'ev' nor 'axis'"));
    };
    Ok(Record { t_us, body })
}

fn parse_line(text: &str, line: usize) -> Result<Json, FormatError> {
    Json::parse(text).map_err(|error| FormatError::Json { line, error })
}

// ---- streaming reader / writer -----------------------------------------------

/// Writes a header then records, flushing on demand.
pub struct Writer<W: Write> {
    out: W,
    records: u64,
}

impl<W: Write> Writer<W> {
    pub fn new(mut out: W, header: &Header) -> io::Result<Self> {
        out.write_all(header_line(header).as_bytes())?;
        Ok(Writer { out, records: 0 })
    }

    pub fn write(&mut self, r: &Record) -> io::Result<()> {
        self.out.write_all(record_line(r).as_bytes())?;
        self.records += 1;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn records(&self) -> u64 {
        self.records
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

/// Reads a header then yields records one line at a time.
pub struct Reader<R: BufRead> {
    input: R,
    header: Header,
    line: usize,
    buf: String,
}

impl<R: BufRead> Reader<R> {
    pub fn new(mut input: R) -> Result<Self, FormatError> {
        let mut buf = String::new();
        if input.read_line(&mut buf)? == 0 {
            return Err(FormatError::NotARecording);
        }
        let header = header_from_json(&parse_line(buf.trim_end(), 1)?)?;
        Ok(Reader { input, header, line: 1, buf })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Next record; `Ok(None)` at end of file. Blank lines are skipped.
    pub fn next_record(&mut self) -> Result<Option<Record>, FormatError> {
        loop {
            self.buf.clear();
            if self.input.read_line(&mut self.buf)? == 0 {
                return Ok(None);
            }
            self.line += 1;
            let text = self.buf.trim();
            if text.is_empty() {
                continue;
            }
            return record_from_json(&parse_line(text, self.line)?, self.line).map(Some);
        }
    }

    /// Drain the rest of the file into memory.
    pub fn read_all(&mut self) -> Result<Vec<Record>, FormatError> {
        let mut out = Vec::new();
        while let Some(r) = self.next_record()? {
            out.push(r);
        }
        Ok(out)
    }
}

/// A whole recording in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub header: Header,
    pub records: Vec<Record>,
}

impl Recording {
    pub fn read_from<R: BufRead>(input: R) -> Result<Self, FormatError> {
        let mut reader = Reader::new(input)?;
        let records = reader.read_all()?;
        Ok(Recording { header: reader.header().clone(), records })
    }

    pub fn load(path: &std::path::Path) -> Result<Self, FormatError> {
        let file = std::fs::File::open(path)?;
        Self::read_from(io::BufReader::new(file))
    }

    pub fn write_to<W: Write>(&self, out: W) -> io::Result<()> {
        let mut w = Writer::new(io::BufWriter::new(out), &self.header)?;
        for r in &self.records {
            w.write(r)?;
        }
        w.flush()
    }

    /// Length of the recording (time of the last record).
    pub fn duration_us(&self) -> u64 {
        self.records.last().map(|r| r.t_us).unwrap_or(0)
    }

    pub fn ffb_event_count(&self) -> usize {
        self.records.iter().filter(|r| matches!(r.body, RecordBody::Ffb(_))).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_effects() -> Vec<Effect> {
        let env = Envelope { attack_level: 0.1, attack_ms: 20, fade_level: 0.5, fade_ms: 30 };
        let cond = Condition {
            center: 0.1,
            deadband: 0.05,
            right_coeff: 0.8,
            left_coeff: -0.3,
            right_sat: 1.0,
            left_sat: 0.6,
        };
        vec![
            Effect::constant(-0.375),
            Effect {
                kind: EffectKind::Constant { magnitude: 0.5, envelope: env },
                duration_ms: Some(250),
                delay_ms: 10,
                direction_deg: 270.0,
                trigger_button: 3,
                trigger_interval_ms: 100,
            },
            Effect::with_kind(EffectKind::Ramp { start: 0.1, end: -0.9, envelope: env })
                .with_duration(Some(500)),
            Effect::with_kind(EffectKind::Periodic {
                waveform: Waveform::SawDown,
                magnitude: 0.6,
                offset: -0.1,
                phase: 0.25,
                period_ms: 40,
                envelope: Envelope::default(),
            }),
            Effect::with_kind(EffectKind::Condition {
                kind: ConditionKind::Damper,
                axes: [cond, Condition::default()],
            }),
            Effect::with_kind(EffectKind::Rumble { strong: 1.0, weak: 0.25 }),
        ]
    }

    #[test]
    fn effects_round_trip() {
        for (i, e) in sample_effects().into_iter().enumerate() {
            let j = effect_to_json(&e);
            let text = j.to_string();
            let back = effect_from_json(&Json::parse(&text).unwrap(), i).unwrap();
            assert_eq!(back, e, "effect {i}: {text}");
        }
    }

    #[test]
    fn events_and_records_round_trip() {
        let events = [
            FfbEvent::Upload { id: 7, effect: Effect::constant(0.25) },
            FfbEvent::Erase { id: 7 },
            FfbEvent::Play { id: 1, count: 0 },
            FfbEvent::Stop { id: 1 },
            FfbEvent::Gain(0.75),
            FfbEvent::Autocenter(0.0),
            FfbEvent::Control(DeviceControl::StopAll),
        ];
        let mut records: Vec<Record> = events
            .iter()
            .enumerate()
            .map(|(i, ev)| Record { t_us: i as u64 * 1000, body: RecordBody::Ffb(*ev) })
            .collect();
        records.push(Record { t_us: 99_999, body: RecordBody::Axis(-0.5) });

        let header = Header {
            created_unix: 1_700_000_000,
            source: "test".into(),
            sink: "null".into(),
            tick_hz: 1000,
            ..Default::default()
        };
        let rec = Recording { header, records };
        let mut bytes = Vec::new();
        rec.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("{\"format\":\"ffb-replay\",\"version\":1,"), "{text}");
        assert_eq!(text.lines().count(), 1 + rec.records.len());

        let back = Recording::read_from(text.as_bytes()).unwrap();
        assert_eq!(back, rec);
        assert_eq!(back.duration_us(), 99_999);
        assert_eq!(back.ffb_event_count(), 7);
    }

    #[test]
    fn rejects_foreign_files_and_future_versions() {
        assert!(matches!(
            Recording::read_from("{\"hello\":1}\n".as_bytes()),
            Err(FormatError::NotARecording)
        ));
        assert!(matches!(Recording::read_from("".as_bytes()), Err(FormatError::NotARecording)));
        assert!(matches!(
            Recording::read_from("{\"format\":\"ffb-replay\",\"version\":99}\n".as_bytes()),
            Err(FormatError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn reports_the_offending_line() {
        let text = "{\"format\":\"ffb-replay\",\"version\":1}\n{\"t\":0,\"ev\":{\"k\":\"play\",\"id\":1,\"n\":1}}\n\n{\"t\":1,\"ev\":{\"k\":\"warp\"}}\n";
        match Recording::read_from(text.as_bytes()) {
            Err(FormatError::Schema { line, message }) => {
                assert_eq!(line, 4);
                assert!(message.contains("warp"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
        let text = "{\"format\":\"ffb-replay\",\"version\":1}\n{\"t\":0,\"ev\":{\"k\":\"play\",\"id\":1,\"n\":1}\n";
        match Recording::read_from(text.as_bytes()) {
            Err(FormatError::Json { line, .. }) => assert_eq!(line, 2),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        let j = Json::parse("{\"type\":\"constant\",\"mag\":0.5}").unwrap();
        let e = effect_from_json(&j, 1).unwrap();
        assert_eq!(e.duration_ms, None);
        assert_eq!(e.direction_deg, 0.0);
        assert_eq!(e.kind.envelope(), Envelope::default());
        let j = Json::parse("{\"type\":\"condition\",\"cond\":\"spring\"}").unwrap();
        assert!(matches!(effect_from_json(&j, 1).unwrap().kind, EffectKind::Condition { .. }));
    }
}
