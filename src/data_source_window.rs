//! "Data Sources" floating window — list running sources, stop them, and add
//! new ones.  Opened from the menu-bar button added in `app.rs`.

use std::path::Path;

use eframe::egui;

use crate::ingest::{SourceKind, SourceManager, SourceSpec, CONNECTING, LIVE, TIMEOUT};

// ── Form ─────────────────────────────────────────────────────────────────────

/// Transient state for the "Add source" form inside the Data Sources window.
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
    fn clear(&mut self) {
        let kind = self.kind;
        *self = Self::default();
        self.kind = kind;
    }
}

// ── Persistence helper ────────────────────────────────────────────────────────

/// Rebuild a [`SourcesConfig`] from the manager's persistent sources and save
/// it to `path`, merging into the existing file content.
fn persist(path: &Path, manager: &SourceManager) -> Result<(), String> {
    let mut cfg = crate::config::SourcesConfig::default();
    for s in manager.sources().iter().filter(|s| s.persist) {
        if let Some(spec) = s.spec() {
            cfg.push(spec);
        }
    }
    cfg.save(path).map_err(|e| e.to_string())
}

// ── Window ────────────────────────────────────────────────────────────────────

/// Render the Data Sources window.
///
/// Call every frame from `DataVisApp::update` after the central panel.  When
/// `*open` is `false` the function returns immediately.  Uses an
/// [`egui::Window`] (floating panel inside the main viewport) which is the
/// reliable approach under eframe 0.28; the deferred-viewport close-handling
/// API in that version is fiddly and not worth the complexity for this use
/// case.
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    manager: &mut SourceManager,
    form: &mut AddSourceForm,
    config_path: &Path,
) {
    egui::Window::new("Data Sources")
        .open(open)
        .default_size([520.0, 420.0])
        .resizable(true)
        .show(ctx, |ui| {
            show_contents(ui, manager, form, config_path);
        });
}

fn show_contents(
    ui: &mut egui::Ui,
    manager: &mut SourceManager,
    form: &mut AddSourceForm,
    config_path: &Path,
) {
    // ── Active sources list ───────────────────────────────────────────────
    ui.heading("Active sources");
    ui.separator();

    let sources: Vec<(u64, String, u8)> = manager
        .sources()
        .iter()
        .filter(|s| s.spec().is_some()) // hide Internal / spec-less
        .map(|s| (s.id, s.label.clone(), s.conn_state()))
        .collect();

    let mut stop_id: Option<u64> = None;

    if sources.is_empty() {
        ui.label("No active sources.");
    } else {
        egui::Grid::new("sources_grid")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Source");
                ui.strong("Status");
                ui.label(""); // Stop column header (empty)
                ui.end_row();

                for (id, label, state) in &sources {
                    ui.label(label);
                    let (dot, color) = conn_dot(*state);
                    ui.colored_label(color, dot);
                    if ui.button("Stop").clicked() {
                        stop_id = Some(*id);
                    }
                    ui.end_row();
                }
            });
    }

    if let Some(id) = stop_id {
        manager.remove(id);
        if let Err(e) = persist(config_path, manager) {
            form.last_error = Some(format!("persist error: {e}"));
        }
    }

    ui.add_space(12.0);

    // ── Add source form ───────────────────────────────────────────────────
    ui.heading("Add source");
    ui.separator();

    // Kind selector
    ui.horizontal(|ui| {
        ui.label("Type:");
        egui::ComboBox::from_id_source("kind_selector")
            .selected_text(kind_label(form.kind))
            .show_ui(ui, |ui| {
                for k in [
                    SourceKind::Zmq,
                    SourceKind::Mqtt,
                    SourceKind::WebSocket,
                    SourceKind::Bridge,
                ] {
                    ui.selectable_value(&mut form.kind, k, kind_label(k));
                }
            });
    });

    ui.add_space(4.0);

    // Per-kind fields
    match form.kind {
        SourceKind::Zmq => {
            ui.horizontal(|ui| {
                ui.label("Endpoint:");
                ui.text_edit_singleline(&mut form.endpoint);
            });
            ui.horizontal(|ui| {
                ui.label("Schema (.proto):");
                ui.text_edit_singleline(&mut form.schema);
                if ui.button("Browse…").clicked() {
                    if let Some(p) =
                        rfd::FileDialog::new().add_filter("Proto schema", &["proto"]).pick_file()
                    {
                        form.schema = p.display().to_string();
                    }
                }
            });
        }
        SourceKind::Mqtt => {
            ui.horizontal(|ui| {
                ui.label("Broker:");
                ui.text_edit_singleline(&mut form.broker);
            });
            ui.horizontal(|ui| {
                ui.label("Client ID:");
                ui.text_edit_singleline(&mut form.client_id);
                ui.weak("(default: datavis)");
            });
        }
        SourceKind::WebSocket => {
            ui.horizontal(|ui| {
                ui.label("Listen address:");
                ui.text_edit_singleline(&mut form.listen);
                ui.weak("e.g. 0.0.0.0:9001");
            });
        }
        SourceKind::Bridge => {
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut form.name);
            });
            ui.horizontal(|ui| {
                ui.label("Command:");
                ui.text_edit_singleline(&mut form.command);
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_file() {
                        form.command = p.display().to_string();
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Args:");
                ui.text_edit_singleline(&mut form.args);
                ui.weak("(space-separated)");
            });
        }
        SourceKind::Internal => {}
    }

    ui.add_space(6.0);

    // Error display
    if let Some(ref err) = form.last_error.clone() {
        ui.colored_label(egui::Color32::RED, format!("Error: {err}"));
    }

    // Start button
    if ui.button("Start").clicked() {
        match form.to_spec() {
            Ok(spec) => match manager.add(&spec, true) {
                Ok(_) => {
                    if let Err(e) = persist(config_path, manager) {
                        form.last_error = Some(format!("persist error: {e}"));
                    } else {
                        form.last_error = None;
                        form.clear();
                    }
                }
                Err(e) => {
                    form.last_error = Some(e.to_string());
                }
            },
            Err(msg) => {
                form.last_error = Some(msg);
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn conn_dot(state: u8) -> (&'static str, egui::Color32) {
    match state {
        LIVE => ("●", egui::Color32::GREEN),
        CONNECTING => ("●", egui::Color32::YELLOW),
        TIMEOUT => ("●", egui::Color32::RED),
        _ => ("●", egui::Color32::GRAY),
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
}
