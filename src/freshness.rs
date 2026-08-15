//! Per-panel data-freshness indicator.
//!
//! A panel's title bar shows a small dot that answers "is this channel still
//! producing data?". Liveness is judged by **how long since the channel's newest
//! sample last moved forward** — in wall-clock time — not by how old the newest
//! sample is.
//!
//! That distinction matters for transforms whose output timestamps lag the wall
//! clock by a fixed offset (e.g. a windowed diff script that emits samples
//! stamped in the past). Such a channel is alive — a new sample lands every tick
//! — even though its newest timestamp is always "old". Keying on *advance*
//! instead of *age* keeps those channels green while still reddening one that has
//! genuinely stopped.
//!
//! Two more properties keep the dot steady:
//!  - The cadence baseline is the interval between *advances* (the real
//!    production rate), not the spacing of samples within a batch — so a source
//!    that emits a burst per tick does not flicker between bursts.
//!  - Evaluation is throttled to `MIN_EVAL_INTERVAL_NS` (100 Hz) per channel;
//!    the dot holds its last value between evaluations, so it never flickers
//!    faster than that no matter how often the GUI repaints.

use eframe::egui::{self, Color32};

use crate::store::ChannelStore;
use crate::types::ChannelId;

/// How current a panel's data is relative to its own learned cadence.
///
/// Ordered by severity (`NoData` < `Fresh` < `Slowing` < `Stalled`) so a
/// multi-channel panel can take the worst state across its channels with
/// `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DataFreshness {
    /// Too few samples to judge a cadence (unresolved binding, or a channel
    /// that has barely started). No dot is drawn.
    NoData,
    /// A new sample landed within ~2× the typical interval — arriving on time.
    Fresh,
    /// No new sample for 2–5× the typical interval — arriving slower than before.
    Slowing,
    /// No new sample for ≥5× the typical interval — effectively stopped.
    Stalled,
}

/// Newest sample timestamps used to estimate the intra-batch gap. A wider window
/// is more likely to include the channel's normal quiet periods.
const SAMPLE_WINDOW: usize = 33;

/// Minimum wall-clock spacing between full freshness evaluations, per channel.
/// The dot holds its last value in between, so it changes at most 100×/second
/// regardless of the GUI repaint rate.
const MIN_EVAL_INTERVAL_NS: i64 = 10_000_000; // 100 Hz

impl DataFreshness {
    /// The title-bar dot color, or `None` when no dot should be drawn.
    /// Fixed semantic colors that read on both light and dark themes.
    pub fn dot_color(self) -> Option<Color32> {
        match self {
            DataFreshness::NoData => None,
            DataFreshness::Fresh => Some(Color32::from_rgb(0x40, 0xbf, 0x57)), // green
            DataFreshness::Slowing => Some(Color32::from_rgb(0xe5, 0xbf, 0x4b)), // amber
            DataFreshness::Stalled => Some(Color32::from_rgb(0xe0, 0x4f, 0x5f)), // red
        }
    }

    /// Human-readable hover text for the dot.
    pub fn tooltip(self) -> &'static str {
        match self {
            DataFreshness::NoData => "no data",
            DataFreshness::Fresh => "data arriving on time",
            DataFreshness::Slowing => "data arriving slower than before",
            DataFreshness::Stalled => "data stopped",
        }
    }
}

/// Decide freshness from the cadence baseline and the wall-clock time since a
/// new sample last arrived. Pure, so it is exhaustively unit-tested; the
/// stateful part lives in [`channel_freshness`].
fn classify(baseline_ns: i64, since_advance_ns: i64) -> DataFreshness {
    let baseline = baseline_ns.max(1);
    let since = since_advance_ns.max(0);
    if since < 2 * baseline {
        DataFreshness::Fresh
    } else if since < 5 * baseline {
        DataFreshness::Slowing
    } else {
        DataFreshness::Stalled
    }
}

/// Cross-frame freshness state, kept in egui memory so it survives between
/// frames without extra plumbing.
#[derive(Clone, Copy)]
struct Tracked {
    /// Newest sample timestamp seen for this channel.
    last_ts: i64,
    /// Wall clock (`now_ns`) when `last_ts` last increased.
    advanced_at: i64,
    /// Wall-clock interval between the last two advances — the channel's real
    /// production cadence. For a batched/diff source this is the batch interval,
    /// far longer than the spacing of samples within a batch, so the dot stays
    /// green between batches instead of oscillating.
    interval: i64,
    /// Wall clock of the last full evaluation (throttle anchor).
    evaluated_at: i64,
    /// Result of that evaluation, held until the next one.
    cached: DataFreshness,
}

