use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{bounded, Receiver, Sender};

use super::cache::ChunkCache;
use super::decode_buf::DecodedChunk;
use super::source::RecordingSource;
use crate::types::ChannelMeta;

/// A decode request: the `(source, chunk, topic_key)` cache key.
pub type Job = (usize, usize, u32);

/// Bounded job-queue capacity. Small so fast scrubbing that outpaces decode
/// drops stale requests (via `try_send`) instead of growing an unbounded
/// backlog; still-visible windows are re-requested next frame.
const QUEUE_CAP: usize = 64;

/// Read-only decode state shared by the store and the worker pool. Every field
/// a decode reads (mmap, parsed summary, bindings) is immutable after `open`,
/// so decoding is safe from any number of threads.
pub struct DecodeShared {
    pub sources: Vec<RecordingSource>,
    pub cache: ChunkCache,
    pub metas: Vec<ChannelMeta>,
    /// Keys queued or being decoded, so a key is enqueued at most once.
    in_flight: Mutex<HashSet<Job>>,
    /// Set by the app to `move || ctx.request_repaint()`. `None` in headless
    /// contexts and tests (wake is then a no-op).
    waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Live worker count, for the lifecycle test to observe shutdown.
    active: Arc<AtomicUsize>,
}

impl DecodeShared {
    pub fn new(sources: Vec<RecordingSource>, cache: ChunkCache, metas: Vec<ChannelMeta>) -> Self {
        Self {
            sources,
            cache,
            metas,
            in_flight: Mutex::new(HashSet::new()),
            waker: Mutex::new(None),
            active: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Cached chunk without decoding (render-thread probe).
    pub fn cached(&self, key: Job) -> Option<Arc<DecodedChunk>> {
        self.cache.get(key)
    }

    /// Decode one chunk (if not already cached), clear its in-flight mark, and
    /// wake the UI. Runs on a worker thread.
    pub fn decode_and_cache(&self, key: Job) {
        let (si, span_idx, topic_key) = key;
        self.cache.get_or_insert_with(key, || {
            self.sources[si].decode_chunk_topic(span_idx, topic_key, &self.metas)
        });
        self.in_flight.lock().unwrap().remove(&key);
        self.wake();
    }

    /// Reserve `key` for decode if it is neither cached nor already in flight.
    /// Returns true iff the caller should now `try_send` it. Locks the cache
    /// first, releases it, then locks `in_flight` — the SAME order as
    /// `decode_and_cache` (cache → in_flight) so the two never deadlock.
    pub fn reserve(&self, key: Job) -> bool {
        if self.cache.get(key).is_some() {
            return false;
        }
        let mut inflight = self.in_flight.lock().unwrap();
        if inflight.contains(&key) {
            return false;
        }
        inflight.insert(key);
        true
    }

    /// Undo a `reserve` whose `try_send` failed (queue full), so the key is
    /// retried on a later frame.
    pub fn unreserve(&self, key: Job) {
        self.in_flight.lock().unwrap().remove(&key);
    }

    pub fn set_waker(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.waker.lock().unwrap() = Some(f);
    }

    pub fn wake(&self) {
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w();
        }
    }

    #[cfg(test)]
    pub fn active_workers(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

/// Spawn `count` detached workers draining a bounded job queue. Returns the
/// `(Sender, Receiver)` pair; dropping every `Sender` closes the channel and
/// each worker's `recv` returns `Err`, ending the loop. The `Receiver` is
/// returned too (each worker keeps its own clone) so callers — namely tests —
/// can retain a clone for synchronous draining without racing the pool.
pub fn spawn_decode_workers(shared: Arc<DecodeShared>, count: usize) -> (Sender<Job>, Receiver<Job>) {
    let (tx, rx) = bounded::<Job>(QUEUE_CAP);
    for _ in 0..count {
        let shared = shared.clone();
        let worker_rx = rx.clone();
        shared.active.fetch_add(1, Ordering::SeqCst);
        let active = shared.active.clone();
        std::thread::spawn(move || {
            while let Ok(key) = worker_rx.recv() {
                shared.decode_and_cache(key);
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    (tx, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::lazy::decode_buf::ChunkDecodeBuf;
    use std::sync::atomic::AtomicUsize as StdAtomicUsize;

    /// A trivially-built decoded chunk, mirroring `cache.rs`'s `chunk(n)` test
    /// helper: a single numeric channel with `n` samples.
    fn chunk(n: usize) -> DecodedChunk {
        use crate::config::ChannelRegistry;
        use crate::store::ChannelStore;
        use crate::types::NumericVal;
        let r = ChannelRegistry::from_toml_str(
            r#"
[channels."a.f"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#,
        )
        .unwrap();
        let buf = ChunkDecodeBuf::new(&r);
        let id = r.id("a.f").unwrap();
        for i in 0..n as i64 {
            buf.write_numeric(id, i, NumericVal::Float(i as f64));
        }
        buf.freeze()
    }

    fn empty_shared() -> DecodeShared {
        DecodeShared::new(vec![], ChunkCache::new(10 * 1024 * 1024), vec![])
    }

    #[test]
    fn reserve_true_for_fresh_key_false_when_already_reserved() {
        let shared = empty_shared();
        let key: Job = (0, 0, 0);
        assert!(shared.reserve(key), "fresh key must reserve");
        assert!(!shared.reserve(key), "already-reserved key must not re-reserve");
    }

    #[test]
    fn unreserve_frees_key_so_reserve_succeeds_again() {
        let shared = empty_shared();
        let key: Job = (0, 0, 0);
        assert!(shared.reserve(key));
        assert!(!shared.reserve(key));
        shared.unreserve(key);
        assert!(shared.reserve(key), "unreserve must free the key for re-reservation");
    }

    #[test]
    fn reserve_false_for_key_already_in_cache() {
        let shared = empty_shared();
        let key: Job = (0, 0, 0);
        let _ = shared.cache.get_or_insert_with(key, || chunk(10));
        assert!(!shared.reserve(key), "cached key must not be reserved for decode");
    }

    #[test]
    fn waker_fires_on_wake() {
        let shared = empty_shared();
        let hits = Arc::new(StdAtomicUsize::new(0));
        let hits2 = hits.clone();
        shared.set_waker(Box::new(move || {
            hits2.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        shared.wake();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        shared.wake();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn wake_without_waker_is_a_noop() {
        let shared = empty_shared();
        shared.wake(); // must not panic
    }

    #[test]
    fn worker_pool_shuts_down_when_sender_dropped() {
        let shared = Arc::new(empty_shared());
        let (tx, _rx) = spawn_decode_workers(shared.clone(), 2);

        // Bounded spin until the workers have registered themselves (no sleep).
        let mut spins = 0;
        while shared.active_workers() != 2 && spins < 10_000 {
            std::thread::yield_now();
            spins += 1;
        }
        assert_eq!(shared.active_workers(), 2, "both workers should be live before shutdown");

        drop(tx); // closes the channel; no job ever enqueued

        let mut spins = 0;
        while shared.active_workers() != 0 && spins < 10_000 {
            std::thread::yield_now();
            spins += 1;
        }
        assert_eq!(shared.active_workers(), 0, "workers must exit once the Sender is dropped");
    }
}
