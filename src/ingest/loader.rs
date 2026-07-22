use std::path::Path;

use anyhow::{anyhow, Context};
use prost_reflect::{DescriptorPool, MessageDescriptor};

pub fn parse_field_path(path: &str) -> anyhow::Result<(String, Vec<String>)> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() < 2 {
        return Err(anyhow!(
            "field path {:?} must have at least 2 dot-separated segments (MessageType.field)",
            path
        ));
    }
    let msg_name = parts[0].to_string();
    let field_steps: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
    Ok((msg_name, field_steps))
}

pub struct ChannelDesc {
    pub msg_desc: MessageDescriptor,
    /// Field steps after the message type name, e.g. ["samples", "x"].
    pub val_path: Vec<String>,
    /// Field steps for timestamp, e.g. ["samples", "t_ns"].
    pub ts_path: Vec<String>,
}

pub struct ProtoSchema {
    pool: DescriptorPool,
    schema_bytes: Vec<u8>,
}

impl ProtoSchema {
    pub fn from_path(proto_file: &Path) -> anyhow::Result<Self> {
        use prost_reflect::prost::Message as _;
        let include_dir = proto_file.parent().unwrap_or(Path::new("."));
        let fds = protox::compile([proto_file], [include_dir])
            .with_context(|| format!("compiling proto schema {}", proto_file.display()))?;
        let schema_bytes = fds.encode_to_vec();
        let pool = DescriptorPool::from_file_descriptor_set(fds)
            .context("building descriptor pool from compiled schema")?;
        Ok(Self { pool, schema_bytes })
    }

    pub fn resolve(&self, proto_path: &str, ts_path: &str) -> anyhow::Result<ChannelDesc> {
        let (val_msg, val_steps) = parse_field_path(proto_path)?;
        let (ts_msg, ts_steps) = parse_field_path(ts_path)?;
        if val_msg != ts_msg {
            return Err(anyhow!(
                "proto_path and ts_path must reference the same message type: \
                 proto_path uses {:?} but ts_path uses {:?}",
                val_msg,
                ts_msg
            ));
        }
        let msg_desc = self
            .pool
            .get_message_by_name(&val_msg)
            .ok_or_else(|| anyhow!("message type {:?} not found in proto schema", val_msg))?;
        Ok(ChannelDesc { msg_desc, val_path: val_steps, ts_path: ts_steps })
    }

    pub fn schema_bytes(&self) -> &[u8] {
        &self.schema_bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        use prost_reflect::prost::Message as _;
        let fds = prost_types::FileDescriptorSet::decode(bytes)
            .context("decoding FileDescriptorSet from schema bytes")?;
        let schema_bytes = bytes.to_vec();
        let pool = DescriptorPool::from_file_descriptor_set(fds)
            .context("building descriptor pool from schema bytes")?;
        Ok(Self { pool, schema_bytes })
    }

    /// Build a schema pool by merging multiple encoded FileDescriptorSets (e.g.
    /// the per-channel schemas embedded in an MCAP file). Files with duplicate
    /// names are skipped (prost-reflect dedups). Self-contained files (no imports)
    /// may be added in any order. An individual set that fails to decode or add is
    /// logged and skipped, so one malformed channel schema does not make the
    /// recording unopenable.
    pub fn from_descriptor_sets(sets: &[&[u8]]) -> Self {
        use prost_reflect::prost::Message as _;
        let mut pool = DescriptorPool::new();
        for bytes in sets {
            match prost_types::FileDescriptorSet::decode(*bytes) {
                Ok(fds) => {
                    if let Err(e) = pool.add_file_descriptor_set(fds) {
                        eprintln!("replay: skipping embedded schema: {e}");
                    }
                }
                Err(e) => eprintln!("replay: skipping undecodable embedded schema: {e}"),
            }
        }
        let schema_bytes = pool.encode_to_vec();
        Self { pool, schema_bytes }
    }

    /// Look up a message descriptor by fully-qualified name.
    pub fn message_by_name(&self, name: &str) -> Option<MessageDescriptor> {
        self.pool.get_message_by_name(name)
    }

    #[cfg(test)]
    pub fn pool_for_test(&self) -> &DescriptorPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_test_proto(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{}", content).unwrap();
        path
    }

    #[test]
    fn parse_two_segments() {
        let (msg, path) = parse_field_path("FlatMsg.value").unwrap();
        assert_eq!(msg, "FlatMsg");
        assert_eq!(path, vec!["value"]);
    }

