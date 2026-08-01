use eframe::egui;

use crate::script::config::ScriptInstance;
use crate::script::runner::ScriptState;
use crate::script::{ScriptStatus, SharedStatus};

/// A committed panel action for the app to forward to the engine / config.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelCommand {
    Upsert(ScriptInstance),
    Remove(String),
    SaveConfig,
}

/// Minimal instance list: per-instance enable toggle + status. The full editor
/// (add/remove, input/output binding) lands in a later task.
pub fn draw_script_panel(
    ui: &mut egui::Ui,
    instances: &[ScriptInstance],
    status: &SharedStatus,
    disabled: &Option<String>,
) -> Vec<PanelCommand> {
    let mut cmds = Vec::new();
    ui.heading("Scripts");
    if let Some(reason) = disabled {
        ui.colored_label(egui::Color32::from_rgb(0xB0, 0x60, 0x00), reason);
        return cmds;
    }
    let states = status.lock().unwrap().clone();
    for inst in instances {
        let mut on = inst.enabled;
        if ui.checkbox(&mut on, &inst.id).changed() {
            cmds.push(PanelCommand::Upsert(ScriptInstance { enabled: on, ..inst.clone() }));
        }
        if let Some(s) = states.iter().find(|s| s.name == inst.id) {
            ui.small(status_line(s));
        }
    }
    cmds
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
