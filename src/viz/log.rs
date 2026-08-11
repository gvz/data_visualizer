//! Iced-port log panel — scrollable latest-N text entries from one or more
//! text channels.
//!
//! `sync` appends new entries from each bound channel via `store.snapshot`,
//! merges, sorts by timestamp, then truncates to the configured cap so the
//! buffer never grows unboundedly.  `view` renders a `scrollable(column![...])`
//! in monospace.

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, ChannelSnapshot, TimeWindow};
use crate::viz::{PanelMessage, VizPanel};

// ---------------------------------------------------------------------------
// Helpers (inlined per-panel)
// ---------------------------------------------------------------------------

fn opt_str_array(cfg: &toml::Table, key: &str) -> Vec<String> {
    cfg.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn opt_i64(cfg: &toml::Table, key: &str, default: i64) -> i64 {
    match cfg.get(key) {
        Some(toml::Value::Integer(i)) => *i,
        _ => default,
    }
}

/// Format a nanosecond timestamp as `HH:MM:SS.mmm` (UTC time of day).
fn format_time_of_day(ts_ns: i64) -> String {
    let secs = ts_ns.div_euclid(1_000_000_000);
    let millis = ts_ns.rem_euclid(1_000_000_000) / 1_000_000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}.{millis:03}")
}

// ---------------------------------------------------------------------------
// Bound-channel bookkeeping
// ---------------------------------------------------------------------------

struct BoundChannel {
    name: String,
    id: Option<ChannelId>,
    /// Highest timestamp seen so far; used to skip already-ingested entries.
    last_ts: i64,
}

impl BoundChannel {
    fn new(name: String, ch: &ChannelRegistry) -> Self {
        let id = ch.id(&name);
        BoundChannel { name, id, last_ts: i64::MIN }
    }
}

// ---------------------------------------------------------------------------
// Log panel
// ---------------------------------------------------------------------------

pub const TYPE_NAME: &str = "log";

const DEFAULT_MAX: usize = 500;

/// Scrolling timestamped log merged from one or more text channels.
pub struct Log {
    channels: Vec<BoundChannel>,
    max_lines: usize,
    /// Accumulated (timestamp_ns, line) entries, newest at the end.
    entries: Vec<(i64, String)>,
}

impl Log {
    /// Construct from a TOML config table and a channel registry.
    ///
    /// `config` may contain:
    /// - `channels = ["name1", "name2", ...]`
    /// - `max_lines = N` (defaults to 500)
    pub fn new(cfg: &toml::Table, ch: &ChannelRegistry) -> Self {
        let names = opt_str_array(cfg, "channels");
        let max_lines = opt_i64(cfg, "max_lines", DEFAULT_MAX as i64).max(1) as usize;
        Log {
            channels: names.iter().map(|n| BoundChannel::new(n.clone(), ch)).collect(),
            max_lines,
            entries: Vec::new(),
        }
    }

    /// Number of retained log entries (used in tests).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// All retained entries (timestamp_ns, line), oldest first.
    pub fn entries(&self) -> &[(i64, String)] {
        &self.entries
    }
}

impl VizPanel for Log {
    fn title(&self) -> String {
        // Use first channel name as title, or "Log" when no channels bound.
        self.channels
            .first()
            .map(|b| b.name.clone())
            .unwrap_or_else(|| "Log".to_string())
    }

    fn sync(&mut self, store: &dyn ChannelStore) {
        // Collect new lines from every bound channel since last seen timestamp.
        for ch in &mut self.channels {
            let Some(id) = ch.id else { continue };
            let window = TimeWindow {
                start_ns: ch.last_ts.saturating_add(1),
                end_ns: i64::MAX,
            };
            let snap = store.snapshot(id, window);
            if let ChannelSnapshot::Text { lines } = snap {
                for (ts, line) in lines {
                    if ts > ch.last_ts {
                        ch.last_ts = ts;
                    }
                    self.entries.push((ts, line));
                }
            }
        }

        // Sort by timestamp so multi-channel merges are chronological.
        self.entries.sort_by_key(|(t, _)| *t);

        // Cap: keep only the newest `max_lines` entries.
        let cap = self.max_lines;
        if self.entries.len() > cap {
            let drain_count = self.entries.len() - cap;
            self.entries.drain(..drain_count);
        }
    }

    fn view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{column, scrollable, text};
        use iced::{Font, Length};

        let mut col = column![];
        for (ts, line) in &self.entries {
            let row_text = format!("{}  {}", format_time_of_day(*ts), line);
            col = col.push(
                text(row_text)
                    .font(Font::MONOSPACE)
                    .size(13),
            );
        }

        scrollable(col.width(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn update(&mut self, _msg: PanelMessage) {
        // No interactive messages for this panel.
    }

    fn config_view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, text};
        let bound: Vec<&str> = self.channels.iter().map(|b| b.name.as_str()).collect();
        let label = if bound.is_empty() {
            "No channels bound — drag text channels here.".to_string()
        } else {
            format!("Channels: {}  |  Max lines: {}", bound.join(", "), self.max_lines)
        };
        container(text(label)).into()
    }

