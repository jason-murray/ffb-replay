//! Writes the event stream a pipeline sees into a `.ffbr` file.

use crate::format::{Header, Record, RecordBody, Writer};
use crate::model::FfbEvent;
use std::io::{self, Write};

pub struct Recorder {
    writer: Writer<Box<dyn Write + Send>>,
    /// Pipeline time at which the recording started.
    start_us: u64,
    axis_interval_us: u64,
    next_axis_us: u64,
    last_axis: Option<f32>,
    pub error: Option<String>,
}

impl Recorder {
    /// Start a recording at pipeline time `now_us`. `initial` is the state
    /// the device already holds (effects loaded before recording began), so
    /// playback starts from the same place; it is written at `t = 0`.
    pub fn start(
        out: Box<dyn Write + Send>,
        header: &Header,
        now_us: u64,
        initial: &[FfbEvent],
        axis_sample_hz: u32,
    ) -> io::Result<Self> {
        let mut writer = Writer::new(out, header)?;
        for ev in initial {
            writer.write(&Record { t_us: 0, body: RecordBody::Ffb(*ev) })?;
        }
        Ok(Recorder {
            writer,
            start_us: now_us,
            axis_interval_us: if axis_sample_hz == 0 { u64::MAX } else { 1_000_000 / axis_sample_hz as u64 },
            next_axis_us: 0,
            last_axis: None,
            error: None,
        })
    }

    fn rel(&self, now_us: u64) -> u64 {
        now_us.saturating_sub(self.start_us)
    }

    pub fn event(&mut self, now_us: u64, ev: &FfbEvent) {
        let r = Record { t_us: self.rel(now_us), body: RecordBody::Ffb(*ev) };
        if let Err(e) = self.writer.write(&r) {
            self.error.get_or_insert(e.to_string());
        }
    }

    /// Offer an axis sample; written at most every `1/axis_sample_hz` and
    /// only when it changed.
    pub fn axis(&mut self, now_us: u64, value: Option<f32>) {
        let Some(v) = value else { return };
        let t = self.rel(now_us);
        if t < self.next_axis_us || self.last_axis == Some(v) {
            return;
        }
        self.next_axis_us = t.saturating_add(self.axis_interval_us);
        self.last_axis = Some(v);
        if let Err(e) = self.writer.write(&Record { t_us: t, body: RecordBody::Axis(v) }) {
            self.error.get_or_insert(e.to_string());
        }
    }

    pub fn records(&self) -> u64 {
        self.writer.records()
    }

    pub fn elapsed_us(&self, now_us: u64) -> u64 {
        self.rel(now_us)
    }

    pub fn flush(&mut self) {
        if let Err(e) = self.writer.flush() {
            self.error.get_or_insert(e.to_string());
        }
    }

    pub fn finish(mut self) -> Result<u64, String> {
        self.flush();
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(self.writer.records()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Recording;
    use crate::model::Effect;
    use std::sync::{Arc, Mutex};

    /// A `Write` that lands in a shared buffer so the test can read it back.
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn timestamps_are_relative_and_axis_is_throttled() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let initial = [FfbEvent::Upload { id: 1, effect: Effect::constant(0.1) }, FfbEvent::Play { id: 1, count: 1 }];
        let mut rec = Recorder::start(Box::new(Shared(buf.clone())), &Header::default(), 5_000_000, &initial, 100).unwrap();
        rec.event(5_001_000, &FfbEvent::Gain(0.5));
        rec.axis(5_001_000, Some(0.1));
        rec.axis(5_002_000, Some(0.2)); // too soon
        rec.axis(5_011_000, Some(0.2)); // 10 ms later: written
        rec.axis(5_021_000, Some(0.2)); // unchanged: skipped
        rec.axis(5_031_000, None);
        assert_eq!(rec.finish().unwrap(), 5);

        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let r = Recording::read_from(text.as_bytes()).unwrap();
        assert_eq!(r.records[0].t_us, 0);
        assert_eq!(r.records[1].t_us, 0);
        assert_eq!(r.records[2], Record { t_us: 1000, body: RecordBody::Ffb(FfbEvent::Gain(0.5)) });
        assert_eq!(r.records[3], Record { t_us: 1000, body: RecordBody::Axis(0.1) });
        assert_eq!(r.records[4], Record { t_us: 11_000, body: RecordBody::Axis(0.2) });
    }
}
