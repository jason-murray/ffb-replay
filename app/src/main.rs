//! `ffb-replay`: record and play back force-feedback signals.
//!
//! With no arguments it starts the local web UI. Subcommands do the same
//! jobs headlessly.

mod backends;
mod ctrl_c;
mod server;
mod session;

use backends::{SinkSpec, SourceSpec};
use ffb_replay_core::offline;
use ffb_replay_core::pipeline::PipelineConfig;
use ffb_replay_core::player::PlayerOptions;
use ffb_replay_core::Recording;
use session::Session;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const USAGE: &str = "\
ffb-replay - record and play back force-feedback signals

USAGE
  ffb-replay [ui] [--port N] [--dir DIR] [--rate HZ] [--no-open]
      Start the web UI (default). Opens http://127.0.0.1:8975/ in a browser.

  ffb-replay devices
      List input devices and their force-feedback capabilities.

  ffb-replay record [NAME] --source SRC --sink DEV [--seconds N] [--notes TEXT]
      Pass FFB from a virtual wheel through to DEV and record it.
      Recording goes to DIR/NAME.ffbr (NAME defaults to a timestamp).

  ffb-replay play FILE --sink DEV [--speed X] [--loop]
      Play a recording into DEV.

  ffb-replay info FILE
      Describe a recording.

  ffb-replay export FILE [--out FILE.csv] [--rate HZ]
      Write the rendered force trace as CSV (stdout when --out is omitted).

SOURCES (--source)
  virtual     a virtual wheel for the game to bind to (uinput)    [default]
  mirror      a virtual wheel shaped like --sink, inputs mirrored from it
  demo        a synthetic sine-shaped constant force (no hardware)
  demo:loop   the same, looping

SINKS (--sink)
  none        no device: graph (and record) only                   [default]
  DEV         a device path, eventN, or part of a device name

OPTIONS
  --dir DIR     recordings directory                 [default: ./recordings]
  --rate HZ     pipeline tick rate, 50..10000        [default: 1000]
  --no-grab     do not take the sink device exclusively
  --port N      UI port                              [default: 8975]
  -h, --help    this text
";

struct Args {
    positional: Vec<String>,
    options: Vec<(String, Option<String>)>,
}

impl Args {
    /// Options are `--name` or `--name value`; `--name=value` works too.
    /// Which names take a value is listed here so bare flags parse.
    fn parse(args: impl Iterator<Item = String>) -> Args {
        const WITH_VALUE: &[&str] =
            &["port", "dir", "rate", "source", "sink", "seconds", "notes", "speed", "out"];
        let mut positional = Vec::new();
        let mut options = Vec::new();
        let mut it = args.peekable();
        while let Some(a) = it.next() {
            if let Some(opt) = a.strip_prefix("--") {
                if let Some((k, v)) = opt.split_once('=') {
                    options.push((k.to_string(), Some(v.to_string())));
                } else if WITH_VALUE.contains(&opt) {
                    options.push((opt.to_string(), it.next()));
                } else {
                    options.push((opt.to_string(), None));
                }
            } else if a == "-h" {
                options.push(("help".into(), None));
            } else {
                positional.push(a);
            }
        }
        Args { positional, options }
    }

    fn flag(&self, name: &str) -> bool {
        self.options.iter().any(|(k, _)| k == name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.options.iter().rev().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
    }

    fn parsed<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.value(name) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| format!("--{name}: invalid value '{v}'")),
        }
    }
}