fn tracked_id(id: ChannelId) -> egui::Id {
    egui::Id::new(("datavis_freshness", id.0))
}

/// Freshness of one channel, throttled to [`MIN_EVAL_INTERVAL_NS`].
fn channel_freshness(
    ctx: &egui::Context,
    store: &dyn ChannelStore,
    id: ChannelId,
    now_ns: i64,
) -> DataFreshness {
    let key = tracked_id(id);
    let prev = ctx.data(|d| d.get_temp::<Tracked>(key));

    // Throttle: within the eval window (and not scrubbed backwards), hold the
    // last value — caps the dot to 100 updates/second per channel.
    if let Some(p) = prev {
        if (0..MIN_EVAL_INTERVAL_NS).contains(&(now_ns - p.evaluated_at)) {
            return p.cached;
        }
    }

    let ts = store.recent_timestamps(id, SAMPLE_WINDOW);
    if ts.len() < 3 {
        return DataFreshness::NoData;
    }
    let newest = *ts.last().unwrap();
    // Longest gap between samples in the window (intra-batch spacing).
    let intra = ts.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(1).max(1);

    // Advance tracking: did the newest sample move forward since last eval?
    let (last_ts, advanced_at, interval) = match prev {
        Some(p) if newest > p.last_ts => (newest, now_ns, (now_ns - p.advanced_at).max(1)),
        Some(p) => (p.last_ts, p.advanced_at, p.interval),
        None => (newest, now_ns, intra),
    };

    // Judge against the slower of the two cadences so neither a bursty batch nor
    // a slow steady stream trips a false stall.
    let baseline = intra.max(interval);
    let result = if newest >= now_ns {
        // Newest sample at/ahead of the clock (replay position behind the
        // recording, or a just-arrived sample): live.
        DataFreshness::Fresh
    } else {
        classify(baseline, now_ns - advanced_at)
    };

    ctx.data_mut(|d| {
        d.insert_temp(
            key,
            Tracked { last_ts, advanced_at, interval, evaluated_at: now_ns, cached: result },
        )
    });
    result
}

