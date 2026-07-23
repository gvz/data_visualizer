use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use crate::dynamic_channel::MqttTopicMap;
use crate::record::mqtt_schema::DynamicProtoRegistry;
use crate::record::RecordMsg;
use crate::store::ChannelStore;
use crate::types::{NumericVal, SampleType};

/// Per-message handling shared by discover + record + route-to-store sources
/// (the MQTT family). A transport supplies `(topic, payload, ts)`; this does
/// discovery, dynamic-schema recording, topic routing and typed store writes.
pub struct ScalarIngest {
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    topic_map: Arc<MqttTopicMap>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    proto_registry: DynamicProtoRegistry,
}

impl ScalarIngest {
    pub fn new(
        discovered: Arc<Mutex<BTreeMap<String, String>>>,
        topic_map: Arc<MqttTopicMap>,
        store: Arc<dyn ChannelStore>,
        record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    ) -> Self {
        Self {
            discovered,
            topic_map,
            store,
            record_sender,
            proto_registry: DynamicProtoRegistry::new(),
        }
    }

    /// Handle one received message: update discovery, queue a record frame if
    /// recording, then route to the store if a channel is bound to `topic`.
    pub fn on_message(&mut self, topic: &str, payload: &str, ts: i64) {
        self.discovered.lock().unwrap().insert(topic.to_string(), payload.to_string());

        if let Ok(guard) = self.record_sender.try_lock() {
            record_publish(&mut self.proto_registry, &guard, topic, payload, ts);
        }

        let Some((id, sample_type)) = self.topic_map.read().unwrap().get(topic).copied() else {
            return;
        };
        match sample_type {
            SampleType::Float => {
                if let Ok(v) = payload.parse::<f64>() {
                    self.store.write_numeric(id, ts, NumericVal::Float(v));
                }
            }
            SampleType::Int => {
                if let Ok(v) = payload.parse::<i64>() {
                    self.store.write_numeric(id, ts, NumericVal::Int(v));
                }
            }
            SampleType::Bool => {
                let v = matches!(
                    payload,
                    "1" | "true" | "True" | "TRUE" | "on" | "ON" | "yes" | "YES"
                );
                self.store.write_numeric(id, ts, NumericVal::Bool(v));
            }
            SampleType::Text => {
                self.store.write_text(id, ts, payload.to_string());
            }
        }
    }
}

/// Encode one publish and queue it for the recorder, if recording is active.
/// Generates the topic's schema on first sight. A parse mismatch or a full
/// queue silently drops the sample.
fn record_publish(
    reg: &mut DynamicProtoRegistry,
    sender: &Option<Sender<RecordMsg>>,
    topic: &str,
    payload: &str,
    ts: i64,
) {
    let Some(tx) = sender else { return };
    if let Some((schema, data)) = reg.record_frame(topic, ts, payload) {
        let _ = tx.try_send(RecordMsg::DynamicProto {
            topic: Arc::from(topic),
            schema,
            data,
            ts,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::record::{record_channel, RecordMsg};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::{ChannelId, Sample, SampleType};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, RwLock};

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."sensor/temp"]
mqtt_topic = "home/temp"
type = "float"
"#,
        )
        .unwrap()
    }

    #[test]
    fn on_message_discovers_records_and_routes() {
        let reg = registry();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));

        // Build the topic_map the way spawn does.
        let mut initial: HashMap<String, (ChannelId, SampleType)> = HashMap::new();
        for id in reg.iter_ids() {
            if let Some(t) = &reg.config(id).mqtt_topic {
                initial.insert(t.clone(), (id, reg.meta(id).sample_type));
            }
        }
        let temp_id = reg.iter_ids().next().unwrap();
        let topic_map = Arc::new(RwLock::new(initial));
        let discovered = Arc::new(Mutex::new(BTreeMap::new()));
        let (tx, rx) = record_channel();
        let record_sender = Arc::new(Mutex::new(Some(tx)));

        let mut ingest = ScalarIngest::new(
            discovered.clone(),
            topic_map,
            store.clone(),
            record_sender,
        );
        ingest.on_message("home/temp", "21.5", 1_000);

        // discovered updated
        assert_eq!(discovered.lock().unwrap().get("home/temp").map(String::as_str), Some("21.5"));
        // routed to the store as a float sample
        assert_eq!(store.latest(temp_id), Some((1_000, Sample::Float(21.5))));
        // a dynamic-proto frame was queued
        match rx.try_recv().unwrap() {
            RecordMsg::DynamicProto { topic, ts, .. } => {
                assert_eq!(topic.as_ref(), "home/temp");
                assert_eq!(ts, 1_000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn on_message_without_sender_still_discovers() {
        let reg = registry();
        let store: Arc<dyn crate::store::ChannelStore> =
            Arc::new(LiveStore::from_registry(&reg));
        let topic_map = Arc::new(RwLock::new(HashMap::new()));
        let discovered = Arc::new(Mutex::new(BTreeMap::new()));
        let record_sender = Arc::new(Mutex::new(None));

        let mut ingest = ScalarIngest::new(discovered.clone(), topic_map, store, record_sender);
        ingest.on_message("a/b", "hello", 0);
        assert_eq!(discovered.lock().unwrap().get("a/b").map(String::as_str), Some("hello"));
    }

    #[test]
    fn no_sender_does_not_lock_topic_proto_type() {
        // With no recorder installed, on_message must not touch the dynamic proto
        // registry — so a topic's type stays uninferred and a later payload (once
        // recording starts) can still determine it. A text-looking payload seen
        // while not recording must not lock "a/b" as Text.
        let reg = registry();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let topic_map = Arc::new(RwLock::new(HashMap::new()));
        let discovered = Arc::new(Mutex::new(BTreeMap::new()));
        let (tx, rx) = record_channel();
        // Start with no sender, feed a text-looking payload, then install a sender.
        let record_sender = Arc::new(Mutex::new(None));
        let mut ingest =
            ScalarIngest::new(discovered, topic_map, store, record_sender.clone());
        ingest.on_message("a/b", "hello", 0);
        *record_sender.lock().unwrap() = Some(tx);

        // A numeric payload now must still infer Int (would decode as Text if the
        // earlier "hello" had locked the topic).
        ingest.on_message("a/b", "5", 1);
        match rx.try_recv().unwrap() {
            RecordMsg::DynamicProto { schema, data, .. } => {
                let pool = prost_reflect::DescriptorPool::decode(schema.as_ref()).unwrap();
                let desc = pool.get_message_by_name("mqtt.AB").unwrap();
                let msg = prost_reflect::DynamicMessage::decode(desc, data.as_ref()).unwrap();
                assert_eq!(msg.get_field_by_name("value").unwrap().as_i64(), Some(5));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