    #[test]
    fn parse_three_segments() {
        let (msg, path) = parse_field_path("AccelBatch.samples.x").unwrap();
        assert_eq!(msg, "AccelBatch");
        assert_eq!(path, vec!["samples", "x"]);
    }

    #[test]
    fn parse_one_segment_is_err() {
        assert!(parse_field_path("NoField").is_err());
    }

    #[test]
    fn parse_empty_is_err() {
        assert!(parse_field_path("").is_err());
    }

    #[test]
    fn schema_loads_and_resolves_batch_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), r#"
syntax = "proto3";
message AccelBatch {
  repeated Sample samples = 1;
  message Sample {
    int64 t_ns = 1;
    float x = 2;
  }
}
"#);
        let schema = ProtoSchema::from_path(&path).unwrap();
        let desc = schema.resolve("AccelBatch.samples.x", "AccelBatch.samples.t_ns").unwrap();
        assert_eq!(desc.val_path, vec!["samples", "x"]);
        assert_eq!(desc.ts_path, vec!["samples", "t_ns"]);
        assert_eq!(desc.msg_desc.name(), "AccelBatch");
    }

    #[test]
    fn schema_loads_and_resolves_flat_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), r#"
syntax = "proto3";
message FlatMsg {
  int64 t_ns = 1;
  float value = 2;
}
"#);
        let schema = ProtoSchema::from_path(&path).unwrap();
        let desc = schema.resolve("FlatMsg.value", "FlatMsg.t_ns").unwrap();
        assert_eq!(desc.val_path, vec!["value"]);
        assert_eq!(desc.ts_path, vec!["t_ns"]);
    }

    #[test]
    fn resolve_unknown_message_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), "syntax = \"proto3\";\n");
        let schema = ProtoSchema::from_path(&path).unwrap();
        assert!(schema.resolve("NoSuchMsg.x", "NoSuchMsg.t").is_err());
    }

    #[test]
    fn resolve_mismatched_message_names_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), r#"
syntax = "proto3";
message A { int64 t = 1; float v = 2; }
message B { int64 t = 1; float v = 2; }
"#);
        let schema = ProtoSchema::from_path(&path).unwrap();
        assert!(schema.resolve("A.v", "B.t").is_err());
    }

    #[test]
    fn from_path_nonexistent_file_is_err() {
        assert!(ProtoSchema::from_path(std::path::Path::new("/nonexistent/schema.proto")).is_err());
    }

    #[test]
    fn schema_bytes_is_valid_file_descriptor_set() {
        use prost_reflect::prost::Message as _;
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), r#"
syntax = "proto3";
message Ping { int64 t_ns = 1; float v = 2; }
"#);
        let schema = ProtoSchema::from_path(&path).unwrap();
        let bytes = schema.schema_bytes();
        assert!(!bytes.is_empty());
        // Must decode back to a valid FileDescriptorSet.
        let fds = prost_types::FileDescriptorSet::decode(bytes).unwrap();
        assert!(!fds.file.is_empty());
    }

    #[test]
    fn from_descriptor_sets_merges_and_resolves() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let pa = dir.path().join("a.proto");
        let pb = dir.path().join("b.proto");
        write!(std::fs::File::create(&pa).unwrap(),
            "syntax = \"proto3\";\nmessage MsgA {{ int64 t = 1; double v = 2; }}\n").unwrap();
        write!(std::fs::File::create(&pb).unwrap(),
            "syntax = \"proto3\";\nmessage MsgB {{ int64 t = 1; bool v = 2; }}\n").unwrap();
        let sa = ProtoSchema::from_path(&pa).unwrap();
        let sb = ProtoSchema::from_path(&pb).unwrap();

        let merged = ProtoSchema::from_descriptor_sets(&[sa.schema_bytes(), sb.schema_bytes()]);
        assert!(merged.message_by_name("MsgA").is_some());
        assert!(merged.message_by_name("MsgB").is_some());
        assert!(merged.message_by_name("MsgC").is_none());
    }

    #[test]
    fn from_descriptor_sets_skips_garbage() {
        // A garbage set is skipped; a valid one still resolves.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let pa = dir.path().join("a.proto");
        write!(std::fs::File::create(&pa).unwrap(),
            "syntax = \"proto3\";\nmessage MsgA {{ int64 t = 1; double v = 2; }}\n").unwrap();
        let sa = ProtoSchema::from_path(&pa).unwrap();
        let garbage: &[u8] = b"not a descriptor set";
        let merged = ProtoSchema::from_descriptor_sets(&[garbage, sa.schema_bytes()]);
        assert!(merged.message_by_name("MsgA").is_some());
    }
}
