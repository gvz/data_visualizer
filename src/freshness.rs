//! Per-panel data-freshness indicator.
//!
//! A panel's title bar shows a small dot that answers "is this channel still
//! arriving at the cadence it used to?". We learn each channel's typical
//! inter-arrival interval from its recent sample timestamps and compare the
//! age of the newest sample against it — no configured rate needed, so the
//! indicator adapts to whatever the source actually delivers.

use eframe::egui::Color32;

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
    /// Newest sample arrived within ~2× the typical interval — on time.
    Fresh,
    /// Overdue by 2–6× the typical interval — arriving slower than before.
    Slowing,
    /// Overdue by ≥6× the typical interval — effectively stopped.
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

/// Classify one channel's freshness from its recent sample timestamps
/// (oldest-first) against the current clock `now_ns`.
///
/// The cadence baseline is the *longest* recent inter-arrival gap, not the
/// median: event- and diff-driven sources (which push only on change) sit
/// quiet between updates, so judging against their busiest interval would cry
/// "offline" during a perfectly normal lull. Measuring against the longest
/// normal quiet keeps the dot green through those lulls and only reddens once
/// the silence clearly exceeds anything the channel has shown recently.
///
/// Needs at least three timestamps (two gaps) to have a baseline; fewer is
/// [`DataFreshness::NoData`].
pub fn classify(ts: &[i64], now_ns: i64) -> DataFreshness {
    if ts.len() < 3 {
        return DataFreshness::NoData;
    }
    let last = *ts.last().unwrap();
    // Longest recent gap, clamped to ≥1 ns so duplicate timestamps can't zero
    // out the multiples below.
    let baseline = ts.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(1).max(1);
    let age = (now_ns - last).max(0);
    if age < 2 * baseline {
        DataFreshness::Fresh
    } else if age < 5 * baseline {
        DataFreshness::Slowing
    } else {
        DataFreshness::Stalled
    }
}

/// Worst freshness across all of a panel's bound channels. `NoData` channels
/// are folded in via ordering, so a panel with one live and one silent channel
/// still reports the live one (`Fresh`+) rather than being dragged to `NoData`.
pub fn panel_freshness(
    store: &dyn ChannelStore,
    channels: &[ChannelId],
    now_ns: i64,
) -> DataFreshness {
    channels
        .iter()
        .map(|&id| classify(&store.recent_timestamps(id, SAMPLE_WINDOW), now_ns))
        .max()
        .unwrap_or(DataFreshness::NoData)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_few_samples_is_no_data() {
        assert_eq!(classify(&[], 100), DataFreshness::NoData);
        assert_eq!(classify(&[10, 20], 100), DataFreshness::NoData);
    }

    #[test]
    fn on_cadence_is_fresh() {
        // Longest gap 10 ns, newest sample 5 ns ago (< 2×10).
        let ts = [0, 10, 20, 30, 40];
        assert_eq!(classify(&ts, 45), DataFreshness::Fresh);
    }

    #[test]
    fn overdue_two_to_five_intervals_is_slowing() {
        // Longest gap 10 ns, newest sample 40 ns ago (2×..5×).
        let ts = [0, 10, 20, 30, 40];
        assert_eq!(classify(&ts, 80), DataFreshness::Slowing);
    }

    #[test]
    fn long_gap_is_stalled() {
        // Longest gap 10 ns, newest sample 100 ns ago (≥ 5×).
        let ts = [0, 10, 20, 30, 40];
        assert_eq!(classify(&ts, 140), DataFreshness::Stalled);
    }

    #[test]
    fn bursty_on_change_source_stays_fresh_during_normal_lull() {
        // An event-/diff-driven source: two bursts ~98 ns apart, so the longest
        // normal gap is 98 ns. A lull shorter than 2× that must stay Fresh
        // rather than falsely reading as stopped.
        let ts = [0, 1, 2, 100, 101, 102, 200];
        assert_eq!(classify(&ts, 350), DataFreshness::Fresh); // 150 ns lull
        // Only silence well past the longest normal gap reads as stopped.
        assert_eq!(classify(&ts, 200 + 5 * 98), DataFreshness::Stalled);
    }

    #[test]
    fn severity_ordering_lets_max_pick_worst() {
        assert!(DataFreshness::Stalled > DataFreshness::Slowing);
        assert!(DataFreshness::Slowing > DataFreshness::Fresh);
        assert!(DataFreshness::Fresh > DataFreshness::NoData);
    }

    #[test]
    fn duplicate_timestamps_do_not_panic() {
        // Zero-gap median is clamped to 1 ns; still classifies without dividing
        // by zero or overflowing.
        let ts = [5, 5, 5, 5];
        let _ = classify(&ts, 10);
    }

    #[test]
    fn panel_freshness_tracks_a_real_panel_and_store() {
        use crate::config::{ChannelRegistry, PanelEntry};
        use crate::store::LiveStore;
        use crate::types::NumericVal;
        use crate::viz::PanelRegistry;

        let reg = ChannelRegistry::from_toml_str(
            r#"
[channels."m.rpm"]
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
        let id = reg.id("m.rpm").unwrap();
        // Ten samples, 1 ms apart, ending at t = 9 ms.
        let step = 1_000_000i64; // 1 ms in ns
        for k in 0..10i64 {
            store.write_numeric(id, k * step, NumericVal::Float(k as f64));
        }

        let entry: PanelEntry = toml::from_str(
            r#"type = "numeric"
channel = "m.rpm""#,
        )
        .unwrap();
        let panel = PanelRegistry::with_builtins().build(&entry, &reg).unwrap();
        let ids = panel.data_channels();
        assert_eq!(ids, vec![id], "panel exposes its bound channel");

        let last = 9 * step;
        // Right after the last sample → on time.
        assert_eq!(panel_freshness(&store, &ids, last + step), DataFreshness::Fresh);
        // Long silence → stopped.
        assert_eq!(
            panel_freshness(&store, &ids, last + 100 * step),
            DataFreshness::Stalled,
        );
    }
}
