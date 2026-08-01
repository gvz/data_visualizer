use eframe::egui;

use crate::script::runner::ScriptState;
use crate::script::{ScriptStatus, SharedStatus};

/// A requested enable/disable from a checkbox click.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelToggle {
    pub name: String,
    pub enable: bool,
}

/// Draw the script list. `available` is every `*.py` stem found in the scripts
/// dir; `enabled` is the currently-active set. Returns any checkbox toggles.
pub fn draw_script_panel(
    ui: &mut egui::Ui,
    available: &[String],
    enabled: &[String],
    status: &SharedStatus,
    disabled: &Option<String>,
) -> Vec<PanelToggle> {
    let mut toggles = Vec::new();
    ui.heading("Scripts");
    if let Some(reason) = disabled {
        ui.colored_label(egui::Color32::from_rgb(0xB0, 0x60, 0x00), reason);
        return toggles;
    }
    let states = status.lock().unwrap().clone();
    for name in available {
        let mut on = enabled.iter().any(|e| e == name);
        if ui.checkbox(&mut on, name).changed() {
            toggles.push(PanelToggle { name: name.clone(), enable: on });
        }
        if let Some(s) = states.iter().find(|s| &s.name == name) {
            ui.small(status_line(s));
        }
    }
    toggles
}

fn status_line(s: &ScriptStatus) -> String {
    match &s.state {
        ScriptState::Healthy => "  ● running".to_string(),
        ScriptState::Waiting(what) => format!("  ○ waiting for {what}"),
        ScriptState::Failed(msg) => format!("  ✗ {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_formats_each_state() {
        let mk = |st| ScriptStatus { name: "x".into(), state: st };
        assert!(status_line(&mk(ScriptState::Healthy)).contains("running"));
        assert!(status_line(&mk(ScriptState::Waiting("in.a".into()))).contains("in.a"));
        assert!(status_line(&mk(ScriptState::Failed("boom".into()))).contains("boom"));
    }
}
