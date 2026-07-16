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
}
