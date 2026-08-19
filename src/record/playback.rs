use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use crate::config::ChannelRegistry;
use crate::record::lazy::{
    ChanSamples, ChunkCache, ChunkDecodeBuf, DecodedChunk, Envelope, RecordingSource,
};
use crate::store::ChannelStore;
use crate::types::{
    ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, SampleType, TimeWindow,
};

const ENVELOPE_BUCKETS: usize = 16384;
const CACHE_BYTES: usize = 512 * 1024 * 1024;
const CHUNK_BUDGET: usize = 16;

/// Ceiling on the envelope grid (`nchannels × buckets`) so foreign recordings
/// that flatten into thousands of channels don't allocate a dense grid that
/// OOMs. At 40 B/cell this caps one envelope near 320 MiB; the load pass builds
/// one per worker thread + one main, so the real peak is that times threads+1.
/// Below this ceiling the requested bucket count is used unchanged (small
/// recordings keep full 16384-bucket overview resolution).
const ENVELOPE_CELL_BUDGET: usize = 8 * 1024 * 1024;
/// Floor on buckets so even a pathological channel count keeps a usable
/// overview rather than collapsing to a handful of buckets.
const ENVELOPE_MIN_BUCKETS: usize = 512;

/// Effective bucket count: the requested value, reduced so the dense
/// `nchannels × buckets` grid stays within [`ENVELOPE_CELL_BUDGET`], but never
/// below [`ENVELOPE_MIN_BUCKETS`].
fn effective_buckets(requested: usize, nchannels: usize) -> usize {
    if nchannels == 0 {
        return requested.max(1);
    }
    let cap = (ENVELOPE_CELL_BUDGET / nchannels).max(ENVELOPE_MIN_BUCKETS);
    requested.min(cap).max(1)
}

/// Tunable memory knobs. `Default` uses the module constants; tests override
/// them to force eviction on small inputs.
pub(crate) struct Caps {
    pub cache_bytes: usize,
    pub envelope_buckets: usize,
    pub chunk_budget: usize,
    pub worker_count: usize,
}

impl Default for Caps {
    fn default() -> Self {
        let worker_count = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
        Caps {
            cache_bytes: CACHE_BYTES,
            envelope_buckets: ENVELOPE_BUCKETS,
            chunk_budget: CHUNK_BUDGET,
            worker_count,
        }
    }
}

pub struct PlaybackStore {
    /// Decode state shared with the background worker pool: sources, cache,
    /// channel metas.
    shared: Arc<crate::record::lazy::DecodeShared>,
    /// Job queue sender, cloned by anything that wants to enqueue a decode
    /// (Task 4).
    #[allow(dead_code)]
    jobs: crossbeam_channel::Sender<crate::record::lazy::Job>,
    /// Retained Receiver clone drained by `run_pending_decodes` (test hook, Task 4).
    #[allow(dead_code)]
    rx: crossbeam_channel::Receiver<crate::record::lazy::Job>,
    /// Numeric channels' decimated min/max overview, built once at load.
    envelope: Envelope,
    /// Text/log channels retained in full; `None` for non-text channels.
    text: Vec<Option<Vec<(i64, String)>>>,
    pub position_ns: Arc<AtomicI64>,
    pub duration_ns: i64,
    pub start_ns: i64,
    /// Sorted start times of each stitched dataset after the first. A line plot
    /// must not connect the sample before one of these to the sample at/after
    /// it, so the join between two recordings is drawn as a gap, not a line.
    breaks: Vec<i64>,
    /// Above this many overlapping chunks a read serves the envelope overview
    /// instead of decoding every chunk.
    chunk_budget: usize,
}

/// Concatenates same-variant channel snapshots in decode order. Chunks are
/// internally time-sorted and decoded in start-time order, so the result stays
/// time-ordered.
enum SnapshotAcc {
    Empty,
    Float { ts: Vec<i64>, vals: Vec<f64> },
    Int { ts: Vec<i64>, vals: Vec<i64> },
    Bool { ts: Vec<i64>, vals: Vec<u8> },
    Text { lines: Vec<(i64, String)> },
}

impl SnapshotAcc {
    fn new() -> Self {
        SnapshotAcc::Empty
    }

    fn extend(&mut self, snap: ChannelSnapshot) {
        match snap {
            ChannelSnapshot::Float { ts, vals } => {
                if let SnapshotAcc::Empty = self {
                    *self = SnapshotAcc::Float { ts: vec![], vals: vec![] };
                }
                if let SnapshotAcc::Float { ts: t, vals: v } = self {
                    t.extend(ts);
                    v.extend(vals);
                }
            }
            ChannelSnapshot::Int { ts, vals } => {
                if let SnapshotAcc::Empty = self {
                    *self = SnapshotAcc::Int { ts: vec![], vals: vec![] };
                }
                if let SnapshotAcc::Int { ts: t, vals: v } = self {
                    t.extend(ts);
                    v.extend(vals);
                }
            }
            ChannelSnapshot::Bool { ts, vals } => {
                if let SnapshotAcc::Empty = self {
                    *self = SnapshotAcc::Bool { ts: vec![], vals: vec![] };
                }
                if let SnapshotAcc::Bool { ts: t, vals: v } = self {
                    t.extend(ts);
                    v.extend(vals);
                }
            }
            ChannelSnapshot::Text { lines } => {
                if let SnapshotAcc::Empty = self {
                    *self = SnapshotAcc::Text { lines: vec![] };
                }
                if let SnapshotAcc::Text { lines: l } = self {
                    l.extend(lines);
                }
            }
        }
    }

    /// Finish, returning a typed-empty snapshot of `fallback` when nothing was
    /// accumulated (so a panel still sees the channel's real variant).
    fn into_snapshot(self, fallback: SampleType) -> ChannelSnapshot {
        match self {
            SnapshotAcc::Float { ts, vals } => ChannelSnapshot::Float { ts, vals },
            SnapshotAcc::Int { ts, vals } => ChannelSnapshot::Int { ts, vals },
            SnapshotAcc::Bool { ts, vals } => ChannelSnapshot::Bool { ts, vals },
            SnapshotAcc::Text { lines } => ChannelSnapshot::Text { lines },
            SnapshotAcc::Empty => match fallback {
                SampleType::Int => ChannelSnapshot::Int { ts: vec![], vals: vec![] },
                SampleType::Bool => ChannelSnapshot::Bool { ts: vec![], vals: vec![] },
                SampleType::Text => ChannelSnapshot::Text { lines: vec![] },
                SampleType::Float => ChannelSnapshot::Float { ts: vec![], vals: vec![] },
            },
        }
    }
}

