//! Data-source modal: form logic lifted from `data_source_window.rs` (egui
//! widgets replaced with iced). Provides [`AddSourceForm`], the `view`
//! function that renders the modal body, and the `rows` helper used by tests.

use std::path::Path;

use iced::widget::{button, column, container, pick_list, row, text, text_input};
use iced::{Color, Element, Length};

use crate::ingest::{SourceKind, SourceManager, SourceSpec, CONNECTING, LIVE, TIMEOUT};

use super::message::{Message, SourceField};

// ── Form ─────────────────────────────────────────────────────────────────────

/// Transient state for the "Add source" form inside the Data Sources modal.
#[derive(Default)]
pub struct AddSourceForm {
    /// Which kind of source the user is configuring.
    pub kind: SourceKind,
    // ZMQ fields
    pub endpoint: String,
    pub schema: String,
    // MQTT fields
    pub broker: String,
    pub client_id: String,
    // WebSocket field
    pub listen: String,
    // Bridge fields
    pub name: String,
    pub command: String,
    /// Space/newline-separated argument string; split on whitespace before use.
    pub args: String,
    /// Last validation or spawn error to display inline.
    pub last_error: Option<String>,
}

impl AddSourceForm {
    /// Build a [`SourceSpec`] from the current form values, or `Err(message)`
    /// if a required field is blank.
    pub fn to_spec(&self) -> Result<SourceSpec, String> {
        match self.kind {
            SourceKind::Zmq => {
                if self.endpoint.trim().is_empty() {
                    return Err("endpoint required".into());
                }
                if self.schema.trim().is_empty() {
                    return Err("schema required".into());
                }
                Ok(SourceSpec::Zmq {
                    endpoint: self.endpoint.trim().to_string(),
                    schema: self.schema.trim().to_string(),
                })
            }
            SourceKind::Mqtt => {
                if self.broker.trim().is_empty() {
                    return Err("broker required".into());
                }
                let client_id = if self.client_id.trim().is_empty() {
                    "datavis".to_string()
                } else {
                    self.client_id.trim().to_string()
                };
                Ok(SourceSpec::Mqtt {
                    broker: self.broker.trim().to_string(),
                    client_id,
                })
            }
            SourceKind::WebSocket => {
                if self.listen.trim().is_empty() {
                    return Err("listen address required".into());
                }
                Ok(SourceSpec::WebSocket { listen: self.listen.trim().to_string() })
            }
            SourceKind::Bridge => {
                if self.name.trim().is_empty() {
                    return Err("bridge name required".into());
                }
                if self.command.trim().is_empty() {
                    return Err("command required".into());
                }
                let args: Vec<String> =
                    self.args.split_whitespace().map(|s| s.to_string()).collect();
                Ok(SourceSpec::Bridge(crate::ingest::bridge::BridgeConfig {
                    name: self.name.trim().to_string(),
                    command: self.command.trim().to_string(),
                    args,
                }))
            }
            SourceKind::Internal => Err("internal sources cannot be added manually".into()),
        }
    }

    /// Reset all form fields to their defaults (retains `kind`).
    pub fn clear(&mut self) {
        let kind = self.kind;
        *self = Self::default();
        self.kind = kind;
    }
}

// ── Persistence helper ────────────────────────────────────────────────────────

/// Rebuild a [`SourcesConfig`] from the manager's persistent sources and save
/// it to `path`, merging into the existing file content.
pub fn persist(path: &Path, manager: &SourceManager) -> Result<(), String> {
    let mut cfg = crate::config::SourcesConfig::default();
    for s in manager.sources().iter().filter(|s| s.persist) {
        if let Some(spec) = s.spec() {
            cfg.push(spec);
        }
    }
    cfg.save(path).map_err(|e| e.to_string())
}

// ── Row data (unit-testable without iced) ────────────────────────────────────

/// A single row in the active-sources list: id, label, and connection state.
pub struct SourceRow {
    pub id: u64,
    pub label: String,
    pub conn_state: u8,
}

/// Returns only the user-visible (non-Internal) sources from `manager`.
///
/// Internal sources have `spec() == None`; they are adopted by the manager
/// but are never user-configurable and must not appear in the data-source list.
pub fn rows(manager: &SourceManager) -> Vec<SourceRow> {
    manager
        .sources()
        .iter()
        .filter(|s| s.spec().is_some())
        .map(|s| SourceRow { id: s.id, label: s.label.clone(), conn_state: s.conn_state() })
        .collect()
}

