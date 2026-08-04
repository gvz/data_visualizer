//! WebSocket Influx line-protocol load generator.
//!
//! Connects to a datavis `--ws-listen` server and pushes a line-protocol frame
//! at a fixed frequency. Each frame carries one measurement with `--channels`
//! numeric fields (`ch0..chN-1`) plus `--state-channels` string fields
//! (`state0..stateM-1`), so the server discovers `measurement/ch0` … and
//! `measurement/state0` … as separate channels. Numeric values are per-channel
//! sine waves so the waveform panels show motion; string values cycle through
//! `--states` labels so the state-chart panel shows transitions.
//!
//! Run:
//!   cargo run --example ws_influx_load -- \
//!       --url ws://127.0.0.1:9001 --channels 8 --freq 5 --measurement load
//!
//! State-chart test (2 string channels cycling idle/running/error/done):
//!   cargo run --example ws_influx_load -- --channels 0 --state-channels 2
//!
//! Parameters (all optional):
//!   --url <ws-url>         default ws://127.0.0.1:9001
//!   --channels <N>         number of numeric channels/fields per frame (default 8)
//!   --state-channels <M>   number of string/log channels per frame (default 0)
//!   --states <a,b,c>       comma-separated state labels to cycle
//!                          (default "idle,running,error,done")
//!   --state-dwell <SECS>   seconds each channel holds a state (default 2.0)
//!   --freq <HZ>            frames per second (default 2.0)
//!   --sine-freq <HZ>       sine wave frequency of the field values (default 0.2)
//!   --measurement <name>   Influx measurement name (default "load")
//!   --duration <SECS>      stop after this many seconds (default 0 = run forever)

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tungstenite::Message;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = arg(&args, "--url").unwrap_or_else(|| "ws://127.0.0.1:9001".to_string());
    let channels: usize = arg(&args, "--channels")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let state_channels: usize = arg(&args, "--state-channels")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let states: Vec<String> = arg(&args, "--states")
        .unwrap_or_else(|| "idle,running,error,done".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let state_dwell: f64 = arg(&args, "--state-dwell")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0);
    let freq: f64 = arg(&args, "--freq").and_then(|s| s.parse().ok()).unwrap_or(2.0);
    let sine_freq: f64 = arg(&args, "--sine-freq")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.2);
    let measurement = arg(&args, "--measurement").unwrap_or_else(|| "load".to_string());
    let duration: f64 = arg(&args, "--duration")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    if channels == 0 && state_channels == 0 {
        eprintln!("--channels and --state-channels cannot both be 0");
        std::process::exit(1);
    }
    if state_channels > 0 && states.is_empty() {
        eprintln!("--states must list at least one label");
        std::process::exit(1);
    }
    if state_channels > 0 && state_dwell <= 0.0 {
        eprintln!("--state-dwell must be > 0");
        std::process::exit(1);
    }
    if freq <= 0.0 {
        eprintln!("--freq must be > 0");
        std::process::exit(1);
    }

    let (mut ws, _resp) = match tungstenite::connect(&url) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("connect {url} failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "connected to {url}: {channels} numeric + {state_channels} state channels @ {freq} Hz, {sine_freq} Hz sine on measurement '{measurement}'{}",
        if duration > 0.0 { format!(" for {duration}s") } else { " (forever, Ctrl-C to stop)".to_string() }
    );

    let period = Duration::from_secs_f64(1.0 / freq);
    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut last_report = start;

    loop {
        let elapsed = start.elapsed().as_secs_f64();
        if duration > 0.0 && elapsed >= duration {
            break;
        }

        let line = build_line(
            &measurement,
            channels,
            state_channels,
            &states,
            state_dwell,
            elapsed,
            sine_freq,
        );
        if let Err(e) = ws.send(Message::Text(line)) {
            eprintln!("send failed: {e}");
            break;
        }
        sent += 1;

        if last_report.elapsed() >= Duration::from_secs(1) {
            let fields_per_frame = (channels + state_channels) as u64;
            println!("sent {sent} frames ({} values)", sent * fields_per_frame);
            last_report = Instant::now();
        }

        std::thread::sleep(period);
    }

    let _ = ws.close(None);
    println!("done: {sent} frames sent");
}

/// One Influx line: `measurement ch0=<v>,…,state0="<label>",… <ts_ns>`.
///
/// Numeric fields carry phase-shifted sines; state fields are quoted strings
/// (Influx string type) that step through `states` once per `dwell` seconds,
/// each channel offset so they transition at different times.
fn build_line(
    measurement: &str,
    channels: usize,
    state_channels: usize,
    states: &[String],
    dwell: f64,
    elapsed: f64,
    sine_freq: f64,
) -> String {
    let mut fields = String::new();
    let mut first = true;
    for i in 0..channels {
        if !first {
            fields.push(',');
        }
        first = false;
        // Distinct sine per channel: `sine_freq` base, phase-shifted by index.
        let v = (elapsed * sine_freq * std::f64::consts::TAU + i as f64 * 0.7).sin();
        fields.push_str(&format!("ch{i}={v:.4}"));
    }
    for i in 0..state_channels {
        if !first {
            fields.push(',');
        }
        first = false;
        // Step through states once per `dwell`; offset each channel by one state.
        let step = (elapsed / dwell) as usize;
        let label = &states[(step + i) % states.len()];
        // Quoted → Influx string field; escape any embedded quotes.
        fields.push_str(&format!("state{i}=\"{}\"", label.replace('"', "\\\"")));
    }
    format!("{measurement} {fields} {}", now_ns())
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Value following `flag` in `args`, if present.
fn arg(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}
