use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use prost_reflect::prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, Value};
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet};

use crate::types::SampleType;

/// Bool payload tokens shared by inference and encoding.
const BOOL_TRUE: &[&str] = &["1", "true", "True", "TRUE", "on", "ON", "yes", "YES"];
const BOOL_FALSE: &[&str] = &["0", "false", "False", "FALSE", "off", "OFF", "no", "NO"];

struct TopicEntry {
    sample_type: SampleType,
    descriptor: MessageDescriptor,
    /// Self-contained FileDescriptorSet (this topic's one file) used as the MCAP
    /// protobuf schema payload.
    schema_bytes: Arc<[u8]>,
}

/// Generates a protobuf schema per MQTT topic at runtime and encodes scalar
/// samples into `DynamicMessage`s. Single-threaded owner (the MQTT ingest
/// thread); no internal locking.
pub struct DynamicProtoRegistry {
    pool: DescriptorPool,
    entries: HashMap<String, TopicEntry>,
    used_names: HashSet<String>,
}

impl DynamicProtoRegistry {
    pub fn new() -> Self {
        Self {
            pool: DescriptorPool::new(),
            entries: HashMap::new(),
            used_names: HashSet::new(),
        }
    }

    /// Infer a sample type from a payload: i64 → Int; else f64 → Float; else a
    /// textual bool token → Bool; else Text. Numeric `"1"`/`"0"` infer Int.
    pub fn infer_type(payload: &str) -> SampleType {
        if payload.parse::<i64>().is_ok() {
            SampleType::Int
        } else if payload.parse::<f64>().is_ok() {
            SampleType::Float
        } else if is_textual_bool(payload) {
            SampleType::Bool
        } else {
            SampleType::Text
        }
    }

    /// One-shot ingest hot path: lazily build the topic's schema on first sight
    /// (type inferred + locked), then encode the sample. Returns the topic's
    /// schema bytes and the encoded message, or `None` on parse mismatch or a
    /// schema build failure.
    pub fn record_frame(
        &mut self,
        topic: &str,
        ts_ns: i64,
        payload: &str,
    ) -> Option<(Arc<[u8]>, Vec<u8>)> {
        let sample_type = match self.entries.get(topic) {
            Some(e) => e.sample_type,
            None => {
                let ty = Self::infer_type(payload);
                self.build_entry(topic, ty)?;
                ty
            }
        };
        let value = parse_value(payload, sample_type)?;
        let entry = self.entries.get(topic)?;
        let mut msg = DynamicMessage::new(entry.descriptor.clone());
        msg.set_field_by_name("t_ns", Value::I64(ts_ns));
        msg.set_field_by_name("value", value);
        Some((entry.schema_bytes.clone(), msg.encode_to_vec()))
    }

    /// Build and cache the generated message + schema for a new topic.
    fn build_entry(&mut self, topic: &str, sample_type: SampleType) -> Option<()> {
        let msg_name = self.unique_name(topic);
        let file = build_file_descriptor(&msg_name, sample_type);
        if self.pool.add_file_descriptor_proto(file.clone()).is_err() {
            return None;
        }
        let fq = format!("mqtt.{msg_name}");
        let descriptor = self.pool.get_message_by_name(&fq)?;
        let schema_bytes: Arc<[u8]> = FileDescriptorSet { file: vec![file] }
            .encode_to_vec()
            .into();
        self.entries.insert(
            topic.to_string(),
            TopicEntry { sample_type, descriptor, schema_bytes },
        );
        self.used_names.insert(msg_name);
        Some(())
    }

    /// A unique, valid protobuf message identifier derived from the topic.
    fn unique_name(&self, topic: &str) -> String {
        let base = sanitize(topic);
        let mut name = base.clone();
        let mut n = 2;
        while self.used_names.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        name
    }
}

fn is_textual_bool(payload: &str) -> bool {
    // Textual tokens only — numeric "1"/"0" are excluded so they infer Int.
    (BOOL_TRUE.contains(&payload) || BOOL_FALSE.contains(&payload))
        && payload != "1"
        && payload != "0"
}

fn parse_value(payload: &str, sample_type: SampleType) -> Option<Value> {
    match sample_type {
        SampleType::Float => payload.parse::<f64>().ok().map(Value::F64),
        SampleType::Int => payload.parse::<i64>().ok().map(Value::I64),
        SampleType::Bool => {
            if BOOL_TRUE.contains(&payload) {
                Some(Value::Bool(true))
            } else if BOOL_FALSE.contains(&payload) {
                Some(Value::Bool(false))
            } else {
                None
            }
        }
        SampleType::Text => Some(Value::String(payload.to_string())),
    }
}

/// CamelCase valid proto identifier from a topic; `T`-prefixed if empty or
/// digit-leading.
fn sanitize(topic: &str) -> String {
    let mut out = String::new();
    for segment in topic.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() || out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, 'T');
    }
    out
}