// ── Widget helpers ────────────────────────────────────────────────────────────

fn conn_color(state: u8) -> Color {
    match state {
        LIVE => Color::from_rgb(0.0, 0.8, 0.0),
        CONNECTING => Color::from_rgb(0.9, 0.8, 0.0),
        TIMEOUT => Color::from_rgb(0.9, 0.1, 0.1),
        _ => Color::from_rgb(0.5, 0.5, 0.5),
    }
}

fn kind_label(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Zmq => "ZMQ",
        SourceKind::Mqtt => "MQTT",
        SourceKind::WebSocket => "WebSocket",
        SourceKind::Bridge => "Bridge",
        SourceKind::Internal => "Internal",
    }
}

// ── SourceKind pick_list support ──────────────────────────────────────────────

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(kind_label(*self))
    }
}

// ── View ──────────────────────────────────────────────────────────────────────

/// Render the data-source modal body (active sources + add-source form).
pub fn view<'a>(form: &'a AddSourceForm, manager: &'a SourceManager) -> Element<'a, Message> {
    // ── Active sources list ───────────────────────────────────────────────
    let sources_section: Element<'_, Message> = {
        let source_rows: Vec<SourceRow> = rows(manager);
        if source_rows.is_empty() {
            text("No active sources.").into()
        } else {
            let mut rows_col = column![].spacing(4);
            for sr in source_rows {
                let dot = text("●").color(conn_color(sr.conn_state));
                let lbl = text(sr.label);
                let stop_btn =
                    button(text("Stop")).on_press(Message::StopSource(sr.id));
                let r = row![dot, lbl, stop_btn].spacing(8).align_y(iced::Alignment::Center);
                rows_col = rows_col.push(r);
            }
            rows_col.into()
        }
    };

    // ── Kind picker ───────────────────────────────────────────────────────
    let kind_options: Vec<SourceKind> =
        vec![SourceKind::Zmq, SourceKind::Mqtt, SourceKind::WebSocket, SourceKind::Bridge];
    let kind_picker = pick_list(kind_options, Some(form.kind), Message::SourceKindSelected);

    // ── Per-kind input fields ─────────────────────────────────────────────
    let kind_fields: Element<'_, Message> = match form.kind {
        SourceKind::Zmq => {
            let ep = row![
                text("Endpoint:").width(Length::Fixed(120.0)),
                text_input("tcp://host:port", &form.endpoint)
                    .on_input(|v| Message::SourceField(SourceField::Endpoint, v))
            ]
            .spacing(6);
            let schema_browse = button(text("Browse…"))
                .on_press(Message::BrowseSchema);
            let sc = row![
                text("Schema (.proto):").width(Length::Fixed(120.0)),
                text_input("path/to/schema.proto", &form.schema)
                    .on_input(|v| Message::SourceField(SourceField::Schema, v)),
                schema_browse,
            ]
            .spacing(6);
            column![ep, sc].spacing(6).into()
        }
        SourceKind::Mqtt => {
            let br = row![
                text("Broker:").width(Length::Fixed(120.0)),
                text_input("host:1883", &form.broker)
                    .on_input(|v| Message::SourceField(SourceField::Broker, v))
            ]
            .spacing(6);
            let ci = row![
                text("Client ID:").width(Length::Fixed(120.0)),
                text_input("datavis", &form.client_id)
                    .on_input(|v| Message::SourceField(SourceField::ClientId, v)),
                text("(default: datavis)"),
            ]
            .spacing(6);
            column![br, ci].spacing(6).into()
        }
        SourceKind::WebSocket => {
            let li = row![
                text("Listen address:").width(Length::Fixed(120.0)),
                text_input("0.0.0.0:9001", &form.listen)
                    .on_input(|v| Message::SourceField(SourceField::Listen, v)),
                text("e.g. 0.0.0.0:9001"),
            ]
            .spacing(6);
            column![li].spacing(6).into()
        }
        SourceKind::Bridge => {
            let nm = row![
                text("Name:").width(Length::Fixed(120.0)),
                text_input("my-bridge", &form.name)
                    .on_input(|v| Message::SourceField(SourceField::BridgeName, v))
            ]
            .spacing(6);
            let cmd_browse = button(text("Browse…"))
                .on_press(Message::BrowseCommand);
            let cm = row![
                text("Command:").width(Length::Fixed(120.0)),
                text_input("/path/to/binary", &form.command)
                    .on_input(|v| Message::SourceField(SourceField::Command, v)),
                cmd_browse,
            ]
            .spacing(6);
            let ar = row![
                text("Args:").width(Length::Fixed(120.0)),
                text_input("--flag value", &form.args)
                    .on_input(|v| Message::SourceField(SourceField::Args, v)),
                text("(space-separated)"),
            ]
            .spacing(6);
            column![nm, cm, ar].spacing(6).into()
        }
        SourceKind::Internal => column![].into(),
    };

    // ── Error display ─────────────────────────────────────────────────────
    let error_row: Element<'_, Message> = if let Some(err) = &form.last_error {
        text(format!("Error: {err}")).color(Color::from_rgb(0.9, 0.1, 0.1)).into()
    } else {
        text("").into()
    };

    // ── Start button ──────────────────────────────────────────────────────
    let start_btn = button(text("Start")).on_press(Message::StartSource);

    // ── Assemble the full modal card ──────────────────────────────────────
    let card = column![
        text("Active Sources").size(18),
        sources_section,
        text("Add Source").size(18),
        row![text("Type:"), kind_picker].spacing(8),
        kind_fields,
        error_row,
        start_btn,
    ]
    .spacing(10)
    .padding(16)
    .width(Length::Fixed(540.0));

    container(card)
        .style(|theme: &iced::Theme| {
            let palette = theme.extended_palette();
            container::Style {
                background: Some(palette.background.base.color.into()),
                border: iced::Border {
                    color: palette.background.strong.color,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..container::Style::default()
            }
        })
        .into()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{SourceKind, SourceSpec};

    #[test]
    fn zmq_form_requires_endpoint_and_schema() {
        let mut f = AddSourceForm { kind: SourceKind::Zmq, ..Default::default() };
        assert!(f.to_spec().is_err());
        f.endpoint = "tcp://x:1".into();
        f.schema = "s.proto".into();
        assert_eq!(
            f.to_spec().unwrap(),
            SourceSpec::Zmq { endpoint: "tcp://x:1".into(), schema: "s.proto".into() }
        );
    }

    #[test]
    fn mqtt_defaults_client_id_when_blank() {
        let f = AddSourceForm {
            kind: SourceKind::Mqtt,
            broker: "h:1883".into(),
            ..Default::default()
        };
        assert_eq!(
            f.to_spec().unwrap(),
            SourceSpec::Mqtt { broker: "h:1883".into(), client_id: "datavis".into() }
        );
    }

    #[test]
    fn bridge_splits_args() {
        let f = AddSourceForm {
            kind: SourceKind::Bridge,
            name: "n".into(),
            command: "/c".into(),
            args: "--a  --b".into(),
            ..Default::default()
        };
        match f.to_spec().unwrap() {
            SourceSpec::Bridge(cfg) => assert_eq!(cfg.args, vec!["--a", "--b"]),
            _ => panic!(),
        }
    }

    #[test]
    fn rows_hides_internal_sources() {
        use std::sync::Arc;
        use crate::config::ChannelRegistry;
        use crate::store::LiveStore;
        use crate::ingest::source::SourceHandle;
        use std::sync::atomic::{AtomicBool, AtomicU8};
        use std::sync::Mutex;

        let channels = Arc::new(ChannelRegistry::from_toml_str("").unwrap());
        let store = Arc::new(LiveStore::from_registry(&channels));
        let mut manager = SourceManager::new(store.clone(), channels.clone());

        // Adopt an internal source (no spec).
        let internal_handle = SourceHandle {
            name: "scripting".to_string(),
            conn_state: Arc::new(AtomicU8::new(crate::ingest::CONNECTING)),
            record_sender: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            child_guard: None,
            schema_bytes: None,
            discovery: None,
        };
        manager.adopt(internal_handle, "scripting".to_string());

        // Verify rows() hides the internal source.
        let visible = rows(&manager);
        assert_eq!(
            visible.len(),
            0,
            "Internal source must be hidden: got {} rows",
            visible.len()
        );
    }
}
