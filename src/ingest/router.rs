use std::collections::HashMap;

use prost_reflect::MessageDescriptor;

use crate::config::ChannelRegistry;
use crate::ingest::loader::ProtoSchema;
use crate::types::{ChannelId, SampleType};

pub struct ChannelBinding {
    pub id: ChannelId,
    pub msg_desc: MessageDescriptor,
    pub val_path: Vec<String>,
    pub ts_path: Vec<String>,
    pub eu_scale: f64,
    pub eu_offset: f64,
    pub sample_type: SampleType,
}

pub struct TopicRouter {
    map: HashMap<String, Vec<ChannelBinding>>,
}

impl TopicRouter {
    pub fn build(registry: &ChannelRegistry, schema: &ProtoSchema) -> Self {
        let mut map: HashMap<String, Vec<ChannelBinding>> = HashMap::new();
        for id in registry.iter_ids() {
            let cfg = registry.config(id);
            let meta = registry.meta(id);
            let (Some(topic), Some(proto_path), Some(ts_path)) =
                (&cfg.topic, &cfg.proto_path, &cfg.ts_path)
            else {
                continue; // MQTT-only channel; no ZMQ binding
            };
            match schema.resolve(proto_path, ts_path) {
                Ok(desc) => {
                    let binding = ChannelBinding {
                        id,
                        msg_desc: desc.msg_desc,
                        val_path: desc.val_path,
                        ts_path: desc.ts_path,
                        eu_scale: cfg.eu_scale,
                        eu_offset: cfg.eu_offset,
                        sample_type: meta.sample_type,
                    };
                    map.entry(topic.clone()).or_default().push(binding);
                }
                Err(e) => {
                    eprintln!("ingest: skipping channel {:?}: {e}", meta.name);
                }
            }
        }
        Self { map }
    }

    pub fn topics(&self) -> impl Iterator<Item = &str> + '_ {
        self.map.keys().map(|s| s.as_str())
    }

    pub fn bindings_for(&self, topic: &str) -> &[ChannelBinding] {
        self.map.get(topic).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::loader::ProtoSchema;
    use std::io::Write;

    fn write_test_proto(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message AccelBatch {{
  repeated Sample samples = 1;
  message Sample {{
    int64 t_ns = 1;
    float x = 2;
    float y = 3;
  }}
}}
message StatusBatch {{
  repeated Sample samples = 1;
  message Sample {{
    int64 t_ns = 1;
    int64 state = 2;
  }}
}}
"#).unwrap();
        path
    }

    fn test_schema() -> (ProtoSchema, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path());
        (ProtoSchema::from_path(&path).unwrap(), dir)
    }

    fn test_registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"

[channels."accel.y"]
topic = "accel"
proto_path = "AccelBatch.samples.y"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
eu_scale = 2.0
eu_offset = -1.0

[channels."motor.state"]
topic = "status"
proto_path = "StatusBatch.samples.state"
ts_path = "StatusBatch.samples.t_ns"
type = "int"
"#).unwrap()
    }

    #[test]
    fn router_routes_two_topics() {
        let (schema, _dir) = test_schema();
        let registry = test_registry();
        let router = TopicRouter::build(&registry, &schema);

        assert_eq!(router.bindings_for("accel").len(), 2);
        assert_eq!(router.bindings_for("status").len(), 1);
        assert!(router.bindings_for("unknown").is_empty());
    }

    #[test]
    fn router_preserves_eu_scale() {
        let (schema, _dir) = test_schema();
        let registry = test_registry();
        let router = TopicRouter::build(&registry, &schema);
        let accel = router.bindings_for("accel");
        let y = accel.iter().find(|b| b.val_path.last().map(|s| s.as_str()) == Some("y")).unwrap();
        assert_eq!(y.eu_scale, 2.0);
        assert_eq!(y.eu_offset, -1.0);
    }

    #[test]
    fn router_topics_iterator() {
        let (schema, _dir) = test_schema();
        let registry = test_registry();
        let router = TopicRouter::build(&registry, &schema);
        let mut topics: Vec<&str> = router.topics().collect();
        topics.sort();
        assert_eq!(topics, vec!["accel", "status"]);
    }

    #[test]
    fn router_skips_channel_with_unknown_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.proto");
        std::fs::write(&path, b"syntax = \"proto3\";\n").unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."bad"]
topic = "t"
proto_path = "NoMsg.field"
ts_path = "NoMsg.t"
type = "float"
"#).unwrap();
        let router = TopicRouter::build(&registry, &schema);
        assert!(router.bindings_for("t").is_empty());
    }
}
