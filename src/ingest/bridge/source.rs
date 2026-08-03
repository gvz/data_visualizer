use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::Sender;

use crate::config::ChannelRegistry;
use crate::ingest::bridge::config::BridgeConfig;
use crate::ingest::bridge::frame::{FrameError, FrameReader};
use crate::ingest::bridge::router::BridgeRouter;
use crate::ingest::bridge::schema::{batch_schema_bytes, pb};
use crate::ingest::source::{ChildGuard, DataSource, SourceHandle};
use crate::ingest::{CONNECTING, LIVE, TIMEOUT};
use crate::record::RecordMsg;
use crate::store::ChannelStore;
use prost::Message;

/// Idle window before the status indicator drops from LIVE to TIMEOUT.
const TIMEOUT_AFTER: Duration = Duration::from_secs(5);

/// A `DataSource` that spawns an external adapter and reads a fixed columnar
/// `Batch` stream off its stdout.
pub struct SubprocessSource {
    cfg: BridgeConfig,
    router: BridgeRouter,
    schema_bytes: Vec<u8>,
}

impl SubprocessSource {
    pub fn new(cfg: BridgeConfig, registry: &ChannelRegistry) -> Self {
        Self {
            cfg,
            router: BridgeRouter::build(registry),
            schema_bytes: batch_schema_bytes().to_vec(),
        }
    }
}

