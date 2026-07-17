use std::sync::atomic::{AtomicU8, Ordering};
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
) {
    let mut backoff_ms = 100u64;
    loop {
        state.store(CONNECTING, Ordering::Relaxed);
        match connect_and_recv(&endpoint, &router, store.as_ref(), &state, &record_sender) {
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
) -> anyhow::Result<()> {
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
                        let _ = tx.try_send((topic_arc, parts[1].clone(), log_time_ns));
                    }
                }
            }
            Ok(_) => {
                // Malformed multipart (wrong frame count); ignore.
            }
            Err(zmq::Error::EAGAIN) => {
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
