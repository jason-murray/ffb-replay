//! A small HTTP server for the browser UI: static page, JSON control
//! endpoints, and a server-sent-events stream of samples and status.
//! Binds to localhost only.

use crate::backends::{self, SinkSpec, SourceSpec};
use crate::ctrl_c;
use crate::session::Session;
use ffb_replay_core::json::Json;
use ffb_replay_core::model::{DeviceControl, FfbEvent};
use ffb_replay_core::offline;
use ffb_replay_core::pipeline::Sample;
use ffb_replay_core::player::PlayerOptions;
use ffb_replay_core::Recording;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub const UI_HTML: &str = include_str!("../ui/index.html");

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty request"));
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let path = percent_decode(path);
    let query = parse_query(query);

    let mut content_length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 1 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(Request { method, path, query, body })
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn json_response(stream: &mut TcpStream, status: u16, j: &Json) -> io::Result<()> {
    respond(stream, status, "application/json", j.to_string().as_bytes())
}

fn ok(stream: &mut TcpStream) -> io::Result<()> {
    json_response(stream, 200, &Json::obj().with("ok", true))
}

fn err_response(stream: &mut TcpStream, status: u16, message: impl Into<String>) -> io::Result<()> {
    json_response(stream, status, &Json::obj().with("ok", false).with("error", message.into()))
}

/// Encode a batch of samples compactly: arrays of
/// `[t_ms, total, constant, ramp, periodic, condition, rumble, axis]`.
fn samples_json(samples: &[Sample], seq: u64) -> Json {
    let rows: Vec<Json> = samples
        .iter()
        .map(|s| {
            Json::Arr(vec![
                Json::Num(s.t_us as f64 / 1000.0),
                Json::from(s.rendered.total),
                Json::from(s.rendered.constant),
                Json::from(s.rendered.ramp),
                Json::from(s.rendered.periodic),
                Json::from(s.rendered.condition),
                Json::from(s.rendered.rumble),
                Json::from(s.axis),
                Json::from(s.recording),
            ])
        })
        .collect();
    Json::obj().with("seq", seq).with("s", Json::Arr(rows))
}

fn stream_events(stream: &mut TcpStream, session: &Session, since: u64) -> io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    )?;
    stream.write_all(b"retry: 1000\n\n")?;
    let mut seq = since;
    let mut last_status = Instant::now() - Duration::from_secs(1);
    loop {
        if ctrl_c::requested() {
            return Ok(());
        }
        let (samples, newest) = session.samples_since(seq, 4000);
        if !samples.is_empty() {
            seq = newest;
            let msg = format!("event: samples\ndata: {}\n\n", samples_json(&samples, seq));
            stream.write_all(msg.as_bytes())?;
        }
        if last_status.elapsed() >= Duration::from_millis(500) {
            last_status = Instant::now();
            let msg = format!("event: status\ndata: {}\n\n", session.status_json());
            stream.write_all(msg.as_bytes())?;
        }
        stream.flush()?;
        std::thread::sleep(Duration::from_millis(33));
    }
}

fn body_json(req: &Request) -> Json {
    if req.body.is_empty() {
        return Json::obj();
    }
    std::str::from_utf8(&req.body).ok().and_then(|s| Json::parse(s).ok()).unwrap_or(Json::obj())
}

fn str_field<'a>(j: &'a Json, key: &str) -> Option<&'a str> {
    j.get(key).and_then(Json::as_str)
}

fn handle(mut stream: TcpStream, session: Session) {
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(_) => return,
    };
    let result = route(&mut stream, &req, &session);
    if let Err(e) = result {
        if e.kind() != io::ErrorKind::BrokenPipe && e.kind() != io::ErrorKind::ConnectionReset {
            let _ = err_response(&mut stream, 500, e.to_string());
        }
    }
}