/// Hand-build a proto3 file with one message `<msg_name>` holding `t_ns` (int64)
/// and a typed `value` field.
fn build_file_descriptor(msg_name: &str, sample_type: SampleType) -> FileDescriptorProto {
    let value_type = match sample_type {
        SampleType::Float => Type::Double,
        SampleType::Int => Type::Int64,
        SampleType::Bool => Type::Bool,
        SampleType::Text => Type::String,
    };
    let field = |name: &str, number: i32, ty: Type| FieldDescriptorProto {
        name: Some(name.to_string()),
        number: Some(number),
        label: Some(Label::Optional as i32),
        r#type: Some(ty as i32),
        ..Default::default()
    };
    FileDescriptorProto {
        name: Some(format!("mqtt/{msg_name}.proto")),
        package: Some("mqtt".to_string()),
        syntax: Some("proto3".to_string()),
        message_type: vec![DescriptorProto {
            name: Some(msg_name.to_string()),
            field: vec![field("t_ns", 1, Type::Int64), field("value", 2, value_type)],
            ..Default::default()
        }],
        ..Default::default()
    }
}

impl Default for DynamicProtoRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SampleType;
    use prost_reflect::DynamicMessage;

    #[test]
    fn infer_type_table() {
        assert_eq!(DynamicProtoRegistry::infer_type("42"), SampleType::Int);
        assert_eq!(DynamicProtoRegistry::infer_type("-7"), SampleType::Int);
        assert_eq!(DynamicProtoRegistry::infer_type("3.14"), SampleType::Float);
        assert_eq!(DynamicProtoRegistry::infer_type("true"), SampleType::Bool);
        assert_eq!(DynamicProtoRegistry::infer_type("off"), SampleType::Bool);
        assert_eq!(DynamicProtoRegistry::infer_type("hello"), SampleType::Text);
        // Numeric 1/0 infer Int, not Bool (i64-first ordering).
        assert_eq!(DynamicProtoRegistry::infer_type("1"), SampleType::Int);
        assert_eq!(DynamicProtoRegistry::infer_type("0"), SampleType::Int);
    }

    /// Encode a frame, then decode it back with the topic's generated descriptor
    /// (rebuilt from the returned self-contained FileDescriptorSet) and check the
    /// t_ns + value fields.
    fn decode_back(schema: &[u8], data: &[u8], msg_name: &str) -> DynamicMessage {
        let pool = prost_reflect::DescriptorPool::decode(schema).unwrap();
        let desc = pool.get_message_by_name(msg_name).unwrap();
        DynamicMessage::decode(desc, data).unwrap()
    }

    #[test]
    fn record_frame_float_roundtrip() {
        let mut reg = DynamicProtoRegistry::new();
        let (schema, data) = reg.record_frame("sensors/x", 111, "2.5").unwrap();
        let msg = decode_back(&schema, &data, "mqtt.SensorsX");
        assert_eq!(msg.get_field_by_name("t_ns").unwrap().as_i64(), Some(111));
        assert_eq!(msg.get_field_by_name("value").unwrap().as_f64(), Some(2.5));
    }

    #[test]
    fn record_frame_int_bool_text() {
        let mut reg = DynamicProtoRegistry::new();
        let (s1, d1) = reg.record_frame("a/count", 1, "7").unwrap();
        assert_eq!(decode_back(&s1, &d1, "mqtt.ACount").get_field_by_name("value").unwrap().as_i64(), Some(7));

        let (s2, d2) = reg.record_frame("a/flag", 2, "true").unwrap();
        assert_eq!(decode_back(&s2, &d2, "mqtt.AFlag").get_field_by_name("value").unwrap().as_bool(), Some(true));

        let (s3, d3) = reg.record_frame("a/msg", 3, "hi").unwrap();
        assert_eq!(
            decode_back(&s3, &d3, "mqtt.AMsg").get_field_by_name("value").unwrap().as_str(),
            Some("hi")
        );
    }

    #[test]
    fn locked_type_rejects_mismatch() {
        let mut reg = DynamicProtoRegistry::new();
        // First sample locks Int.
        assert!(reg.record_frame("t/n", 1, "5").is_some());
        // A non-numeric payload no longer matches the locked Int type.
        assert!(reg.record_frame("t/n", 2, "notanint").is_none());
    }

    #[test]
    fn bool_topic_records_numeric_after_textual_lock() {
        let mut reg = DynamicProtoRegistry::new();
        // Textual first sample locks Bool.
        assert!(reg.record_frame("t/b", 1, "on").is_some());
        // Later numeric 1/0 still encodes as bool.
        let (s, d) = reg.record_frame("t/b", 2, "0").unwrap();
        assert_eq!(decode_back(&s, &d, "mqtt.TB").get_field_by_name("value").unwrap().as_bool(), Some(false));
    }

    #[test]
    fn name_collision_disambiguated() {
        let mut reg = DynamicProtoRegistry::new();
        // Both sanitize to "AB".
        let (s1, _) = reg.record_frame("a/b", 1, "1").unwrap();
        let (s2, _) = reg.record_frame("a.b", 2, "1").unwrap();
        // Second topic's schema must define a differently-named message.
        let pool2 = prost_reflect::DescriptorPool::decode(s2.as_ref()).unwrap();
        assert!(pool2.get_message_by_name("mqtt.AB").is_none());
        assert!(pool2.get_message_by_name("mqtt.AB_2").is_some());
        // First topic keeps the base name.
        let pool1 = prost_reflect::DescriptorPool::decode(s1.as_ref()).unwrap();
        assert!(pool1.get_message_by_name("mqtt.AB").is_some());
    }
}
