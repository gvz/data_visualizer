//! Profile MCAP read (load + scrub) against a real recording.
//!
//! Run: cargo run --release --example mcap_profile -- <file.mcap> [config.toml]
//!
//! Times the two phases a user actually waits on:
//!   1. PlaybackStore::load_many — mmap, plan, and the parallel envelope pass.
//!   2. a scrubbing pass — detail snapshots stepped across the whole timeline,
//!      the per-frame read the panels issue while dragging the playhead.
//! Also reports chunk counts, so a quadratic-in-chunks cost is visible.

use std::path::{Path, PathBuf};
use std::time::Instant;

use datavis::config::ChannelRegistry;
use datavis::record::playback::PlaybackStore;
use datavis::store::ChannelStore;
use datavis::types::TimeWindow;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let file = args.next().unwrap_or_else(|| "sample_recording.mcap".to_string());
    let cfg = args.next().unwrap_or_else(|| "config.toml".to_string());

    let registry = ChannelRegistry::load(Path::new(&cfg))?;
    let path = PathBuf::from(&file);
    let meta_len = std::fs::metadata(&path)?.len();

    // Phase 1: load.
    let t0 = Instant::now();
    let store = PlaybackStore::load(&path, &registry)?;
    let load = t0.elapsed();

    let ids: Vec<_> = registry.iter_ids().collect();
    let span = store.duration_ns.max(1);
    println!(
        "file {}  ({:.1} MiB)\nchannels {}  duration {:.1}s  load {:.3}s ({:.1} MiB/s)",
        file,
        meta_len as f64 / (1024.0 * 1024.0),
        ids.len(),
        span as f64 / 1e9,
        load.as_secs_f64(),
        meta_len as f64 / (1024.0 * 1024.0) / load.as_secs_f64(),
    );

    // Phase 2: scrub. Step a narrow detail window across the timeline for the
    // first handful of channels, as the panels would while dragging.
    let steps = 400i64;
    let win = span / 200; // ~0.5% of the timeline per read
    let probe: Vec<_> = ids.iter().take(8).copied().collect();
    let t1 = Instant::now();
    let mut samples = 0usize;
    for k in 0..steps {
        let start = store.start_ns + span * k / steps;
        let w = TimeWindow { start_ns: start, end_ns: start + win };
        for &id in &probe {
            samples += store.snapshot(id, w).len();
        }
    }
    let scrub = t1.elapsed();
    println!(
        "scrub {} reads x {} ch  {:.3}s  ({:.2} ms/read, {} samples)",
        steps,
        probe.len(),
        scrub.as_secs_f64(),
        scrub.as_secs_f64() * 1e3 / steps as f64,
        samples,
    );

    Ok(())
}