    fn bind_channel(&mut self, name: &str) -> bool {
        // Duplicate guard: reject if already bound.
        if self.channels.iter().any(|b| b.name == name) {
            return false;
        }
        // We don't have registry access here; id stays None until next construction.
        self.channels.push(BoundChannel {
            name: name.to_string(),
            id: None,
            last_ts: i64::MIN,
        });
        true
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channels".to_string(),
            toml::Value::Array(
                self.channels
                    .iter()
                    .map(|b| toml::Value::String(b.name.clone()))
                    .collect(),
            ),
        );
        t.insert(
            "max_lines".to_string(),
            toml::Value::Integer(self.max_lines as i64),
        );
        t
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::store::LiveStore;

    const CHANNEL_TOML: &str = r#"
[channels."system.log"]
mqtt_topic = "log/system"
type = "text"

[channels."app.log"]
mqtt_topic = "log/app"
type = "text"
"#;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(CHANNEL_TOML).unwrap()
    }

    // --- required test from the brief ---

    /// Log keeps at most `cap` entries even after many sync calls with new data.
    #[test]
    fn log_caps_entries_at_max_lines() {
        let ch = registry();
        let store = LiveStore::from_registry(&ch);
        let sys_id = ch.id("system.log").unwrap();

        let cap: usize = 5;
        let mut cfg = toml::Table::new();
        cfg.insert("channels".into(), toml::Value::Array(vec![
            toml::Value::String("system.log".into())
        ]));
        cfg.insert("max_lines".into(), toml::Value::Integer(cap as i64));
        let mut panel = Log::new(&cfg, &ch);

        // Push 3× more entries than the cap over multiple sync calls.
        for i in 0..(cap * 3) {
            store.write_text(sys_id, i as i64 + 1, format!("line {i}"));
        }
        // Multiple syncs — cap must hold after every call.
        panel.sync(&store);
        panel.sync(&store); // second sync must not double-count or blow cap
        assert!(
            panel.len() <= cap,
            "expected ≤{cap} entries, got {}",
            panel.len()
        );
    }

    #[test]
    fn log_retains_newest_entries_when_capped() {
        let ch = registry();
        let store = LiveStore::from_registry(&ch);
        let sys_id = ch.id("system.log").unwrap();

        let cap: usize = 3;
        let mut cfg = toml::Table::new();
        cfg.insert("channels".into(), toml::Value::Array(vec![
            toml::Value::String("system.log".into())
        ]));
        cfg.insert("max_lines".into(), toml::Value::Integer(cap as i64));
        let mut panel = Log::new(&cfg, &ch);

        for i in 0..10_u64 {
            store.write_text(sys_id, i as i64 + 1, format!("line {i}"));
        }
        panel.sync(&store);
        assert_eq!(panel.len(), cap);
        // The last retained entry is the newest.
        let entries = panel.entries();
        assert_eq!(entries.last().map(|(_, l)| l.as_str()), Some("line 9"));
    }

    // --- serialize round-trip ---

    #[test]
    fn log_serialize_roundtrips() {
        let ch = registry();
        let cfg: toml::Table = toml::from_str(
            r#"channels = ["system.log", "app.log"]
max_lines = 200"#,
        )
        .unwrap();
        let p = Log::new(&cfg, &ch);
        let out = p.serialize();
        assert_eq!(
            out.get("channels")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(2),
        );
        assert_eq!(
            out.get("max_lines").and_then(|v| v.as_integer()),
            Some(200),
        );
        // Reparsing produces identical panel.
        let p2 = Log::new(&out, &ch);
        assert_eq!(p2.serialize(), out);
    }

    #[test]
    fn log_title_first_channel() {
        let ch = registry();
        let cfg: toml::Table = toml::from_str(
            r#"channels = ["system.log", "app.log"]"#,
        )
        .unwrap();
        let p = Log::new(&cfg, &ch);
        assert_eq!(p.title(), "system.log");
    }

    #[test]
    fn log_title_default_no_channels() {
        let ch = registry();
        let p = Log::new(&toml::Table::new(), &ch);
        assert_eq!(p.title(), "Log");
    }

    #[test]
    fn log_sync_multi_channel_merges_sorted() {
        let ch = registry();
        let store = LiveStore::from_registry(&ch);
        let sys_id = ch.id("system.log").unwrap();
        let app_id = ch.id("app.log").unwrap();

        store.write_text(sys_id, 3, "sys:C".into());
        store.write_text(sys_id, 1, "sys:A".into());
        store.write_text(app_id, 2, "app:B".into());

        let cfg: toml::Table = toml::from_str(
            r#"channels = ["system.log", "app.log"]
max_lines = 50"#,
        )
        .unwrap();
        let mut p = Log::new(&cfg, &ch);
        p.sync(&store);

        let entries = p.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].1, "sys:A");
        assert_eq!(entries[1].1, "app:B");
        assert_eq!(entries[2].1, "sys:C");
    }

    #[test]
    fn log_bind_channel_rejects_duplicate() {
        let ch = registry();
        let cfg: toml::Table = toml::from_str(
            r#"channels = ["system.log"]"#,
        )
        .unwrap();
        let mut p = Log::new(&cfg, &ch);
        assert!(!p.bind_channel("system.log"));
        assert!(p.bind_channel("app.log"));
    }
}
