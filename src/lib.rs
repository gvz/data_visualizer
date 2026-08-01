//! # datavis
//!
//! A real-time channel data visualizer built with eframe/egui. Channels
//! carry timestamped scalar or text samples from one or more live sources;
//! panels render those samples in various ways inside a tiled workspace.
//!
//! This document covers the main extension points: adding a new
//! **visualization panel**, adding a new **data source**, and writing a
//! **Python script** that derives new channels from existing ones.
//!
//! ---
//!
//! ## Adding a visualization panel
//!
//! A panel is any type that implements [`viz::VizPanel`]. The trait is defined
//! in [`viz`] alongside all built-in panels (waveform, gauge, numeric, …).
//!
//! ### Minimal checklist
//!
//! 1. Create `src/viz/my_panel.rs`.
//! 2. Implement [`viz::VizPanel`]:
//!    - [`title`] — label shown in the pane header.
//!    - [`accepted_types`] — which [`types::SampleType`]s the panel accepts
//!      (gates drag-and-drop).
//!    - [`config_ui`] — egui controls rendered in the settings popup.
//!    - [`render`] — draw the panel; read samples via [`store::ChannelStore`].
//!    - [`serialize`] / constructor — round-trip panel state through
//!      `toml::Table` for layout persistence. Binding errors (unknown channel,
//!      wrong type) must produce an inline error panel, **not** `Err`.
//! 3. Expose `pub const TYPE_NAME: &str` and `pub fn ctor(…)` matching
//!    [`viz::PanelCtor`].
//! 4. Register both in [`viz::PanelRegistry::with_builtins`]
//!    (`src/viz/mod.rs`).
//!
//! See [`viz`] for the full trait contract and any of the existing panels
//! (e.g. `src/viz/numeric.rs`, `src/viz/waveform.rs`) for a concrete
//! reference.
//!
//! [`title`]: viz::VizPanel::title
//! [`accepted_types`]: viz::VizPanel::accepted_types
//! [`config_ui`]: viz::VizPanel::config_ui
//! [`render`]: viz::VizPanel::render
//! [`serialize`]: viz::VizPanel::serialize
//!
//! ---
//!
//! ## Adding a data source
//!
//! A source is any type that implements [`ingest::DataSource`]. The trait is
//! defined in [`ingest::source`] alongside the built-in sources (ZMQ, MQTT,
//! WebSocket).
//!
//! ### Minimal checklist
//!
//! 1. Create `src/ingest/my_source.rs`.
//! 2. Define a config struct (see [`ingest::MqttConfig`] or
//!    [`ingest::WsConfig`] for examples).
//! 3. Implement [`ingest::DataSource`]:
//!    - [`name`] — short label for the status bar and logs.
//!    - [`spawn`] — allocate shared `conn_state` and `record_sender` `Arc`s,
//!      start the background receive thread, return a
//!      [`ingest::SourceHandle`]. Set [`ingest::LIVE`] on the `conn_state`
//!      once the first message arrives and [`ingest::TIMEOUT`] when the
//!      heartbeat window expires.
//! 4. If the source discovers topics at runtime (like MQTT), populate
//!    [`ingest::Discovery`] and include it in the handle so the sidebar
//!    channel picker can show discovered topics.
//! 5. Wire the new source into `src/main.rs` (parse its CLI flag, construct
//!    it, call `.spawn(store.clone())`, push the handle into `sources`).
//!
//! Samples are written to the store via [`store::ChannelStore::write_numeric`]
//! or [`store::ChannelStore::write_text`]. The store is `Arc<dyn ChannelStore>`
//! so both live and replay stores satisfy the same interface.
//!
//! See [`ingest`] for the full trait contract and `src/ingest/mqtt.rs` or
//! `src/ingest/websocket.rs` for concrete references.
//!
//! [`name`]: ingest::DataSource::name
//! [`spawn`]: ingest::DataSource::spawn
//!
//! ---
//!
//! ## Writing a Python script
//!
//! Scripts read channels, compute, and publish new channels at native speed —
//! each is compiled with numba when loaded. The engine is [`script::ScriptEngine`]
//! (a [`ingest::DataSource`] that ticks every script on a background thread);
//! numba is required, and the feature disables itself with a visible warning
//! when it is absent.
//!
//! Put `.py` files in a `scripts/` directory beside `config.toml` and enable
//! them from the **Scripts** sidebar panel. The active set persists in the
//! `[scripts]` table (`dir`, `enabled`, `window_s` — the seconds of history a
//! script sees each tick). See [`script::config::ScriptsConfig`].
//!
//! A script self-declares its bindings and provides a numba-compiled `compute`:
//!
//! ```python
//! import numpy as np
//! import numba
//!
//! INPUTS  = ["accel.x", "accel.y", "accel.z"]
//! OUTPUTS = [{"name": "accel.magnitude", "type": "float", "unit": "m/s2"}]
//!
//! @numba.njit
//! def compute(ts, vals):
//!     t = ts[0]                            # timestamps of accel.x (int64 ns)
//!     x, y, z = vals[0], vals[1], vals[2]  # one value array per input
//!     return (t, np.sqrt(x**2 + y**2 + z**2))
//! ```
//!
//! - `ts[i]` / `vals[i]` are the timestamp and value arrays of input `i` (in
//!   `INPUTS` order) over the last `window_s` seconds. They are **tuples of
//!   separate 1‑D arrays**, not a 2‑D array: each input keeps its own length
//!   and its own timestamps, so aligning different-rate inputs (e.g. with
//!   `np.interp`) is the script's job — the engine never resamples.
//! - Return **one `(ts, vals)` pair per output** (a bare pair for a single
//!   output, or a tuple of pairs in `OUTPUTS` order). Return arrays the length
//!   of `ts` for a per-sample transform, or length‑1 arrays for one reduced
//!   value per tick. Output `type` is `"float"`, `"int"`, or `"bool"`; keep
//!   arrays `int64`/`float64`.
//!
//! The engine appends only samples newer than the last it wrote, so overlapping
//! windows never duplicate output. A compile error or an exception in `compute`
//! fails only that script (shown in the Scripts panel); the app and other
//! scripts keep running. Outputs are ordinary channels — drop them onto any
//! panel like live data. See [`script`] for the full module.
//!
//! ---
//!
//! ## Key types
//!
//! | Type | Where | Role |
//! |------|-------|------|
//! | [`viz::VizPanel`] | `viz` | Trait every panel implements |
//! | [`viz::PanelRegistry`] | `viz` | Maps `type` strings → constructors |
//! | [`ingest::DataSource`] | `ingest` | Trait every source implements |
//! | [`ingest::SourceHandle`] | `ingest` | Uniform handle for a running source |
//! | [`store::ChannelStore`] | `store` | Read/write interface panels and sources share |
//! | [`types::SampleType`] | `types` | Float / Int / Bool / Text discriminant |
//! | [`config::ChannelRegistry`] | `config` | Channel name → id + metadata at startup |
//! | [`script::ScriptEngine`] | `script` | Runs numba-compiled user scripts as a data source |

pub mod app;
pub mod channel_tree;
pub mod config;
pub mod demo;
pub mod dynamic_channel;
pub mod ingest;
pub mod record;
pub mod script;
pub mod store;
pub mod types;
pub mod viz;
pub mod workspace;
