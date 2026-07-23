//! Runtime channel binding for dropped MQTT topics.
//!
//! A discovered MQTT topic (subscribed via `#`) has no `ChannelId` and no
//! store slot until it is configured. When such a topic is dropped onto a
//! panel we register it on the spot: infer its type from the last seen
//! payload, append a registry entry and a store slot, and tell the MQTT
//! ingest thread to route it. All growth is append-only and `&self`, so it is
//! safe while ingest writes to existing channels.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, NumericVal, SampleType};

/// Shared topic → (id, type) routing table. Built by MQTT ingest, extended by
/// the drop path so newly-registered topics start flowing to the store.
pub type MqttTopicMap = RwLock<HashMap<String, (ChannelId, SampleType)>>;

/// Guess a channel's sample type from one payload string: integer, then
/// float, then boolean keyword, else free text.
pub fn infer_sample_type(s: &str) -> SampleType {
    let t = s.trim();
    if t.parse::<i64>().is_ok() {
        SampleType::Int
    } else if t.parse::<f64>().is_ok() {
        SampleType::Float
    } else if matches!(
        t,
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" | "on" | "ON" | "off" | "OFF"
            | "yes" | "YES" | "no" | "NO"
    ) {
        SampleType::Bool
    } else {
        SampleType::Text
    }
}

fn bool_from_str(s: &str) -> bool {
    matches!(s.trim(), "1" | "true" | "True" | "TRUE" | "on" | "ON" | "yes" | "YES")
}

/// Write the current payload into the freshly-created slot so the panel shows
/// a value immediately, before the next MQTT message arrives.
fn seed_value(store: &dyn ChannelStore, id: ChannelId, ty: SampleType, value: &str) {
    let ts = store.now_ns();
    let v = value.trim();
    match ty {
        SampleType::Int => {
            if let Ok(n) = v.parse::<i64>() {
                store.write_numeric(id, ts, NumericVal::Int(n));
            }
        }
        SampleType::Float => {
            if let Ok(f) = v.parse::<f64>() {
                store.write_numeric(id, ts, NumericVal::Float(f));
            }
        }
        SampleType::Bool => store.write_numeric(id, ts, NumericVal::Bool(bool_from_str(v))),
        SampleType::Text => store.write_text(id, ts, v.to_string()),
    }
}

/// Resolve a dropped raw string to the channel name a panel should bind to.
///
/// - Already-configured channels (by `mqtt_topic` or by name) resolve directly.
/// - An unconfigured but *discovered* MQTT topic is registered on the fly:
///   type inferred from its last payload, registry + store grown, routing
///   table extended, current value seeded. Returns the topic as the name.
/// - Anything else (e.g. a raw string that is neither a channel nor a known
///   MQTT topic) returns `None` and binds nothing.
pub fn resolve_or_register_drop(
    raw: &str,
    channels: &ChannelRegistry,
    store: &dyn ChannelStore,
    mqtt: Option<(&MqttTopicMap, &BTreeMap<String, String>)>,
) -> Option<String> {
    if let Some(id) = channels.id_by_mqtt_topic(raw).or_else(|| channels.id(raw)) {
        return Some(channels.meta(id).name.clone());
    }
    // Not configured — only a known discovered MQTT topic can be registered.
    let (topic_map, snapshot) = mqtt?;
    let value = snapshot.get(raw)?;
    let ty = infer_sample_type(value);
    let id = channels.add_dynamic(raw, raw, ty);
    // Registry id and store index advance in lockstep: this is the only path
    // that calls both, and the early return above guarantees a fresh id here.
    store.add_channel(channels.meta(id).clone());
    seed_value(store, id, ty, value);
    topic_map.write().unwrap().insert(raw.to_string(), (id, ty));
    Some(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LiveStore;
    use crate::types::Sample;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
"#,
        )
        .unwrap()
    }

    #[test]
    fn infer_covers_int_float_bool_text() {
        assert_eq!(infer_sample_type("42"), SampleType::Int);
        assert_eq!(infer_sample_type("-3"), SampleType::Int);
        assert_eq!(infer_sample_type("3.14"), SampleType::Float);
        assert_eq!(infer_sample_type("on"), SampleType::Bool);
        assert_eq!(infer_sample_type("false"), SampleType::Bool);
        assert_eq!(infer_sample_type("hello world"), SampleType::Text);
    }

    #[test]
    fn configured_channel_resolves_without_mqtt() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let name = resolve_or_register_drop("demo.sine", &reg, &store, None);
        assert_eq!(name.as_deref(), Some("demo.sine"));
        assert_eq!(reg.len(), 1, "no dynamic channel added for a configured one");
    }

    #[test]
    fn unknown_without_snapshot_binds_nothing() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        assert_eq!(resolve_or_register_drop("home/x", &reg, &store, None), None);
    }

    #[test]
    fn discovered_mqtt_topic_is_registered_and_seeded() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let topic_map: MqttTopicMap = RwLock::new(HashMap::new());
        let mut snapshot = BTreeMap::new();
        snapshot.insert("home/sensors/temp".to_string(), "21.5".to_string());

        let name = resolve_or_register_drop(
            "home/sensors/temp",
            &reg,
            &store,
            Some((&topic_map, &snapshot)),
        );
        assert_eq!(name.as_deref(), Some("home/sensors/temp"));

        let id = reg.id("home/sensors/temp").expect("registered");
        assert_eq!(reg.meta(id).sample_type, SampleType::Float);
        // Store slot exists at the registry's id and holds the seeded value.
        assert!(matches!(store.latest(id), Some((_, Sample::Float(v))) if v == 21.5));
        // Routing table now carries the topic.
        assert_eq!(topic_map.read().unwrap().get("home/sensors/temp"), Some(&(id, SampleType::Float)));

        // Second drop is idempotent: no new channel.
        let n = reg.len();
        let again = resolve_or_register_drop(
            "home/sensors/temp",
            &reg,
            &store,
            Some((&topic_map, &snapshot)),
        );
        assert_eq!(again.as_deref(), Some("home/sensors/temp"));
        assert_eq!(reg.len(), n);
    }
}