/// Worst freshness across all of a panel's bound channels. `NoData` channels
/// are folded in via ordering, so a panel with one live and one silent channel
/// still reports the live one (`Fresh`+) rather than being dragged to `NoData`.
pub fn panel_freshness(
    ctx: &egui::Context,
    store: &dyn ChannelStore,
    channels: &[ChannelId],
    now_ns: i64,
) -> DataFreshness {
    channels
        .iter()
        .map(|&id| channel_freshness(ctx, store, id, now_ns))
        .max()
        .unwrap_or(DataFreshness::NoData)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::store::LiveStore;
    use crate::types::NumericVal;

    #[test]
    fn classify_thresholds() {
        // baseline 10 ns: <20 fresh, <50 slowing, ≥50 stalled.
        assert_eq!(classify(10, 5), DataFreshness::Fresh);
        assert_eq!(classify(10, 19), DataFreshness::Fresh);
        assert_eq!(classify(10, 20), DataFreshness::Slowing);
        assert_eq!(classify(10, 49), DataFreshness::Slowing);
        assert_eq!(classify(10, 50), DataFreshness::Stalled);
    }

    #[test]
    fn classify_clamps_degenerate_inputs() {
        assert_eq!(classify(0, 0), DataFreshness::Fresh); // zero baseline → 1 ns
        assert_eq!(classify(10, -100), DataFreshness::Fresh); // negative since → 0
    }

    #[test]
    fn severity_ordering_lets_max_pick_worst() {
        assert!(DataFreshness::Stalled > DataFreshness::Slowing);
        assert!(DataFreshness::Slowing > DataFreshness::Fresh);
        assert!(DataFreshness::Fresh > DataFreshness::NoData);
    }

    fn one_float_channel() -> (ChannelRegistry, LiveStore, ChannelId) {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[channels."x"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
max_rate = 1000
history_s = 10.0
"#,
        )
        .unwrap();
        let store = LiveStore::from_registry(&reg);
        let id = reg.id("x").unwrap();
        (reg, store, id)
    }

    #[test]
    fn too_few_samples_is_no_data() {
        let ctx = egui::Context::default();
        let (_reg, store, id) = one_float_channel();
        store.write_numeric(id, 0, NumericVal::Float(0.0));
        assert_eq!(channel_freshness(&ctx, &store, id, 100), DataFreshness::NoData);
    }

    /// A transform whose output lags the wall clock by a fixed offset but keeps
    /// producing (a new sample every tick) must stay green — the diff-script
    /// case. Once it stops, it must go red.
    #[test]
    fn lagged_but_live_channel_stays_fresh_then_stalls_when_stopped() {
        let ctx = egui::Context::default();
        let (_reg, store, id) = one_float_channel();
        let step = 10_000_000i64; // 10 ms cadence (= throttle window, so each eval runs)
        let lag = 500_000_000i64; // output stamped 0.5 s in the past
        let mut wall = 1_000_000_000i64;

        // Warm up with three lagged samples so a baseline exists.
        for _ in 0..3 {
            store.write_numeric(id, wall - lag, NumericVal::Float(1.0));
            wall += step;
        }

        // Live phase: a new (still-lagged) sample every tick → always Fresh,
        // even though `now - newest` is a constant 0.5 s.
        for _ in 0..20 {
            store.write_numeric(id, wall - lag, NumericVal::Float(1.0));
            assert_eq!(
                channel_freshness(&ctx, &store, id, wall),
                DataFreshness::Fresh,
                "lagged-but-live channel must read Fresh",
            );
            wall += step;
        }

        // Stop producing: newest stops advancing. After enough silence → red.
        let mut last = DataFreshness::Fresh;
        for _ in 0..20 {
            last = channel_freshness(&ctx, &store, id, wall);
            wall += step;
        }
        assert_eq!(last, DataFreshness::Stalled, "a stopped channel must read Stalled");
    }

    /// A source that emits a burst of closely-spaced samples once per tick must
    /// not oscillate: the cadence baseline is the batch interval, not the tiny
    /// within-batch spacing.
    #[test]
    fn batched_source_stays_fresh_between_batches() {
        let ctx = egui::Context::default();
        let (_reg, store, id) = one_float_channel();
        let batch_dt = 100_000_000i64; // 100 ms between batches
        let intra = 1_000_000i64; // 1 ms within a batch
        let mut wall = 1_000_000_000i64;

        let emit_batch = |at: i64| {
            for j in 0..5i64 {
                store.write_numeric(id, at + j * intra, NumericVal::Float(1.0));
            }
        };
        // Prime two batches so an inter-advance interval is measured.
        emit_batch(wall);
        let _ = channel_freshness(&ctx, &store, id, wall);
        wall += batch_dt;
        emit_batch(wall);
        let _ = channel_freshness(&ctx, &store, id, wall);

        // Now step through the quiet between this batch and the next, sampling at
        // 100 Hz. The dot must stay Fresh the whole 100 ms — no flicker.
        for k in 1..=10i64 {
            let t = wall + k * MIN_EVAL_INTERVAL_NS; // +10 ms each
            assert_eq!(
                channel_freshness(&ctx, &store, id, t),
                DataFreshness::Fresh,
                "batched source flickered mid-quiet at +{} ms",
                k * 10,
            );
        }
    }

    /// Within a 10 ms window the dot holds its cached value even if the
    /// underlying state would change — the 100 Hz cap.
    #[test]
    fn evaluation_is_throttled_to_100hz() {
        let ctx = egui::Context::default();
        let (_reg, store, id) = one_float_channel();
        let step = 10_000_000i64;
        for k in 0..3i64 {
            store.write_numeric(id, k * step, NumericVal::Float(0.0));
        }
        let base = 3 * step; // newest = 20 ms
        assert_eq!(channel_freshness(&ctx, &store, id, base), DataFreshness::Fresh);
        // Long silence → Stalled (a real recompute, >10 ms later).
        let stalled_now = base + 100 * step;
        assert_eq!(channel_freshness(&ctx, &store, id, stalled_now), DataFreshness::Stalled);
        // A new sample arrives — a recompute would flip to Fresh...
        store.write_numeric(id, stalled_now, NumericVal::Float(1.0));
        // ...but within 10 ms the throttled dot holds Stalled.
        assert_eq!(
            channel_freshness(&ctx, &store, id, stalled_now + 5_000_000),
            DataFreshness::Stalled,
            "within the throttle window the cached value must be held",
        );
        // After 10 ms it re-evaluates and sees the advance.
        assert_eq!(
            channel_freshness(&ctx, &store, id, stalled_now + MIN_EVAL_INTERVAL_NS),
            DataFreshness::Fresh,
        );
    }
}
