use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::ChannelRegistry;
use crate::store::{ChannelStore, LiveStore};
use crate::types::{now_ns, ChannelId, NumericVal, SampleType};

/// Dev-only synthetic data source (~1 kHz): sine → float channels,
/// wrapping counter → int, slow toggle → bool, periodic lines → text.
/// Runs until process exit.
pub fn spawn_demo(store: Arc<LiveStore>, reg: &ChannelRegistry) -> JoinHandle<()> {
    let targets: Vec<(ChannelId, SampleType)> = reg
        .iter_ids()
        .map(|id| (id, reg.meta(id).sample_type))
        .collect();
    std::thread::spawn(move || {
        let start = now_ns();
        let mut tick: u64 = 0;
        loop {
            let ts = now_ns();
            let t = (ts - start) as f64 / 1e9;
            for &(id, sample_type) in &targets {
                match sample_type {
                    SampleType::Float => {
                        let v = (2.0 * std::f64::consts::PI * t).sin() * 10.0;
                        store.write_numeric(id, ts, NumericVal::Float(v));
                    }
                    SampleType::Int => {
                        store.write_numeric(id, ts, NumericVal::Int((tick % 100) as i64));
                    }
                    SampleType::Bool => {
                        store.write_numeric(id, ts, NumericVal::Bool((tick / 500) % 2 == 0));
                    }
                    SampleType::Text => {
                        if tick % 250 == 0 {
                            store.write_text(id, ts, format!("demo log line {tick}"));
                        }
                    }
                }
            }
            tick += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::store::{ChannelStore, LiveStore};
    use std::sync::Arc;

    #[test]
    fn demo_feeds_all_channel_types() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
max_rate = 2000
history_s = 1.0

[channels."demo.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap();
        let store = Arc::new(LiveStore::from_registry(&reg));
        let _handle = spawn_demo(store.clone(), &reg);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let sine = reg.id("demo.sine").unwrap();
        let log = reg.id("demo.log").unwrap();
        assert!(store.latest(sine).is_some(), "no float data produced");
        assert!(store.latest(log).is_some(), "no text data produced");
    }
}
