//! Per-panel data-freshness indicator.
//!
//! A panel's title bar shows a small dot that answers "is this channel still
//! producing data?". We learn each channel's typical inter-arrival interval
//! from its recent sample timestamps, then judge liveness by **how long since
//! its newest sample last moved forward** — in wall-clock time — rather than by
//! how old the newest sample is.
//!
//! That distinction matters for transforms whose output timestamps lag the wall
//! clock by a fixed offset (e.g. a windowed diff script that emits samples
//! stamped in the past). Such a channel is very much alive — a new sample lands
//! every tick — even though its newest timestamp is always "old". Keying on
//! *advance* instead of *age* keeps those channels green while still reddening a
//! channel that has genuinely stopped.

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

/// Newest sample timestamps used to estimate cadence. A wider window is more
/// likely to include the channel's normal quiet periods, so event-driven
/// sources are judged against a realistic gap rather than a burst.
const SAMPLE_WINDOW: usize = 33;

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
/// stateful part (measuring `since_advance_ns`) lives in [`channel_freshness`].
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

/// Cross-frame record of when a channel's newest sample last moved forward.
/// Stored in egui memory, so it survives between frames without extra plumbing.
#[derive(Clone, Copy)]
struct Advance {
    /// The newest sample timestamp we have seen for this channel.
    last_ts: i64,
    /// Wall clock (`now_ns`) at the frame we last saw `last_ts` increase.
    advanced_at: i64,
}

fn advance_id(id: ChannelId) -> egui::Id {
    egui::Id::new(("datavis_freshness_advance", id.0))
}

/// Wall-clock nanoseconds since `id`'s newest sample last advanced, updating the
/// stored record. First sight of a channel (or a fresh advance) resets the
/// clock to now, so a just-opened panel never flashes stalled.
fn since_last_advance(ctx: &egui::Context, id: ChannelId, newest_ts: i64, now_ns: i64) -> i64 {
    let key = advance_id(id);
    let prev = ctx.data(|d| d.get_temp::<Advance>(key));
    let rec = match prev {
        // No new sample since we last looked: keep the original advance time.
        Some(a) if newest_ts <= a.last_ts => a,
        // Advanced (or first sight): the newest sample moved forward now.
        _ => Advance { last_ts: newest_ts, advanced_at: now_ns },
    };
    ctx.data_mut(|d| d.insert_temp(key, rec));
    (now_ns - rec.advanced_at).max(0)
}

/// Freshness of one channel: cadence baseline from its recent gaps, liveness
/// from how long since its newest sample last advanced.
fn channel_freshness(
    ctx: &egui::Context,
    store: &dyn ChannelStore,
    id: ChannelId,
    now_ns: i64,
) -> DataFreshness {
    let ts = store.recent_timestamps(id, SAMPLE_WINDOW);
    if ts.len() < 3 {
        return DataFreshness::NoData;
    }
    let newest = *ts.last().unwrap();
    // Newest sample at or ahead of the clock: we simply have not caught up to it
    // (replay position behind the recording, or a just-arrived sample). Live.
    if newest >= now_ns {
        // Still record the advance so a later stall is measured from here.
        let _ = since_last_advance(ctx, id, newest, now_ns);
        return DataFreshness::Fresh;
    }
    // Baseline cadence = the longest recent inter-arrival gap, so event-/
    // diff-driven sources are judged against their normal quiet, not a burst.
    let baseline = ts.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(1);
    let since = since_last_advance(ctx, id, newest, now_ns);
    classify(baseline, since)
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
        // Zero baseline is clamped to 1 ns; negative "since" to 0 (fresh).
        assert_eq!(classify(0, 0), DataFreshness::Fresh);
        assert_eq!(classify(10, -100), DataFreshness::Fresh);
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
    /// producing (a new sample every tick) must stay green — the case the diff
    /// script hits. Then, once it stops producing, it must go red.
    #[test]
    fn lagged_but_live_channel_stays_fresh_then_stalls_when_stopped() {
        let ctx = egui::Context::default();
        let (_reg, store, id) = one_float_channel();
        let step = 10_000_000i64; // 10 ms cadence
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

        // Stop producing: newest stops advancing. Wall time keeps moving, so
        // after enough silence the dot must turn red.
        let stopped_at = wall;
        let mut last = DataFreshness::Fresh;
        for _ in 0..20 {
            last = channel_freshness(&ctx, &store, id, wall);
            wall += step;
        }
        assert_eq!(
            last,
            DataFreshness::Stalled,
            "a channel that stopped producing must eventually read Stalled",
        );
        // Sanity: it was still Fresh immediately after the last sample.
        let ctx2 = egui::Context::default();
        // (fresh baseline established by one warm read at stopped_at)
        let _ = channel_freshness(&ctx2, &store, id, stopped_at);
        assert_eq!(
            channel_freshness(&ctx2, &store, id, stopped_at + step),
            DataFreshness::Fresh,
        );
    }

    #[test]
    fn panel_freshness_worst_across_channels() {
        let ctx = egui::Context::default();
        let (_reg, store, id) = one_float_channel();
        // Three samples 10 ms apart, newest at t=20 ms; clock far in the future
        // with no advance since → stalled after the first observation.
        let step = 10_000_000i64;
        for k in 0..3i64 {
            store.write_numeric(id, k * step, NumericVal::Float(k as f64));
        }
        let ids = [id];
        let newest = 2 * step;
        // First look establishes the advance clock → Fresh.
        assert_eq!(panel_freshness(&ctx, &store, &ids, newest), DataFreshness::Fresh);
        // Much later with no new sample → Stalled.
        assert_eq!(
            panel_freshness(&ctx, &store, &ids, newest + 100 * step),
            DataFreshness::Stalled,
        );
    }
}
