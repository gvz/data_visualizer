//! Out-of-process ingest: spawn an organization's proprietary adapter and read
//! a fixed columnar Protobuf `Batch` off its stdout. See
//! `docs/superpowers/specs/2026-08-03-proprietary-source-bridge-design.md`.

pub mod config;
pub mod frame;
pub mod router;
pub mod schema;