fn route(stream: &mut TcpStream, req: &Request, session: &Session) -> io::Result<()> {
    let body = body_json(req);
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(stream, 200, "text/html; charset=utf-8", UI_HTML.as_bytes())
        }
        ("GET", "/api/status") => json_response(stream, 200, &session.status_json()),
        ("GET", "/api/devices") => json_response(stream, 200, &backends::devices_json()),
        ("GET", "/api/recordings") => json_response(stream, 200, &session.recordings_json()),
        ("GET", "/api/stream") => {
            let since = req.query.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
            stream_events(stream, session, since)
        }
        ("GET", "/api/recording") => {
            let Some(name) = req.query.get("file") else {
                return err_response(stream, 400, "missing 'file'");
            };
            let path = session.resolve_recording(name);
            match Recording::load(&path) {
                Ok(rec) => {
                    let st = offline::stats(&rec, 200);
                    let h = &rec.header;
                    json_response(
                        stream,
                        200,
                        &Json::obj()
                            .with("path", path.display().to_string())
                            .with("source", h.source.as_str())
                            .with("sink", h.sink.as_str())
                            .with("created_unix", h.created_unix)
                            .with("tick_hz", h.tick_hz)
                            .with("notes", h.notes.as_str())
                            .with("duration_s", rec.duration_us() as f64 / 1e6)
                            .with("events", rec.ffb_event_count() as u64)
                            .with("uploads", st.uploads)
                            .with("plays", st.plays)
                            .with("axis_samples", st.axis_samples)
                            .with(
                                "effect_types",
                                Json::Arr(st.effect_types.iter().map(|s| Json::from(*s)).collect()),
                            )
                            .with("peak_force", st.peak_force)
                            .with("mean_abs_force", st.mean_abs_force)
                            .with("clipped_fraction", st.clipped_fraction),
                    )
                }
                Err(e) => err_response(stream, 400, e.to_string()),
            }
        }
        ("POST", "/api/live") => {
            let source = match SourceSpec::parse(str_field(&body, "source").unwrap_or("virtual")) {
                Ok(s) => s,
                Err(e) => return err_response(stream, 400, e),
            };
            let sink = SinkSpec::parse(str_field(&body, "sink").unwrap_or("none"));
            let grab = body.get("grab").and_then(Json::as_bool).unwrap_or(true);
            match session.start_live(&source, &sink, grab) {
                Ok(()) => ok(stream),
                Err(e) => err_response(stream, 409, e),
            }
        }
        ("POST", "/api/play") => {
            let Some(file) = str_field(&body, "file") else {
                return err_response(stream, 400, "missing 'file'");
            };
            let path = session.resolve_recording(file);
            let sink = SinkSpec::parse(str_field(&body, "sink").unwrap_or("none"));
            let options = PlayerOptions {
                speed: body.get("speed").and_then(Json::as_f32).unwrap_or(1.0),
                looped: body.get("loop").and_then(Json::as_bool).unwrap_or(false),
            };
            let grab = body.get("grab").and_then(Json::as_bool).unwrap_or(true);
            match session.start_playback(&path, &sink, options, grab) {
                Ok(()) => ok(stream),
                Err(e) => err_response(stream, 409, e),
            }
        }
        ("POST", "/api/stop") => {
            session.stop();
            ok(stream)
        }
        ("POST", "/api/record/start") => {
            let name = str_field(&body, "name").unwrap_or("");
            let notes = str_field(&body, "notes").unwrap_or("");
            match session.start_recording(name, notes) {
                Ok(p) => json_response(
                    stream,
                    200,
                    &Json::obj().with("ok", true).with("path", p.display().to_string()),
                ),
                Err(e) => err_response(stream, 409, e),
            }
        }
        ("POST", "/api/record/stop") => match session.stop_recording() {
            Ok(()) => ok(stream),
            Err(e) => err_response(stream, 409, e),
        },
        ("POST", "/api/pause") => {
            let paused = body.get("paused").and_then(Json::as_bool).unwrap_or(true);
            match session.set_paused(paused) {
                Ok(()) => ok(stream),
                Err(e) => err_response(stream, 409, e),
            }
        }
        ("POST", "/api/pulse") => {
            let magnitude = body.get("magnitude").and_then(Json::as_f32).unwrap_or(0.3);
            let ms = body.get("ms").and_then(Json::as_u64).unwrap_or(200) as u32;
            match session.inject_pulse(magnitude, ms) {
                Ok(()) => ok(stream),
                Err(e) => err_response(stream, 409, e),
            }
        }
        ("POST", "/api/reset") => match session.inject(FfbEvent::Control(DeviceControl::Reset)) {
            Ok(()) => ok(stream),
            Err(e) => err_response(stream, 409, e),
        },
        ("GET", _) => err_response(stream, 404, format!("no route for {}", req.path)),
        _ => err_response(stream, 405, "method not allowed"),
    }
}

/// Serve until Ctrl+C. Returns the address actually bound.
pub fn serve(session: Session, addr: SocketAddr) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    loop {
        if ctrl_c::requested() {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let session = session.clone();
                std::thread::Builder::new()
                    .name("ffb-http".into())
                    .spawn(move || handle(stream, session))
                    .ok();
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_percent_decoding() {
        let q = parse_query("file=lap%201.ffbr&since=42&flag");
        assert_eq!(q.get("file").map(String::as_str), Some("lap 1.ffbr"));
        assert_eq!(q.get("since").map(String::as_str), Some("42"));
        assert_eq!(q.get("flag").map(String::as_str), Some(""));
        assert_eq!(percent_decode("a+b%2Fc%"), "a b/c%");
    }

    #[test]
    fn samples_encode_compactly() {
        let s = Sample {
            t_us: 1500,
            rendered: Default::default(),
            axis: None,
            events: 0,
            events_total: 0,
            recording: true,
            recording_us: 0,
            progress: None,
            sink_errors: 0,
            late_ticks: 0,
        };
        let j = samples_json(&[s], 7);
        assert_eq!(j.to_string(), "{\"seq\":7,\"s\":[[1.5,0,0,0,0,0,0,null,true]]}");
    }
}
