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
    /// Wall clock by default; PlaybackStore overrides to return playback position.
    fn now_ns(&self) -> i64 {
        crate::types::now_ns()
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
