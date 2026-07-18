//! Generate a sample MCAP recording compatible with channels.toml.
//!
//! Run: cargo run --example gen_sample_mcap
//! Output: sample_recording.mcap in the current directory.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;

use prost_reflect::prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage, Kind, Value};

// Matches the proto_path / ts_path fields in channels.toml.
const PROTO_SCHEMA: &str = r#"
syntax = "proto3";

message DemoBatch {
  repeated Sample samples = 1;

  message Sample {
    int64  t_ns    = 1;
    float  sine    = 2;
    int64  counter = 3;
    bool   enabled = 4;
    string message = 5;
  }
}
"#;

fn main() -> anyhow::Result<()> {
    // Compile proto schema → FileDescriptorSet bytes.
    let dir = tempfile::tempdir()?;
    let proto_path = dir.path().join("demo.proto");
    std::fs::write(&proto_path, PROTO_SCHEMA)?;

    let fds = protox::compile([&proto_path], [dir.path()])?;
    let schema_bytes = fds.encode_to_vec();
    let pool = DescriptorPool::from_file_descriptor_set(fds)?;

    let batch_desc = pool
        .get_message_by_name("DemoBatch")
        .ok_or_else(|| anyhow::anyhow!("DemoBatch not found in pool"))?;

    let sample_desc = match batch_desc
        .get_field_by_name("samples")
        .ok_or_else(|| anyhow::anyhow!("no samples field on DemoBatch"))?
        .kind()
    {
        Kind::Message(d) => d,
        _ => anyhow::bail!("samples field is not a message type"),
    };

    // Open output MCAP file.
    let out_path = "sample_recording.mcap";
    let file = BufWriter::new(File::create(out_path)?);
    let mut writer = mcap::Writer::new(file)?;

    let mcap_schema = Arc::new(mcap::Schema {
        name: "protobuf".to_string(),
        encoding: "protobuf".to_string(),
        data: Cow::Owned(schema_bytes),
    });
    let channel_id = writer.add_channel(&mcap::Channel {
        topic: "demo".to_string(),
        schema: Some(mcap_schema),
        message_encoding: "protobuf".to_string(),
        metadata: BTreeMap::new(),
    })?;

    // 5 seconds at 100 Hz, 10 samples per batch → 50 batches.
    let start_ns: i64 = 1_750_000_000_000_000_000;
    let step_ns: i64 = 10_000_000; // 10 ms between samples
    let n_samples: i64 = 500;
    let batch_size: i64 = 10;
    let mut sequence = 0u32;

    for batch_idx in 0..(n_samples / batch_size) {
        let base = batch_idx * batch_size;
        let mut sample_vals = Vec::with_capacity(batch_size as usize);

        for i in base..base + batch_size {
            let ts_ns = start_ns + i * step_ns;
            let t = i as f64 / 100.0;

            let mut s = DynamicMessage::new(sample_desc.clone());
            s.set_field_by_name("t_ns", Value::I64(ts_ns));
            s.set_field_by_name(
                "sine",
                Value::F32((2.0 * std::f64::consts::PI * t).sin() as f32 * 10.0),
            );
            s.set_field_by_name("counter", Value::I64(i % 100));
            s.set_field_by_name("enabled", Value::Bool((i / 50) % 2 == 0));
            if i % 50 == 0 {
                s.set_field_by_name(
                    "message",
                    Value::String(format!("log line {batch_idx} (t={t:.2}s)")),
                );
            }
            sample_vals.push(Value::Message(s));
        }

        let mut batch = DynamicMessage::new(batch_desc.clone());
        batch.set_field_by_name("samples", Value::List(sample_vals));

        let payload = batch.encode_to_vec();
        let batch_ts = (start_ns + base * step_ns) as u64;

        writer.write_to_known_channel(
            &mcap::records::MessageHeader {
                channel_id,
                sequence,
                log_time: batch_ts,
                publish_time: batch_ts,
            },
            &payload,
        )?;
        sequence += 1;
    }

    writer.flush()?;
    writer.finish()?;

    println!("Wrote {out_path}");
    println!("  {n_samples} samples over 5s at 100Hz, topic: demo");
    println!("  Channels: sine (float ~±10), counter (int 0-99), enabled (bool), message (text)");

    Ok(())
}