impl DataSource for SubprocessSource {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let this = *self;
        let name = this.cfg.name.clone();
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));
        let record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let current: Arc<Mutex<Option<std::process::Child>>> = Arc::new(Mutex::new(None));
        let last_frame_ns = Arc::new(AtomicI64::new(0));
        let schema_bytes = this.schema_bytes.clone();

        // Reader / restart thread.
        {
            let conn = conn_state.clone();
            let rec = record_sender.clone();
            let stop = stop.clone();
            let current = current.clone();
            let last = last_frame_ns.clone();
            std::thread::spawn(move || {
                run_loop(this.cfg, this.router, store, conn, rec, stop, current, last);
            });
        }

        // Watchdog: downgrade LIVE → TIMEOUT after an idle gap.
        {
            let conn = conn_state.clone();
            let stop = stop.clone();
            let last = last_frame_ns.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    if conn.load(Ordering::Relaxed) == LIVE {
                        let last_ns = last.load(Ordering::Relaxed);
                        if crate::types::now_ns() - last_ns > TIMEOUT_AFTER.as_nanos() as i64 {
                            conn.store(TIMEOUT, Ordering::Relaxed);
                        }
                    }
                }
            });
        }

        SourceHandle {
            name,
            conn_state,
            record_sender,
            discovery: None,
            schema_bytes: Some(schema_bytes),
            child_guard: Some(ChildGuard { stop, current }),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    cfg: BridgeConfig,
    router: BridgeRouter,
    store: Arc<dyn ChannelStore>,
    conn: Arc<AtomicU8>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    stop: Arc<AtomicBool>,
    current: Arc<Mutex<Option<std::process::Child>>>,
    last_frame_ns: Arc<AtomicI64>,
) {
    let topic: std::sync::Arc<str> = std::sync::Arc::from(cfg.name.as_str());
    let mut backoff = Duration::from_millis(250);
    while !stop.load(Ordering::Relaxed) {
        conn.store(CONNECTING, Ordering::Relaxed);

        let mut child = match Command::new(&cfg.command)
            .args(&cfg.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("bridge {:?}: failed to spawn {:?}: {e}", cfg.name, cfg.command);
                if sleep_or_stop(&stop, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        };

        let stdout = child.stdout.take().expect("piped stdout");
        // Forward the child's stderr to our log, prefixed with the source name.
        if let Some(stderr) = child.stderr.take() {
            let name = cfg.name.clone();
            std::thread::spawn(move || log_stderr(name, stderr));
        }
        *current.lock().unwrap() = Some(child);
        // Close the orphan window: if `ChildGuard::drop` fired between spawn and
        // the store above, it set `stop` and found `current` empty (killing
        // nothing). Re-check `stop` now that the child is stored — if drop
        // already ran, reap the child ourselves so it never outlives datavis.
        if stop.load(Ordering::Relaxed) {
            reap(&current);
            return;
        }

        match read_stream(stdout, &router, store.as_ref(), &conn, &record_sender, &last_frame_ns, &topic) {
            StreamEnd::PermanentProtocol => {
                eprintln!("bridge {:?}: protocol mismatch; not restarting", cfg.name);
                conn.store(TIMEOUT, Ordering::Relaxed);
                reap(&current);
                return; // permanent — do not respawn
            }
            StreamEnd::Ended => {
                reap(&current);
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                eprintln!("bridge {:?}: child ended; restarting in {:?}", cfg.name, backoff);
                if sleep_or_stop(&stop, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
        // A stream that produced frames resets the backoff on the next healthy run.
        if conn.load(Ordering::Relaxed) == LIVE {
            backoff = Duration::from_millis(250);
        }
    }
}

enum StreamEnd {
    PermanentProtocol,
    Ended,
}

#[allow(clippy::too_many_arguments)]
fn read_stream<R: Read>(
    stdout: R,
    router: &BridgeRouter,
    store: &dyn ChannelStore,
    conn: &Arc<AtomicU8>,
    record_sender: &Arc<Mutex<Option<Sender<RecordMsg>>>>,
    last_frame_ns: &Arc<AtomicI64>,
    topic: &std::sync::Arc<str>,
) -> StreamEnd {
    let mut reader = FrameReader::new(stdout);
    if let Err(e) = reader.read_preamble() {
        return match e {
            FrameError::BadPreamble => StreamEnd::PermanentProtocol,
            _ => StreamEnd::Ended, // child died before/mid preamble → restart
        };
    }
    loop {
        match reader.next_frame() {
            Ok(None) => return StreamEnd::Ended, // clean EOF
            Ok(Some(body)) => {
                match pb::Batch::decode(body.as_slice()) {
                    Ok(batch) => {
                        router.apply(&batch, store);
                    }
                    Err(e) => {
                        eprintln!("bridge: batch decode error: {e}; skipping frame");
                        continue;
                    }
                }
                conn.store(LIVE, Ordering::Relaxed);
                last_frame_ns.store(crate::types::now_ns(), Ordering::Relaxed);
                forward_to_recorder(record_sender, topic, &body);
            }
            Err(FrameError::BadPreamble) => return StreamEnd::PermanentProtocol,
            Err(e) => {
                eprintln!("bridge: frame error: {e}");
                return StreamEnd::Ended; // oversized/io → restart
            }
        }
    }
}

fn forward_to_recorder(
    record_sender: &Arc<Mutex<Option<Sender<RecordMsg>>>>,
    topic: &std::sync::Arc<str>,
    body: &[u8],
) {
    if let Ok(guard) = record_sender.try_lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.try_send(RecordMsg::Proto {
                topic: topic.clone(),
                data: body.to_vec(),
                ts: crate::types::now_ns(),
            });
        }
    }
}

fn log_stderr(name: String, stderr: impl Read) {
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(stderr);
    for line in reader.lines().map_while(Result::ok) {
        eprintln!("bridge {name}: {line}");
    }
}

fn reap(current: &Arc<Mutex<Option<std::process::Child>>>) {
    if let Ok(mut guard) = current.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Sleep for `dur`, waking early if `stop` is set. Returns `true` if stopping.
fn sleep_or_stop(stop: &Arc<AtomicBool>, dur: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use crate::config::ChannelRegistry;
    use crate::ingest::source::DataSource;
    use crate::ingest::LIVE;
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::{ChannelSnapshot, TimeWindow};

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."accel"]
topic = "accel"
type = "float"

[channels."state"]
topic = "state"
type = "int"

[channels."log"]
topic = "log"
type = "text"
"#,
        )
        .unwrap()
    }

    // Path to the compiled `echo_bridge` example (built alongside tests).
    fn echo_bridge_bin() -> std::path::PathBuf {
        // target/<profile>/examples/echo_bridge, relative to the test binary.
        let mut dir = std::env::current_exe().unwrap();
        dir.pop(); // test binary name
        if dir.ends_with("deps") {
            dir.pop();
        }
        dir.join("examples").join(if cfg!(windows) { "echo_bridge.exe" } else { "echo_bridge" })
    }

    fn wait_until<F: Fn() -> bool>(f: F, within: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < within {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    #[test]
    fn spawns_child_and_ingests_samples() {
        let reg = registry();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let cfg = crate::ingest::bridge::config::BridgeConfig {
            name: "echo".into(),
            command: echo_bridge_bin().to_string_lossy().into_owned(),
            args: vec![],
        };
        let src = SubprocessSource::new(cfg, &reg);
        let handle = Box::new(src).spawn(store.clone());

        let accel = reg.id("accel").unwrap();
        let got = wait_until(
            || matches!(store.snapshot(accel, ALL), ChannelSnapshot::Float { ts, .. } if ts.len() == 3),
            Duration::from_secs(5),
        );
        assert!(got, "expected 3 accel samples from the child");
        assert_eq!(handle.conn_state.load(Ordering::Relaxed), LIVE);
        assert!(handle.child_guard.is_some());
        assert!(handle.schema_bytes.as_ref().is_some_and(|b| !b.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_handle_kills_child() {
        let reg = registry();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        // `sleep` never emits a frame and never exits on its own.
        let cfg = crate::ingest::bridge::config::BridgeConfig {
            name: "sleeper".into(),
            command: "sleep".into(),
            args: vec!["30".into()],
        };
        // Preamble will never arrive; the child stays alive until we drop.
        let src = SubprocessSource::new(cfg, &reg);
        let handle = Box::new(src).spawn(store);
        // Give the thread time to spawn the child.
        std::thread::sleep(Duration::from_millis(200));
        let current = handle.child_guard.as_ref().unwrap().current.clone();
        let pid = current.lock().unwrap().as_ref().map(|c| c.id());
        assert!(pid.is_some(), "child should be running");
        drop(handle); // ChildGuard::drop kills + reaps it
        // After drop, the shared slot is emptied.
        assert!(current.lock().unwrap().is_none());
    }
}