fn main() {
    let args = Args::parse(std::env::args().skip(1));
    if args.flag("help") {
        print!("{USAGE}");
        return;
    }
    let cmd = args.positional.first().map(String::as_str).unwrap_or("ui");
    let result = match cmd {
        "ui" => cmd_ui(&args),
        "devices" => cmd_devices(),
        "record" => cmd_record(&args),
        "play" => cmd_play(&args),
        "info" => cmd_info(&args),
        "export" => cmd_export(&args),
        "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn config_from(args: &Args) -> Result<PipelineConfig, String> {
    let tick_hz: u32 = args.parsed("rate", 1000)?;
    if !(50..=10_000).contains(&tick_hz) {
        return Err("--rate must be 50..10000".into());
    }
    Ok(PipelineConfig { tick_hz, axis_sample_hz: 100, sample_every: 1 })
}

fn recordings_dir(args: &Args) -> PathBuf {
    PathBuf::from(args.value("dir").unwrap_or("recordings"))
}

fn cmd_ui(args: &Args) -> Result<(), String> {
    let port: u16 = args.parsed("port", 8975)?;
    let session = Session::new(config_from(args)?, recordings_dir(args));
    ctrl_c::install();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let url = format!("http://{addr}/");
    eprintln!("ffb-replay UI at {url}  (Ctrl+C to quit)");
    if !args.flag("no-open") {
        open_browser(&url);
    }
    let s2 = session.clone();
    let r = server::serve(s2, addr).map_err(|e| format!("serve {addr}: {e}"));
    session.stop();
    r
}

fn open_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "start", "", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

fn cmd_devices() -> Result<(), String> {
    let j = backends::devices_json();
    let list = j.as_arr().unwrap_or(&[]);
    if list.is_empty() {
        println!("no input devices found (on Linux, check read access to /dev/input/event*)");
        return Ok(());
    }
    for d in list {
        let get = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let ff = d.get("ff").and_then(|v| v.as_bool()).unwrap_or(false);
        let effects: Vec<&str> = d
            .get("effects")
            .and_then(|v| v.as_arr())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        println!(
            "{:<22} {:<40} {}{}",
            get("path"),
            get("name"),
            if ff {
                format!(
                    "FFB: {} (max {} effects)",
                    effects.join(" "),
                    d.get("max_effects").and_then(|v| v.as_u64()).unwrap_or(0)
                )
            } else {
                "no FFB".into()
            },
            if d.get("virtual").and_then(|v| v.as_bool()).unwrap_or(false) {
                "  [virtual]"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn print_stats_line(session: &Session, start: Instant) {
    let st = session.status_json();
    let f = |k: &str| st.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let u = |k: &str| st.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    eprintln!(
        "{:>6.1}s  events {:>7}  force {:>+6.3}  axis {}  playing {}  gain {:.2}  late {}  sink errors {}",
        start.elapsed().as_secs_f64(),
        u("events_total"),
        f("force"),
        st.get("axis").and_then(|v| v.as_f64()).map(|a| format!("{a:>+6.3}")).unwrap_or_else(|| "   -  ".into()),
        u("playing"),
        f("gain"),
        u("late_ticks"),
        u("sink_errors"),
    );
}

fn cmd_record(args: &Args) -> Result<(), String> {
    let name = args.positional.get(1).map(String::as_str).unwrap_or("");
    let source = SourceSpec::parse(args.value("source").unwrap_or("virtual"))?;
    let sink = SinkSpec::parse(args.value("sink").unwrap_or("none"));
    let seconds: f64 = args.parsed("seconds", 0.0)?;
    let session = Session::new(config_from(args)?, recordings_dir(args));
    ctrl_c::install();
    session.start_live(&source, &sink, !args.flag("no-grab"))?;
    session.start_recording(name, args.value("notes").unwrap_or(""))?;
    eprintln!("press Ctrl+C to stop");
    let start = Instant::now();
    let mut next_line = Instant::now() + Duration::from_secs(1);
    while !ctrl_c::requested() && session.is_running() {
        if seconds > 0.0 && start.elapsed().as_secs_f64() >= seconds {
            break;
        }
        if Instant::now() >= next_line {
            next_line += Duration::from_secs(1);
            print_stats_line(&session, start);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = session.stop_recording();
    session.stop();
    if let Some(s) = session.last_summary() {
        if let ffb_replay_core::pipeline::Outcome::Error(e) = s.outcome {
            return Err(e);
        }
    }
    Ok(())
}

fn cmd_play(args: &Args) -> Result<(), String> {
    let Some(file) = args.positional.get(1) else { return Err("play: missing FILE".into()) };
    let sink = SinkSpec::parse(args.value("sink").unwrap_or("none"));
    let options = PlayerOptions { speed: args.parsed("speed", 1.0)?, looped: args.flag("loop") };
    let session = Session::new(config_from(args)?, recordings_dir(args));
    ctrl_c::install();
    let path = session.resolve_recording(file);
    session.start_playback(&path, &sink, options, !args.flag("no-grab"))?;
    let start = Instant::now();
    let mut next_line = Instant::now() + Duration::from_secs(1);
    while !ctrl_c::requested() && session.is_running() {
        if Instant::now() >= next_line {
            next_line += Duration::from_secs(1);
            print_stats_line(&session, start);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    session.stop();
    if let Some(s) = session.last_summary() {
        if let ffb_replay_core::pipeline::Outcome::Error(e) = s.outcome {
            return Err(e);
        }
    }
    Ok(())
}

fn load(file: &str) -> Result<(PathBuf, Recording), String> {
    let path = Path::new(file);
    let rec = Recording::load(path).map_err(|e| format!("{file}: {e}"))?;
    Ok((path.to_path_buf(), rec))
}

fn cmd_info(args: &Args) -> Result<(), String> {
    let Some(file) = args.positional.get(1) else { return Err("info: missing FILE".into()) };
    let (path, rec) = load(file)?;
    let h = &rec.header;
    let st = offline::stats(&rec, 500);
    println!("{}", path.display());
    println!("  format version  {}", h.version);
    println!(
        "  created         {}",
        if h.created_unix > 0 { format!("{} (unix)", h.created_unix) } else { "unknown".into() }
    );
    println!("  source          {}", h.source);
    println!("  sink            {}", h.sink);
    println!("  tick rate       {} Hz", h.tick_hz);
    if !h.notes.is_empty() {
        println!("  notes           {}", h.notes);
    }
    println!("  duration        {:.3} s", rec.duration_us() as f64 / 1e6);
    println!(
        "  records         {} ({} FFB events, {} axis samples)",
        rec.records.len(),
        rec.ffb_event_count(),
        st.axis_samples
    );
    println!(
        "  events          upload {}  erase {}  play {}  stop {}  gain {}  autocenter {}  control {}",
        st.uploads, st.erases, st.plays, st.stops, st.gains, st.autocenters, st.controls
    );
    println!(
        "  effect types    {}",
        if st.effect_types.is_empty() { "-".into() } else { st.effect_types.join(", ") }
    );
    println!(
        "  rendered force  peak {:.3}  mean |f| {:.3}  clipped {:.1}%",
        st.peak_force,
        st.mean_abs_force,
        st.clipped_fraction * 100.0
    );
    Ok(())
}

fn cmd_export(args: &Args) -> Result<(), String> {
    let Some(file) = args.positional.get(1) else { return Err("export: missing FILE".into()) };
    let (_, rec) = load(file)?;
    let rate: u32 = args.parsed("rate", 1000)?;
    let trace = offline::render_trace(&rec, rate);
    let mut out = String::with_capacity(trace.len() * 48);
    out.push_str("t_s,force,constant,ramp,periodic,condition,rumble,gain,playing,axis\n");
    for p in &trace {
        let r = &p.rendered;
        out.push_str(&format!(
            "{:.6},{:.5},{:.5},{:.5},{:.5},{:.5},{:.5},{:.3},{},{}\n",
            p.t_us as f64 / 1e6,
            r.total,
            r.constant,
            r.ramp,
            r.periodic,
            r.condition,
            r.rumble,
            r.gain,
            r.playing,
            p.axis.map(|a| format!("{a:.5}")).unwrap_or_default()
        ));
    }
    match args.value("out") {
        Some(path) => {
            std::fs::write(path, out).map_err(|e| format!("{path}: {e}"))?;
            eprintln!("wrote {} rows to {path}", trace.len());
        }
        None => print!("{out}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_flags_values_and_positionals() {
        let a = Args::parse(
            ["record", "lap", "--sink", "event5", "--loop", "--rate=500", "--seconds", "3"]
                .iter()
                .map(|s| s.to_string()),
        );
        assert_eq!(a.positional, vec!["record", "lap"]);
        assert_eq!(a.value("sink"), Some("event5"));
        assert!(a.flag("loop"));
        assert_eq!(a.parsed::<u32>("rate", 1000).unwrap(), 500);
        assert_eq!(a.parsed::<f64>("seconds", 0.0).unwrap(), 3.0);
        assert!(a.parsed::<u32>("sink", 0).is_err());
        assert_eq!(a.parsed::<u16>("port", 8975).unwrap(), 8975);
    }
}
