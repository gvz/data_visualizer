pub mod live;
pub mod ring;
pub mod text;

pub use live::LiveStore;
pub use ring::SoaRing;
pub use text::TextBuf;

use crate::types::{ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, TimeWindow};

/// The one interface viz panels see. Implemented by LiveStore (this plan)
/// and PlaybackStore (replay plan). Writers: ingest thread (live) or the
/// replay engine. Readers: main thread panels.
pub trait ChannelStore: Send + Sync {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal);
    fn write_text(&self, channel: ChannelId, ts: i64, line: String);
    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot;
    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)>;
    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta;
    /// For a discovered (interned-code) text channel, its code→label table
    /// indexed by code — so a state graph can turn the `Int` codes from
    /// [`snapshot`](Self::snapshot) back into state names. `None` for every
    /// other channel (numeric, or a verbatim `TextBuf` log channel).
    fn state_labels(&self, _channel: ChannelId) -> Option<Vec<String>> {
        None
    }
    /// Append a runtime-registered channel slot. The new slot's index must
    /// equal the ChannelId the registry assigned it. Live-only; the default
    /// is a no-op (replay has a fixed channel set).
    fn add_channel(&self, _meta: ChannelMeta) {}
    /// Sorted timestamps where a line plot must not connect across the gap
    /// (e.g. the join between two stitched recordings). A panel breaks its
    /// polyline at any of these that falls between two consecutive samples.
    /// Empty for a continuous store (the default).
    fn break_times(&self) -> &[i64] {
        &[]
    }
    /// Wall clock by default; PlaybackStore overrides to return playback position.
    fn now_ns(&self) -> i64 {
        crate::types::now_ns()
    }

    /// Monotonic counter bumped on every data write. The GUI polls this for
    /// cheap change detection so it repaints at full rate only while new
    /// samples arrive and stays idle otherwise. Default 0 = never changes.
    fn write_seq(&self) -> u64 {
        0
    }

    /// Newest sample at or before `end_ns`. When panels pass `now_ns()`, this
    /// honors the live scrub slider (and replay position) instead of always
    /// returning the very latest sample.
    fn latest_at(&self, channel: ChannelId, end_ns: i64) -> Option<(i64, Sample)> {
        match self.latest(channel) {
            None => None,
            Some((t, s)) if t <= end_ns => Some((t, s)),
            // Scrubbed into the past: pull the last sample within the window.
            Some(_) => snapshot_last(&self.snapshot(
                channel,
                TimeWindow { start_ns: i64::MIN, end_ns: end_ns + 1 },
            )),
        }
    }
}

/// Last (newest) sample of a snapshot, if any.
fn snapshot_last(snap: &ChannelSnapshot) -> Option<(i64, Sample)> {
    match snap {
        ChannelSnapshot::Float { ts, vals } => Some((*ts.last()?, Sample::Float(*vals.last()?))),
        ChannelSnapshot::Int { ts, vals } => Some((*ts.last()?, Sample::Int(*vals.last()?))),
        ChannelSnapshot::Bool { ts, vals } => Some((*ts.last()?, Sample::Bool(*vals.last()? != 0))),
        ChannelSnapshot::Text { lines } => {
            lines.last().map(|(t, l)| (*t, Sample::Text(l.clone())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LiveStore;
    use crate::config::ChannelRegistry;

    fn empty_registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(r#"
[channels."x"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap()
    }

    #[test]
    fn live_store_now_ns_returns_wall_clock() {
        let reg = empty_registry();
        let store = LiveStore::from_registry(&reg);
        let before = crate::types::now_ns();
        let got = store.now_ns();
        let after = crate::types::now_ns();
        assert!(got >= before, "now_ns should be >= before");
        assert!(got <= after, "now_ns should be <= after");
    }
}
