//! The fixed columnar wire schema shared with external bridge processes.
//!
//! Generated at build time from `proto/batch.proto` (see `build.rs`). The
//! payload is decoded directly into these `prost` structs — no reflection.

/// Generated `prost` types for the `datavis.bridge` package.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/datavis.bridge.rs"));
}

/// Serialized `FileDescriptorSet` for the `Batch` schema, embedded into the
/// MCAP recording header (mirrors `ZmqSource::schema_bytes`).
pub fn batch_schema_bytes() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/batch.fds"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn batch_round_trips_through_prost() {
        let batch = pb::Batch {
            cols: vec![pb::Column {
                topic: "accel".to_string(),
                t_ns: vec![1_000, 2_000],
                values: Some(pb::column::Values::Doubles(pb::DoubleCol {
                    v: vec![1.5, 2.5],
                })),
            }],
        };
        let bytes = batch.encode_to_vec();
        let decoded = pb::Batch::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.cols.len(), 1);
        assert_eq!(decoded.cols[0].topic, "accel");
        assert_eq!(decoded.cols[0].t_ns, vec![1_000, 2_000]);
        match &decoded.cols[0].values {
            Some(pb::column::Values::Doubles(d)) => assert_eq!(d.v, vec![1.5, 2.5]),
            other => panic!("wrong oneof variant: {other:?}"),
        }
    }

    #[test]
    fn schema_bytes_are_non_empty() {
        assert!(!batch_schema_bytes().is_empty());
    }
}