/// Fold one decoded chunk into the envelope (numeric) and retained text.
fn fold_chunk(chunk: &DecodedChunk, envelope: &mut Envelope, text: &mut [Option<Vec<(i64, String)>>]) {
    for (ch, cs) in chunk.channels.iter().enumerate() {
        match cs {
            ChanSamples::Float { ts, vals } => {
                for (t, v) in ts.iter().zip(vals) {
                    envelope.fold_numeric(ch, *t, *v);
                }
            }
            ChanSamples::Int { ts, vals } => {
                for (t, v) in ts.iter().zip(vals) {
                    envelope.fold_numeric(ch, *t, *v as f64);
                }
            }
            ChanSamples::Bool { ts, vals } => {
                for (t, v) in ts.iter().zip(vals) {
                    envelope.fold_numeric(ch, *t, *v as f64);
                }
            }
            ChanSamples::Text { lines } => {
                if let Some(Some(dst)) = text.get_mut(ch) {
                    dst.extend(lines.iter().cloned());
                }
            }
        }
    }
}

impl PlaybackStore {
    pub fn load(path: &Path, registry: &ChannelRegistry) -> anyhow::Result<Arc<Self>> {
        Self::load_many(std::slice::from_ref(&path), registry)
    }

    /// Install the repaint hook workers call when a decoded chunk lands, so the
    /// UI re-renders and picks up the now-cached detail. Set once after load with
    /// the egui context. Idempotent (replaces any previous hook).
    pub fn set_repaint_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        self.shared.set_waker(Box::new(f));
    }

    /// Load one or more MCAP recordings into a single playback store. Channels
    /// are keyed by name via the shared registry, so same-named channels across
    /// files merge into one. Timestamps are preserved exactly as recorded; the
    /// files sit on one shared timeline at their original times. The start time
    /// of each file after the first is recorded as a break so a line plot does
    /// not draw a segment bridging the gap between two recordings.
    ///
    /// Each file is memory-mapped and indexed by chunk time-bounds; a single
    /// pass folds samples into a decimated min/max envelope (numeric) and full
    /// retention (text), keeping no raw numeric samples. Reads decode the few
    /// chunks overlapping the requested window on demand.
    pub fn load_many(paths: &[&Path], registry: &ChannelRegistry) -> anyhow::Result<Arc<Self>> {
        Self::load_many_with(paths, registry, Caps::default())
    }

    /// [`load_many`](Self::load_many) with explicit memory caps (test hook).
    pub(crate) fn load_many_with(
        paths: &[&Path],
        registry: &ChannelRegistry,
        caps: Caps,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(!paths.is_empty(), "no recording files to load");
        let Caps { cache_bytes, envelope_buckets, chunk_budget, worker_count } = caps;

        // Open (mmap + index + routing) every file first, registering any
        // reconstructed channels BEFORE we snapshot the registry into metas, so
        // metas cover every file's channels.
        let mut sources = Vec::with_capacity(paths.len());
        for path in paths {
            sources.push(RecordingSource::open(path, registry)?);
        }

        // Timeline + breaks from per-file bounds (no decode). A line break sits
        // at the start of every dataset except the earliest in time —
        // order-independent, so it works no matter what order the files were
        // picked in.
        let global_min = sources.iter().filter_map(|s| s.bounds.map(|(mn, _)| mn)).min();
        let global_max = sources.iter().filter_map(|s| s.bounds.map(|(_, mx)| mx)).max();
        let mut mins: Vec<i64> = sources.iter().filter_map(|s| s.bounds.map(|(mn, _)| mn)).collect();
        mins.sort_unstable();
        let breaks: Vec<i64> = mins.into_iter().skip(1).collect();

        let metas: Vec<ChannelMeta> =
            registry.iter_ids().map(|id| registry.meta(id).clone()).collect();
        let (start_ns, duration_ns) = match (global_min, global_max) {
            (Some(mn), Some(mx)) if mn <= mx => (mn, mx - mn),
            _ => (0, 0),
        };

        // One parallel pass builds the envelope + retained text, retaining no
        // raw numeric samples. MCAP chunks are independently compressed, so we
        // split all (source, chunk) jobs across worker threads; each folds into
        // a thread-local envelope + text, then we merge.
        let nchannels = metas.len();
        // Bound the dense grid: foreign recordings flatten into thousands of
        // channels; a full-resolution grid per worker thread would OOM.
        let envelope_buckets = effective_buckets(envelope_buckets, nchannels);
        let mut envelope = Envelope::new(nchannels, start_ns, duration_ns, envelope_buckets);
        let mut text: Vec<Option<Vec<(i64, String)>>> = metas
            .iter()
            .map(|m| (m.sample_type == SampleType::Text).then(Vec::new))
            .collect();

        let jobs: Vec<(usize, usize)> = sources
            .iter()
            .enumerate()
            .flat_map(|(si, s)| (0..s.spans().len()).map(move |k| (si, k)))
            .collect();
        let nthreads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let workers = nthreads.min(jobs.len().max(1));
        type Partial = (Envelope, Vec<Option<Vec<(i64, String)>>>);
        let partials: Vec<Partial> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|w| {
                    let lo = w * jobs.len() / workers;
                    let hi = (w + 1) * jobs.len() / workers;
                    let sources = &sources;
                    let metas = &metas;
                    let jobs = &jobs;
                    scope.spawn(move || -> Partial {
                        let mut env =
                            Envelope::new(nchannels, start_ns, duration_ns, envelope_buckets);
                        let mut txt: Vec<Option<Vec<(i64, String)>>> = metas
                            .iter()
                            .map(|m| (m.sample_type == SampleType::Text).then(Vec::new))
                            .collect();
                        for &(si, k) in &jobs[lo..hi] {
                            let buf = ChunkDecodeBuf::from_metas(metas);
                            let _ = sources[si].decode_into(&sources[si].spans()[k], &buf);
                            let chunk = buf.freeze();
                            fold_chunk(&chunk, &mut env, &mut txt);
                        }
                        (env, txt)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("envelope worker panicked")).collect()
        });
        for (penv, ptxt) in partials {
            envelope.merge(&penv);
            for (ch, slot) in ptxt.into_iter().enumerate() {
                if let (Some(dst), Some(src)) =
                    (text.get_mut(ch).and_then(|o| o.as_mut()), slot)
                {
                    dst.extend(src);
                }
            }
        }
        // Text must end sorted by time (files may interleave).
        for slot in text.iter_mut().flatten() {
            slot.sort_by_key(|(t, _)| *t);
        }

        let shared = Arc::new(crate::record::lazy::DecodeShared::new(
            sources,
            ChunkCache::new(cache_bytes),
            metas,
        ));
        // Retain one Receiver clone so the test hook can drain the queue on the
        // test thread; unused in production (workers own their own Receiver
        // clones).
        let (jobs, rx) = crate::record::lazy::spawn_decode_workers(shared.clone(), worker_count);

        Ok(Arc::new(Self {
            shared,
            jobs,
            rx,
            envelope,
            text,
            position_ns: Arc::new(AtomicI64::new(start_ns)),
            duration_ns,
            start_ns,
            breaks,
            chunk_budget,
        }))
    }

    /// Wide-zoom read: the channel's envelope buckets intersecting the window,
    /// wrapped as the channel's numeric snapshot variant.
    fn snapshot_overview(&self, ch: usize, window: TimeWindow) -> ChannelSnapshot {
        let pts = self.envelope.read(ch, window);
        let ts: Vec<i64> = pts.iter().map(|(t, _)| *t).collect();
        match self.shared.metas.get(ch).map(|m| m.sample_type).unwrap_or(SampleType::Float) {
            SampleType::Int => {
                ChannelSnapshot::Int { ts, vals: pts.iter().map(|(_, v)| *v as i64).collect() }
            }
            SampleType::Bool => ChannelSnapshot::Bool {
                ts,
                vals: pts.iter().map(|(_, v)| (*v != 0.0) as u8).collect(),
            },
            _ => ChannelSnapshot::Float { ts, vals: pts.iter().map(|(_, v)| *v).collect() },
        }
    }

    /// Coarse `latest_at` fallback when the exact chunk isn't cached yet: the
    /// newest envelope bucket point at or before `end_ns`, wrapped as the
    /// channel's typed `Sample`. `None` if the envelope has nothing in range.
    fn envelope_latest(&self, ch: usize, end_ns: i64) -> Option<(i64, Sample)> {
        let pts = self.envelope.read(ch, TimeWindow { start_ns: self.start_ns, end_ns });
        let (t, v) = *pts.last()?;
        let st = self.shared.metas.get(ch).map(|m| m.sample_type).unwrap_or(SampleType::Float);
        Some((
            t,
            match st {
                SampleType::Int => Sample::Int(v as i64),
                SampleType::Bool => Sample::Bool(v != 0.0),
                _ => Sample::Float(v),
            },
        ))
    }

    /// Enqueue a missing chunk for background decode, deduped. A full queue
    /// (fast scrub outpacing decode) drops the request; the still-visible window
    /// re-requests it next frame.
    fn request(&self, key: crate::record::lazy::Job) {
        if self.shared.reserve(key) {
            if self.jobs.try_send(key).is_err() {
                self.shared.unreserve(key);
            }
        }
    }

    /// Bytes currently retained by the decoded-chunk cache (test hook).
    #[cfg(test)]
    pub(crate) fn cache_retained_bytes(&self) -> usize {
        self.shared.cache.retained_bytes()
    }

    /// Drain any queued decodes on THIS thread (used by zero-worker tests).
    #[cfg(test)]
    pub(crate) fn run_pending_decodes(&self) {
        while let Ok(key) = self.rx.try_recv() {
            self.shared.decode_and_cache(key);
        }
    }

    /// Block until every enqueued detail decode has completed, for any worker
    /// count: drain the queue on this thread (covers worker_count == 0), then
    /// wait out in-flight worker decodes. Terminates — the job set is finite and
    /// workers make progress.
    #[cfg(test)]
    pub(crate) fn settle_decodes(&self) {
        while let Ok(key) = self.rx.try_recv() {
            self.shared.decode_and_cache(key);
        }
        while !self.shared.in_flight_is_empty() {
            std::thread::yield_now();
        }
    }

    /// Snapshot with all detail decoded: prime the async request, settle, re-read.
    /// A single snapshot enqueues at most `QUEUE_CAP` missing chunks, so a window
    /// overlapping more chunks than the queue holds needs several rounds: prime,
    /// settle, repeat until a probe enqueues nothing more. Each round caches at
    /// least one new chunk, so it terminates.
    #[cfg(test)]
    pub(crate) fn snapshot_blocking(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot {
        loop {
            let _ = self.snapshot(channel, window);
            let pending = !self.shared.in_flight_is_empty() || !self.rx.is_empty();
            self.settle_decodes();
            if !pending {
                break;
            }
        }
        self.snapshot(channel, window)
    }

    /// Test hook: latest_at with all needed detail decoded (prime → settle → re-read).
    #[cfg(test)]
    pub(crate) fn latest_at_blocking(&self, channel: ChannelId, end_ns: i64) -> Option<(i64, Sample)> {
        loop {
            let _ = self.latest_at(channel, end_ns);
            let pending = !self.shared.in_flight_is_empty() || !self.rx.is_empty();
            self.settle_decodes();
            if !pending {
                break;
            }
        }
        self.latest_at(channel, end_ns)
    }

    /// Test hook: `latest` at the current position, fully decoded.
    #[cfg(test)]
    pub(crate) fn latest_blocking(&self, channel: ChannelId) -> Option<(i64, Sample)> {
        self.latest_at_blocking(channel, self.position_ns.load(std::sync::atomic::Ordering::Relaxed))
    }
}

