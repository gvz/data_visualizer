//! Reference "bridge": the minimal program datavis spawns. A real adapter
//! replaces the hard-coded samples with data from its proprietary transport,
//! but the framing below is the entire contract.
//!
//! Run standalone to inspect the bytes:
//!   cargo run --example echo_bridge | xxd | head

use std::io::{self, Write};

use datavis::ingest::bridge::frame::{MAGIC, VERSION};
use datavis::ingest::bridge::schema::pb;
use prost::Message;

fn write_frame<W: Write>(w: &mut W, batch: &pb::Batch) -> io::Result<()> {
    let body = batch.encode_to_vec();
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    Ok(())
}

fn main() -> io::Result<()> {
    let mut out = io::stdout().lock();

    // Preamble: once, at the very start of the stream.
    out.write_all(&MAGIC)?;
    out.write_all(&[VERSION])?;

    // One frame carrying three channels at different rates / types.
    let batch = pb::Batch {
        cols: vec![
            pb::Column {
                topic: "accel".into(),
                t_ns: vec![1_000, 2_000, 3_000],
                values: Some(pb::column::Values::Doubles(pb::DoubleCol {
                    v: vec![0.1, 0.2, 0.3],
                })),
            },
            pb::Column {
                topic: "state".into(),
                t_ns: vec![1_500],
                values: Some(pb::column::Values::Ints(pb::Sint64Col { v: vec![1] })),
            },
            pb::Column {
                topic: "log".into(),
                t_ns: vec![1_500],
                values: Some(pb::column::Values::Strings(pb::StringCol {
                    v: vec!["armed".into()],
                })),
            },
        ],
    };
    write_frame(&mut out, &batch)?;
    out.flush()?;
    Ok(())
}
