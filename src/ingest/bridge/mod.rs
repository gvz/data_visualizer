//! Out-of-process ingest: spawn an organization's proprietary adapter and read
//! a fixed columnar Protobuf `Batch` off its stdout. See
//! `docs/superpowers/specs/2026-08-03-proprietary-source-bridge-design.md`.
//!
//! The wire contract is documented in `docs/external-source-protocol.md`; the
//! reference adapter is `examples/echo_bridge.rs`.

pub mod config;
pub mod frame;
pub mod router;
pub mod schema;
pub mod source;

pub use config::BridgeConfig;
pub use source::SubprocessSource;