impl ChannelStore for PlaybackStore {
    // Playback is read-only: samples come from the mmapped files, not writes.
    fn write_numeric(&self, _channel: ChannelId, _ts: i64, _val: NumericVal) {}
    fn write_text(&self, _channel: ChannelId, _ts: i64, _line: String) {}

    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot {
        let ch = channel.0 as usize;
        // Text: retained full, exact slice at all zooms.
        if let Some(Some(lines)) = self.text.get(ch) {
            let s = lines.partition_point(|(t, _)| *t < window.start_ns);
            let e = lines.partition_point(|(t, _)| *t < window.end_ns);
            return ChannelSnapshot::Text { lines: lines[s..e].to_vec() };
        }
        // Overview when the window truly overlaps too many chunks to decode
        // exactly. This decision uses the UNPADDED overlap count: the batched-file
        // start pad in `overlapping` only widens the detail decode set below, and
        // must not inflate the count and force a finely-chunked unbatched file
        // onto the decimated overview (a fidelity regression at some zooms).
        let true_overlaps: usize =
            self.shared.sources.iter().map(|src| src.overlapping_true_count(window)).sum();
        if true_overlaps > self.chunk_budget {
            return self.snapshot_overview(ch, window);
        }
        // Detail: gather the padded overlapping chunks across sources, sorted by
        // start time. The pad adds edge chunks so a batched file's coarsened
        // chunk-start bound cannot hide in-window samples; per-sample clipping in
        // `chunk.window` keeps the result exact.
        let mut overlaps: Vec<(i64, usize, usize)> = Vec::new(); // (start_ns, source, span_idx)
        for (si, src) in self.shared.sources.iter().enumerate() {
            for span_idx in src.overlapping(window) {
                overlaps.push((src.spans()[span_idx].start_ns, si, span_idx));
            }
        }
        overlaps.sort_by_key(|(t, _, _)| *t);
        // Probe the cache for every needed chunk; never decode on the read
        // thread. If any is missing, enqueue the misses for background decode and
        // serve the coarse overview now — a worker fills the cache and requests a
        // repaint, and the next frame lands in the all-cached branch below.
        let mut chunks: Vec<Arc<DecodedChunk>> = Vec::new();
        let mut missing = false;
        for (_, si, span_idx) in overlaps {
            // Only the topic that owns `ch` matters; sources that don't record it
            // contribute nothing.
            let Some(topic_key) = self.shared.sources[si].topic_key_of(ch) else { continue };
            let key = (si, span_idx, topic_key);
            match self.shared.cached(key) {
                Some(c) => chunks.push(c),
                None => {
                    missing = true;
                    self.request(key); // enqueue every missing chunk (do not early-return)
                }
            }
        }
        if missing {
            return self.snapshot_overview(ch, window);
        }
        let mut acc = SnapshotAcc::new();
        for c in chunks {
            acc.extend(c.window(ch, window));
        }
        acc.into_snapshot(self.shared.metas.get(ch).map(|m| m.sample_type).unwrap_or(SampleType::Float))
    }

    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)> {
        self.latest_at(channel, self.position_ns.load(Ordering::Relaxed))
    }

    fn latest_at(&self, channel: ChannelId, end_ns: i64) -> Option<(i64, Sample)> {
        let ch = channel.0 as usize;
        if let Some(Some(lines)) = self.text.get(ch) {
            let idx = lines.partition_point(|(t, _)| *t <= end_ns);
            return (idx > 0).then(|| (lines[idx - 1].0, Sample::Text(lines[idx - 1].1.clone())));
        }
        // Numeric: scan candidate spans (start_ns <= end_ns) from latest to
        // earliest, decoding via the cache, and return the first chunk that
        // holds a sample <= end_ns. Only the containing chunk is decoded.
        let mut candidates: Vec<(usize, usize, i64)> = Vec::new(); // (source, span_idx, start_ns)
        for (si, src) in self.shared.sources.iter().enumerate() {
            for (k, span) in src.spans().iter().enumerate() {
                // Pad the start bound: a batched file's chunk-index start can sit
                // up to SPAN_PAD_NS after the chunk's true earliest sample, so a
                // chunk holding a sample <= end_ns can have start_ns > end_ns.
                // `last_le` re-checks actual samples, so extra candidates are safe.
                if span.start_ns.saturating_sub(crate::record::lazy::source::SPAN_PAD_NS) <= end_ns {
                    candidates.push((si, k, span.start_ns));
                }
            }
        }
        candidates.sort_by_key(|(_, _, s)| *s);
        for (si, k, _) in candidates.into_iter().rev() {
            let Some(topic_key) = self.shared.sources[si].topic_key_of(ch) else { continue };
            let key = (si, k, topic_key);
            match self.shared.cached(key) {
                Some(chunk) => {
                    if let Some(hit) = chunk.last_le(ch, end_ns) {
                        return Some(hit);
                    }
                }
                None => self.request(key),
            }
        }
        // Nothing cached yields a hit yet: coarse fallback from the envelope, or None.
        self.envelope_latest(ch, end_ns)
    }

    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta {
        &self.shared.metas[channel.0 as usize]
    }

    fn now_ns(&self) -> i64 {
        self.position_ns.load(Ordering::Relaxed)
    }

    fn break_times(&self) -> &[i64] {
        &self.breaks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::loader::ProtoSchema;
    use crate::types::{ChannelSnapshot, TimeWindow};
    use std::io::Write;
    use std::sync::atomic::Ordering;

    #[test]
    fn effective_buckets_bounds_dense_grid() {
        // Few channels: full requested resolution, grid well under budget.
        assert_eq!(effective_buckets(ENVELOPE_BUCKETS, 30), ENVELOPE_BUCKETS);
        // Many channels: buckets scaled down so nchannels*buckets <= budget.
        let b = effective_buckets(ENVELOPE_BUCKETS, 11467);
        assert!(b < ENVELOPE_BUCKETS, "should reduce from {ENVELOPE_BUCKETS}, got {b}");
        assert!(11467 * b <= ENVELOPE_CELL_BUDGET, "grid {} over budget", 11467 * b);
        assert!(b >= ENVELOPE_MIN_BUCKETS);
        // Pathological channel count: never collapses below the floor.
        assert_eq!(effective_buckets(ENVELOPE_BUCKETS, usize::MAX), ENVELOPE_MIN_BUCKETS);
        // Zero channels: requested value, never zero.
        assert_eq!(effective_buckets(ENVELOPE_BUCKETS, 0), ENVELOPE_BUCKETS);
    }

    fn make_proto_and_registry() -> (ProtoSchema, tempfile::TempDir, ChannelRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message AccelBatch {{
  repeated Sample samples = 1;
  message Sample {{ int64 t_ns = 1; float x = 2; }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
        (schema, dir, registry)
    }

    fn write_test_mcap(
        path: &std::path::Path,
        schema: &ProtoSchema,
        messages: &[(i64, f32)],  // (t_ns, x)
    ) {
        use prost_reflect::prost::Message as _;
        use prost_reflect::{DynamicMessage, Value};
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("AccelBatch").unwrap();
        let sample_desc = pool.get_message_by_name("AccelBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let x_field = sample_desc.get_field_by_name("x").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();

        let mcap_schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(schema.schema_bytes().to_vec()),
        });
        let channel = Arc::new(mcap::Channel {
            topic: "accel".to_string(),
            schema: Some(mcap_schema),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        for (t_ns, x) in messages {
            let list = vec![{
                let mut s = DynamicMessage::new(sample_desc.clone());
                s.set_field(&t_field, Value::I64(*t_ns));
                s.set_field(&x_field, Value::F32(*x));
                Value::Message(s)
            }];
            let mut batch = DynamicMessage::new(batch_desc.clone());
            batch.set_field(&samples_field, Value::List(list));
            let data = batch.encode_to_vec();
            writer.write(&mcap::Message {
                channel: channel.clone(),
                sequence: 0,
                log_time: *t_ns as u64,
                publish_time: *t_ns as u64,
                data: Cow::Owned(data),
            }).unwrap();
        }
        writer.finish().unwrap();
    }

    /// Like `write_test_mcap` but forces a small target chunk size, so a modest
    /// message count still produces many independently-compressed chunks.
    fn write_test_mcap_chunked(
        path: &std::path::Path,
        schema: &ProtoSchema,
        messages: &[(i64, f32)],
        chunk_size: u64,
    ) {
        use prost_reflect::prost::Message as _;
        use prost_reflect::{DynamicMessage, Value};
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("AccelBatch").unwrap();
        let sample_desc = pool.get_message_by_name("AccelBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let x_field = sample_desc.get_field_by_name("x").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();

        let channel = Arc::new(mcap::Channel {
            topic: "accel".to_string(),
            schema: Some(Arc::new(mcap::Schema {
                name: "protobuf".to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(schema.schema_bytes().to_vec()),
            })),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = mcap::WriteOptions::new()
            .chunk_size(Some(chunk_size))
            .create(file)
            .unwrap();
        for (t_ns, x) in messages {
            let list = vec![{
                let mut s = DynamicMessage::new(sample_desc.clone());
                s.set_field(&t_field, Value::I64(*t_ns));
                s.set_field(&x_field, Value::F32(*x));
                Value::Message(s)
            }];
            let mut batch = DynamicMessage::new(batch_desc.clone());
            batch.set_field(&samples_field, Value::List(list));
            let data = batch.encode_to_vec();
            writer
                .write(&mcap::Message {
                    channel: channel.clone(),
                    sequence: 0,
                    log_time: *t_ns as u64,
                    publish_time: *t_ns as u64,
                    data: Cow::Owned(data),
                })
                .unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn cache_stays_within_cap_while_scrubbing() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scrub.mcap");
        // Many small chunks so scrubbing touches many distinct cache keys.
        let msgs: Vec<(i64, f32)> = (0..8000).map(|i| (i as i64 + 1, i as f32)).collect();
        write_test_mcap_chunked(&path, &schema, &msgs, 2048);
        let caps =
            Caps { cache_bytes: 64 * 1024, envelope_buckets: 1024, chunk_budget: 4, worker_count: 0 };
        let store = PlaybackStore::load_many_with(&[&path], &registry, caps).unwrap();
        let id = registry.id("accel.x").unwrap();
        for start in (0..8000i64).step_by(200) {
            let w = TimeWindow { start_ns: start, end_ns: start + 100 };
            let _ = store.snapshot(id, w);
            assert!(
                store.cache_retained_bytes() <= 64 * 1024,
                "cache exceeded cap: {}",
                store.cache_retained_bytes()
            );
        }
    }

    #[test]
    fn latest_at_decodes_correct_chunk_across_span_boundary() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.mcap");
        // Small chunk size → many chunks, so 3500 lands in a non-last chunk and
        // the targeted decode must select the right span.
        let msgs: Vec<(i64, f32)> = (0..4000).map(|i| (i as i64 + 1, i as f32)).collect();
        write_test_mcap_chunked(&path, &schema, &msgs, 4096);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("accel.x").unwrap();
        let (t, s) = store.latest_at_blocking(id, 3500).unwrap();
        assert_eq!(t, 3500);
        match s {
            Sample::Float(v) => assert!((v - 3499.0).abs() < 1e-6),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn latest_at_is_nonblocking_and_sharpens_after_decode() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latest_async.mcap");
        let msgs: Vec<(i64, f32)> = (0..4000).map(|i| (i as i64 + 1, i as f32)).collect();
        write_test_mcap_chunked(&path, &schema, &msgs, 4096);
        let caps = Caps {
            cache_bytes: 64 * 1024 * 1024,
            envelope_buckets: 256,
            chunk_budget: 16,
            worker_count: 0,
        };
        let store = PlaybackStore::load_many_with(&[&path], &registry, caps).unwrap();
        let id = registry.id("accel.x").unwrap();

        // Cold cache: must return without decoding (either None or an envelope-coarse
        // value), never blocking.
        let _cold = store.latest_at(id, 3500);
        // After draining, the exact containing chunk is decoded → precise sample.
        store.run_pending_decodes();
        let (t, s) = store.latest_at(id, 3500).expect("exact after decode");
        assert_eq!(t, 3500);
        match s {
            Sample::Float(v) => assert!((v - 3499.0).abs() < 1e-6),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn narrow_window_on_finely_chunked_file_returns_exact_detail() {
        // Regression: the SPAN_PAD in `overlapping` must not inflate the
        // overview/budget decision. On a finely-chunked UNBATCHED file the pad
        // pulls in every chunk (all starts sit within 500 ms of each other), so
        // the padded overlap count blows past chunk_budget even for a tiny
        // window. The budget decision must use the TRUE (unpadded) overlap
        // count, so this narrow read still returns exact per-sample detail
        // rather than the decimated envelope overview.
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fine.mcap");
        // 8000 samples 1 ns apart, tiny chunks → far more than chunk_budget
        // chunks, all within an 8 us total span (<< 500 ms pad).
        let msgs: Vec<(i64, f32)> = (0..8000).map(|i| (i as i64 + 1, i as f32)).collect();
        write_test_mcap_chunked(&path, &schema, &msgs, 4096);
        let caps = Caps {
            cache_bytes: 64 * 1024 * 1024,
            envelope_buckets: 64,
            chunk_budget: 16,
            worker_count: 0,
        };
        let store = PlaybackStore::load_many_with(&[&path], &registry, caps).unwrap();
        let id = registry.id("accel.x").unwrap();
        // Window [1, 30) truly overlaps only the first chunk (well under budget),
        // but the pad makes every chunk "overlap". Exact detail → ts 1..=29.
        let snap = store.snapshot_blocking(id, TimeWindow { start_ns: 1, end_ns: 30 });
        match snap {
            ChannelSnapshot::Float { ts, .. } => {
                assert_eq!(ts, (1..30).collect::<Vec<i64>>(), "must return exact samples, not overview");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn detail_snapshot_is_coarse_until_workers_decode_then_sharp() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("async.mcap");
        // Many samples, small chunks; a narrow window truly overlaps <= budget so
        // the detail path is taken (not the overview budget path).
        let msgs: Vec<(i64, f32)> = (0..4000).map(|i| (i as i64 + 1, i as f32)).collect();
        write_test_mcap_chunked(&path, &schema, &msgs, 4096);
        // Zero workers → the cache only fills when the test drains the queue.
        let caps = Caps { cache_bytes: 64 * 1024 * 1024, envelope_buckets: 256,
                          chunk_budget: 16, worker_count: 0 };
        let store = PlaybackStore::load_many_with(&[&path], &registry, caps).unwrap();
        let id = registry.id("accel.x").unwrap();
        let window = TimeWindow { start_ns: 1, end_ns: 30 };

        // First read: chunks not yet decoded → coarse overview (decimated), not the
        // exact 1..30 detail.
        let first = store.snapshot(id, window);
        let coarse_len = match &first {
            ChannelSnapshot::Float { ts, .. } => ts.len(),
            other => panic!("wrong variant: {other:?}"),
        };

        // Drain the queued decode jobs, then re-read: now exact detail.
        store.run_pending_decodes();
        let sharp = store.snapshot(id, window);
        match sharp {
            ChannelSnapshot::Float { ts, .. } => {
                assert_eq!(ts, (1..30).collect::<Vec<i64>>(), "must be exact detail after decode");
                assert!(ts.len() >= coarse_len, "detail should be at least as dense as coarse");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn wide_window_uses_envelope_overview() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.mcap");
        // 40k samples > ENVELOPE_BUCKETS (16384) so the overview is decimated;
        // a 4 KiB target chunk size forces far more than CHUNK_BUDGET chunks so
        // a whole-file window takes the overview path.
        let msgs: Vec<(i64, f32)> =
            (0..40_000).map(|i| (i as i64 * 1_000_000 + 1, (i % 7) as f32)).collect();
        write_test_mcap_chunked(&path, &schema, &msgs, 4096);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("accel.x").unwrap();
        let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
        let snap = store.snapshot(id, all);
        match snap {
            ChannelSnapshot::Float { ts, .. } => {
                assert!(!ts.is_empty());
                assert!(ts.len() < 40_000, "overview must be decimated, got {}", ts.len());
                assert!(ts.windows(2).all(|w| w[0] <= w[1]), "overview must be time-ordered");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn snapshot_acc_concatenates_same_variant() {
        let mut acc = SnapshotAcc::new();
        acc.extend(ChannelSnapshot::Float { ts: vec![1, 2], vals: vec![1.0, 2.0] });
        acc.extend(ChannelSnapshot::Float { ts: vec![3], vals: vec![3.0] });
        match acc.into_snapshot(SampleType::Float) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1, 2, 3]);
                assert_eq!(vals, vec![1.0, 2.0, 3.0]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn detail_snapshot_matches_exact_samples() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mcap");
        write_test_mcap(
            &path,
            &schema,
            &[(1_000_000_000, 1.0), (2_000_000_000, 2.0), (3_000_000_000, 3.0)],
        );
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("accel.x").unwrap();
        // Narrow window (few chunks) → exact raw samples.
        let snap = store.snapshot(id, TimeWindow { start_ns: 1_000_000_000, end_ns: 3_000_000_000 });
        match snap {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                assert!((vals[0] - 1.0).abs() < 1e-9 && (vals[1] - 2.0).abs() < 1e-9);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn load_and_snapshot_returns_data_in_window() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (2_000_000_000, 2.0),
            (3_000_000_000, 3.0),
        ]);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("accel.x").unwrap();
        let window = TimeWindow { start_ns: 1_000_000_000, end_ns: 3_000_000_000 };
        let snap = store.snapshot(id, window);
        match snap {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts.len(), 2);
                assert_eq!(ts[0], 1_000_000_000);
                assert_eq!(ts[1], 2_000_000_000);
                // EU scale 1.0, offset 0.0 (defaults) → values unchanged
                assert!((vals[0] - 1.0_f64).abs() < 1e-4);
                assert!((vals[1] - 2.0_f64).abs() < 1e-4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn now_ns_returns_position_not_wall_clock() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[(1_000_000_000, 1.0)]);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let expected_pos = 42_999_999_999i64;
        store.position_ns.store(expected_pos, Ordering::Relaxed);
        assert_eq!(store.now_ns(), expected_pos);
    }

    #[test]
    fn latest_returns_sample_at_or_before_position() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (3_000_000_000, 3.0),
        ]);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("accel.x").unwrap();

        // Position before any sample → None
        store.position_ns.store(0, Ordering::Relaxed);
        assert!(store.latest_blocking(id).is_none());

        // Position at first sample → returns first
        store.position_ns.store(1_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest_blocking(id).unwrap();
        assert_eq!(ts, 1_000_000_000);

        // Position between samples → returns first
        store.position_ns.store(2_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest_blocking(id).unwrap();
        assert_eq!(ts, 1_000_000_000);

        // Position at second sample → returns second
        store.position_ns.store(3_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest_blocking(id).unwrap();
        assert_eq!(ts, 3_000_000_000);
    }

    #[test]
    fn duration_and_start_ns_computed_from_data() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (4_000_000_000, 4.0),
        ]);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        assert_eq!(store.start_ns, 1_000_000_000);
        assert_eq!(store.duration_ns, 3_000_000_000);
    }

    #[test]
    fn load_many_preserves_timestamps_merges_channels_and_records_break() {
        use crate::store::ChannelStore;
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.mcap");
        let path_b = dir.path().join("b.mcap");
        // Two files recording the SAME channel ("accel") at distinct times.
        write_test_mcap(&path_a, &schema, &[(1_000_000_000, 1.0), (2_000_000_000, 2.0)]);
        write_test_mcap(&path_b, &schema, &[(5_000_000_000, 3.0), (6_000_000_000, 4.0)]);

        let store = PlaybackStore::load_many(&[&path_a, &path_b], &registry).unwrap();
        let id = registry.id("accel.x").unwrap();

        // Same-named channel across files → one merged channel, ORIGINAL times.
        let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
        match store.snapshot(id, all) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(
                    ts,
                    vec![1_000_000_000, 2_000_000_000, 5_000_000_000, 6_000_000_000]
                );
                assert_eq!(vals.len(), 4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // Timeline spans both files at their real times.
        assert_eq!(store.start_ns, 1_000_000_000);
        assert_eq!(store.duration_ns, 5_000_000_000);
        // The second file's start is a line break so the join is not connected.
        assert_eq!(store.break_times(), &[5_000_000_000]);
    }

    #[test]
    fn load_many_break_is_independent_of_file_order() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let early = dir.path().join("early.mcap");
        let late = dir.path().join("late.mcap");
        write_test_mcap(&early, &schema, &[(1_000_000_000, 1.0), (2_000_000_000, 2.0)]);
        write_test_mcap(&late, &schema, &[(5_000_000_000, 3.0), (6_000_000_000, 4.0)]);

        // Load with the LATER file first — the break must still fall at the
        // later dataset's start, not the first-loaded file's.
        let store = PlaybackStore::load_many(&[&late, &early], &registry).unwrap();
        assert_eq!(store.break_times(), &[5_000_000_000]);
    }

    /// Write a single Batch-shaped MCAP message for one MQTT topic using the
    /// recorder's own schema generator, so the embedded schema matches
    /// production exactly. All `(ts, payload)` samples are wrapped as repeated
    /// `Sample` elements (tag 0x0A, varint length, bytes) in one message whose
    /// `log_time` is the last sample's ts — the reader can only decode a Batch
    /// schema's payload as a repeated field, not a bare `Sample`.
    fn write_mqtt_mcap(path: &std::path::Path, topic: &str, payloads: &[(i64, &str)]) {
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let mut reg = DynamicProtoRegistry::new();
        let mut schema_bytes: Option<Arc<[u8]>> = None;
        let mut batch: Vec<u8> = Vec::new();
        let mut last_ts = 0i64;
        for (ts, payload) in payloads {
            let (schema, sample_bytes) = reg.record_frame(topic, *ts, payload).unwrap();
            schema_bytes.get_or_insert(schema);
            // repeated Sample samples = 1  →  tag 0x0A, len, bytes
            batch.push(0x0A);
            let mut len = sample_bytes.len();
            loop {
                let mut b = (len & 0x7f) as u8;
                len >>= 7;
                if len != 0 { b |= 0x80; }
                batch.push(b);
                if len == 0 { break; }
            }
            batch.extend_from_slice(&sample_bytes);
            last_ts = *ts;
        }
        let channel = Arc::new(mcap::Channel {
            topic: topic.to_string(),
            schema: Some(Arc::new(mcap::Schema {
                name: topic.to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(schema_bytes.unwrap().to_vec()),
            })),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        writer.write(&mcap::Message {
            channel, sequence: 0, log_time: last_ts as u64, publish_time: last_ts as u64,
            data: Cow::Owned(batch),
        }).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn mqtt_topic_reconstructed_from_embedded_schema() {
        // Registry does NOT contain the MQTT topic.
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mqtt.mcap");
        write_mqtt_mcap(&path, "home/temp", &[(1_000_000_000, "21.5"), (2_000_000_000, "22.0")]);

        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("home/temp").expect("reconstructed channel registered");
        assert_eq!(registry.meta(id).sample_type, SampleType::Float);

        let window = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
        match store.snapshot(id, window) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                assert!((vals[0] - 21.5).abs() < 1e-4);
                assert!((vals[1] - 22.0).abs() < 1e-4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn mixed_zmq_and_mqtt_topics_both_decode() {
        use prost_reflect::prost::Message as _;
        use prost_reflect::{DynamicMessage, Value};
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let (schema, _dir, registry) = make_proto_and_registry(); // registers "accel.x" on topic "accel"
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("mixed.mcap");

        // ZMQ channel (embedded AccelBatch schema).
        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("AccelBatch").unwrap();
        let sample_desc = pool.get_message_by_name("AccelBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let x_field = sample_desc.get_field_by_name("x").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();
        let accel_channel = Arc::new(mcap::Channel {
            topic: "accel".to_string(),
            schema: Some(Arc::new(mcap::Schema {
                name: "protobuf".to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(schema.schema_bytes().to_vec()),
            })),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });

        // MQTT channel (generated schema).
        let mut reg = DynamicProtoRegistry::new();
        let (mqtt_schema_bytes, mqtt_data) = reg.record_frame("home/temp", 1_500_000_000, "30.0").unwrap();
        let mqtt_channel = Arc::new(mcap::Channel {
            topic: "home/temp".to_string(),
            schema: Some(Arc::new(mcap::Schema {
                name: "home/temp".to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(mqtt_schema_bytes.to_vec()),
            })),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });

        let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        // one accel batch sample
        let list = vec![{
            let mut s = DynamicMessage::new(sample_desc.clone());
            s.set_field(&t_field, Value::I64(1_000_000_000));
            s.set_field(&x_field, Value::F32(1.0));
            Value::Message(s)
        }];
        let mut batch = DynamicMessage::new(batch_desc.clone());
        batch.set_field(&samples_field, Value::List(list));
        writer.write(&mcap::Message {
            channel: accel_channel, sequence: 0,
            log_time: 1_000_000_000, publish_time: 1_000_000_000,
            data: Cow::Owned(batch.encode_to_vec()),
        }).unwrap();
        // Wrap the bare Sample into a one-element Batch (tag 0x0A, len, bytes) —
        // the embedded schema is Batch-shaped, so the payload must be too.
        let mut mqtt_batch = vec![0x0A];
        let mut len = mqtt_data.len();
        loop {
            let mut b = (len & 0x7f) as u8;
            len >>= 7;
            if len != 0 { b |= 0x80; }
            mqtt_batch.push(b);
            if len == 0 { break; }
        }
        mqtt_batch.extend_from_slice(&mqtt_data);
        writer.write(&mcap::Message {
            channel: mqtt_channel, sequence: 0,
            log_time: 1_500_000_000, publish_time: 1_500_000_000,
            data: Cow::Owned(mqtt_batch),
        }).unwrap();
        writer.finish().unwrap();

        let store = PlaybackStore::load(&path, &registry).unwrap();
        let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

        let accel_id = registry.id("accel.x").unwrap();
        match store.snapshot(accel_id, all) {
            ChannelSnapshot::Float { ts, .. } => assert_eq!(ts, vec![1_000_000_000]),
            other => panic!("accel wrong variant: {other:?}"),
        }
        let mqtt_id = registry.id("home/temp").unwrap();
        match store.snapshot(mqtt_id, all) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_500_000_000]);
                assert!((vals[0] - 30.0).abs() < 1e-4);
            }
            other => panic!("mqtt wrong variant: {other:?}"),
        }
    }

    #[test]
    fn value_sample_type_maps_kinds() {
        use crate::record::lazy::source::{first_message_name, value_sample_type};
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        use prost_reflect::{Cardinality, Kind};
        let mut reg = DynamicProtoRegistry::new();
        // Each first payload locks a distinct type; grab the generated descriptor
        // back out via the merged pool. The top-level message is now the Batch
        // (repeated Sample, no `value` field of its own), so navigate to the
        // nested Sample descriptor before checking value_sample_type.
        for (topic, payload, expected) in [
            ("t/f", "3.14", SampleType::Float),
            ("t/i", "7", SampleType::Int),
            ("t/b", "true", SampleType::Bool),
            ("t/s", "hello", SampleType::Text),
        ] {
            let (schema_bytes, _data) = reg.record_frame(topic, 1, payload).unwrap();
            let full = first_message_name(&schema_bytes).unwrap();
            let merged = ProtoSchema::from_descriptor_sets(&[&schema_bytes]);
            let batch_desc = merged.message_by_name(&full).unwrap();
            let sample_desc = batch_desc
                .fields()
                .find_map(|f| match (f.cardinality(), f.kind()) {
                    (Cardinality::Repeated, Kind::Message(m)) => Some(m),
                    _ => None,
                })
                .expect("batch message has a repeated Sample field");
            assert_eq!(value_sample_type(&sample_desc), Some(expected), "topic {topic}");
        }
    }

    #[test]
    fn reopen_with_conflicting_type_skips_not_corrupts() {
        // Registry with an unrelated ZMQ channel only — no "home/temp" pre-registered.
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();

        // File A: "home/temp" recorded as Float.
        let dir_a = tempfile::tempdir().unwrap();
        let path_a = dir_a.path().join("file_a.mcap");
        write_mqtt_mcap(&path_a, "home/temp", &[(1_000_000_000, "21.5")]);

        let _store_a = PlaybackStore::load(&path_a, &registry).unwrap();
        let id = registry.id("home/temp").expect("channel registered from file A");
        assert_eq!(
            registry.meta(id).sample_type,
            SampleType::Float,
            "file A should register home/temp as Float"
        );

        // File B: same topic "home/temp" but written with a Bool payload.
        // write_mqtt_mcap uses a fresh DynamicProtoRegistry per call, so the
        // embedded schema will type the value field as Bool.
        let dir_b = tempfile::tempdir().unwrap();
        let path_b = dir_b.path().join("file_b.mcap");
        write_mqtt_mcap(&path_b, "home/temp", &[(2_000_000_000, "true")]);

        // Must not panic, must not corrupt the existing Float registration.
        let store_b = PlaybackStore::load(&path_b, &registry).unwrap();

        // The channel type must remain Float — add_dynamic is idempotent by name.
        assert_eq!(
            registry.meta(id).sample_type,
            SampleType::Float,
            "reopening with conflicting type must not mutate the existing channel meta"
        );

        // The conflicting Bool samples must have been silently skipped: the
        // Float snapshot over the full window should be empty.
        let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
        match store_b.snapshot(id, all) {
            ChannelSnapshot::Float { ts, vals } => {
                assert!(
                    ts.is_empty() && vals.is_empty(),
                    "conflicting Bool samples must not appear in the Float channel"
                );
            }
            other => panic!("expected Float snapshot, got {other:?}"),
        }
    }

    /// Old (pre-batch) recordings embedded a flat schema: a top-level message
    /// with `t_ns`/`value` and no repeated field. Proves `plan_file`'s flat
    /// branch still fires and the file decodes, guarding full backward
    /// compatibility after the Task 3 batch-schema change.
    #[test]
    fn old_flat_file_still_reads() {
        use prost_reflect::prost::Message as _;
        use prost_reflect::{DescriptorPool, DynamicMessage, Value};
        use prost_types::field_descriptor_proto::{Label, Type};
        use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet};
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let field = |name: &str, number: i32, ty: Type| FieldDescriptorProto {
            name: Some(name.to_string()),
            number: Some(number),
            label: Some(Label::Optional as i32),
            r#type: Some(ty as i32),
            ..Default::default()
        };
        let file = FileDescriptorProto {
            name: Some("mqtt/HomeTemp.proto".to_string()),
            package: Some("mqtt".to_string()),
            syntax: Some("proto3".to_string()),
            message_type: vec![DescriptorProto {
                name: Some("HomeTemp".to_string()),
                field: vec![field("t_ns", 1, Type::Int64), field("value", 2, Type::Double)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let schema_bytes = FileDescriptorSet { file: vec![file.clone()] }.encode_to_vec();

        let mut pool = DescriptorPool::new();
        pool.add_file_descriptor_proto(file).unwrap();
        let desc = pool.get_message_by_name("mqtt.HomeTemp").unwrap();
        let mut msg = DynamicMessage::new(desc);
        msg.set_field_by_name("t_ns", Value::I64(1_000_000_000));
        msg.set_field_by_name("value", Value::F64(21.5));
        let data = msg.encode_to_vec();

        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flat.mcap");
        let channel = Arc::new(mcap::Channel {
            topic: "home/temp".to_string(),
            schema: Some(Arc::new(mcap::Schema {
                name: "home/temp".to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(schema_bytes),
            })),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });
        let f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let mut writer = mcap::Writer::new(f).unwrap();
        writer.write(&mcap::Message {
            channel,
            sequence: 0,
            log_time: 1_000_000_000,
            publish_time: 1_000_000_000,
            data: Cow::Owned(data),
        }).unwrap();
        writer.finish().unwrap();

        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("home/temp").expect("flat topic reconstructed");
        assert_eq!(registry.meta(id).sample_type, SampleType::Float);
        let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
        match store.snapshot(id, all) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000]);
                assert!((vals[0] - 21.5).abs() < 1e-9);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
