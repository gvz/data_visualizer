use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ingest::decode::decode_batch;
use crate::ingest::router::TopicRouter;
use crate::ingest::{CONNECTING, LIVE, TIMEOUT};
use crate::record::RecordMsg;
use crate::store::ChannelStore;

pub fn run_loop(
    endpoint: String,
    router: TopicRouter,
    store: Arc<dyn ChannelStore>,
    state: Arc<AtomicU8>,
    record_sender: Arc<std::sync::Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
    stop: Arc<AtomicBool>,
) {
    let mut backoff_ms = 100u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        state.store(CONNECTING, Ordering::Relaxed);
        match connect_and_recv(&endpoint, &router, store.as_ref(), &state, &record_sender, &stop) {
            Ok(()) => {
                backoff_ms = 100;
            }
            Err(e) => {
                eprintln!("ingest: recv loop error: {e}; reconnecting in {backoff_ms}ms");
                std::thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(5_000);
            }
        }
    }
}

fn connect_and_recv(
    endpoint: &str,
    router: &TopicRouter,
    store: &dyn ChannelStore,
    state: &Arc<AtomicU8>,
    record_sender: &Arc<std::sync::Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }

    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::SUB)?;
    socket.set_rcvtimeo(1_000)?;
    for topic in router.topics() {
        socket.set_subscribe(topic.as_bytes())?;
    }
    socket.connect(endpoint)?;

    // Assume no data has arrived yet; treat as if last_live was 10s ago.
    let mut last_live = Instant::now() - Duration::from_secs(10);

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match socket.recv_multipart(0) {
            Ok(parts) if parts.len() >= 2 => {
                let topic = std::str::from_utf8(&parts[0]).unwrap_or("");
                let bindings = router.bindings_for(topic);
                decode_batch(&parts[1], bindings, store);
                state.store(LIVE, Ordering::Relaxed);
                last_live = Instant::now();

                // Push to recorder queue if recording is active.
                if let Ok(guard) = record_sender.try_lock() {
                    if let Some(tx) = guard.as_ref() {
                        let log_time_ns = crate::types::now_ns();
                        let topic_arc: Arc<str> = topic.into();
                        let _ = tx.try_send(crate::record::RecordMsg::Proto {
                            topic: topic_arc,
                            data: parts[1].clone(),
                            ts: log_time_ns,
                        });
                    }
                }
            }
            Ok(_) => {
                // Malformed multipart (wrong frame count); ignore.
            }
            Err(zmq::Error::EAGAIN) => {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                if last_live.elapsed() > Duration::from_secs(5) {
                    state.store(TIMEOUT, Ordering::Relaxed);
                }
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn connect_and_recv_returns_when_stopped() {
        // No publisher on this port → recv times out (EAGAIN) each second.
        // With stop set, connect_and_recv must return Ok(()) promptly.
        let router = crate::ingest::router::TopicRouter::empty();
        let store: Arc<dyn crate::store::ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(
                &crate::config::ChannelRegistry::from_toml_str("").unwrap()));
        let state = Arc::new(AtomicU8::new(crate::ingest::CONNECTING));
        let rec = Arc::new(std::sync::Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(true)); // already stopped
        let r = connect_and_recv("tcp://localhost:5599", &router, store.as_ref(),
                                 &state, &rec, &stop);
        assert!(r.is_ok());
    }
}
