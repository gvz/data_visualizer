use std::collections::HashMap;

use eframe::egui;

use crate::script::config::{OutputBinding, ScriptInstance};
use crate::script::runner::ScriptState;
use crate::script::types::ScriptMeta;
use crate::script::{ScriptStatus, SharedStatus};
use crate::types::SampleType;

/// A committed panel action for the app to forward to the engine / config.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelCommand {
    Upsert(ScriptInstance),
    Remove(String),
    SaveConfig,
}

/// A row being edited in the panel before Apply commits it.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedInstance {
    pub id: String,
    pub script: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<OutputBinding>,
    pub enabled: bool,
}

impl StagedInstance {
    pub fn to_instance(&self) -> ScriptInstance {
        ScriptInstance {
            id: self.id.clone(),
            script: self.script.clone(),
            inputs: Some(self.inputs.clone()),
            outputs: Some(self.outputs.clone()),
            enabled: self.enabled,
        }
    }
}

/// Panel-persistent editor state: one staged row per instance id, plus the
/// add-instance form.
#[derive(Default)]
pub struct ScriptPanelState {
    pub staged: HashMap<String, StagedInstance>,
    pub new_id: String,
    pub new_script: String,
}

/// Case-insensitive subsequence match; ranks tighter match spans first. Empty
/// query returns every candidate in original order.
pub fn fuzzy_rank<'a>(query: &str, candidates: &'a [String]) -> Vec<&'a String> {
    if query.is_empty() {
        return candidates.iter().collect();
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut scored: Vec<(usize, &String)> = Vec::new();
    for cand in candidates {
        if let Some(span) = match_span(&q, cand) {
            scored.push((span, cand));
        }
    }
    scored.sort_by_key(|(span, _)| *span);
    scored.into_iter().map(|(_, c)| c).collect()
}

fn match_span(q: &[char], cand: &str) -> Option<usize> {
    let lower = cand.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut qi = 0;
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    for (i, ch) in chars.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            if first.is_none() {
                first = Some(i);
            }
            last = i;
            qi += 1;
        }
    }
    if qi == q.len() {
        Some(last - first.unwrap() + 1)
    } else {
        None
    }
}

/// A bound input is valid only if it names a channel that currently exists.
pub fn input_is_valid(name: &str, channel_names: &[String]) -> bool {
    !name.is_empty() && channel_names.iter().any(|n| n == name)
}

/// Map a `SampleType` to a short string for prefilling output type fields.
fn type_str(st: SampleType) -> &'static str {
    match st {
        SampleType::Float => "float",
        SampleType::Int => "int",
        SampleType::Bool => "bool",
        SampleType::Text => "float", // defensive fallback
    }
}

/// Seed a `StagedInstance` from a committed `ScriptInstance`.
fn staged_from_instance(inst: &ScriptInstance) -> StagedInstance {
    StagedInstance {
        id: inst.id.clone(),
        script: inst.script.clone(),
        inputs: inst.inputs.clone().unwrap_or_default(),
        outputs: inst.outputs.clone().unwrap_or_default(),
        enabled: inst.enabled,
    }
}

/// Ids to render, in order: committed instances first (config order), then any
/// staged-only rows — instances the user "Add"ed but hasn't applied yet. Those
/// have no committed counterpart, so without this they would never be drawn and
/// their Apply button (the only way to commit them) would be unreachable.
fn render_order(
    instance_ids: &[String],
    staged: &HashMap<String, StagedInstance>,
) -> Vec<String> {
    let mut ids = instance_ids.to_vec();
    let mut extra: Vec<String> =
        staged.keys().filter(|k| !instance_ids.contains(k)).cloned().collect();
    extra.sort();
    ids.extend(extra);
    ids
}

pub fn draw_script_panel(
    ui: &mut egui::Ui,
    state: &mut ScriptPanelState,
    instances: &[ScriptInstance],
    metas: &HashMap<String, ScriptMeta>,
    channel_names: &[String],
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

    // Ensure every committed instance has a staged row.
    for inst in instances {
        state.staged.entry(inst.id.clone()).or_insert_with(|| staged_from_instance(inst));
    }

    // Render committed instances (config order) followed by any staged-only
    // rows the user just added but hasn't applied yet.
    let instance_ids: Vec<String> = instances.iter().map(|i| i.id.clone()).collect();
    let render_ids = render_order(&instance_ids, &state.staged);
    for id in &render_ids {
        let Some(row) = state.staged.get_mut(id) else { continue };

        ui.separator();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&row.id).strong());
            ui.checkbox(&mut row.enabled, "enabled");
        });

        // Script selector
        ui.horizontal(|ui| {
            ui.label("script:");
            let script_current = row.script.clone();
            let mut script_keys: Vec<String> = metas.keys().cloned().collect();
            script_keys.sort();
            egui::ComboBox::from_id_source(format!("{id}#script"))
                .selected_text(&script_current)
                .show_ui(ui, |ui| {
                    for key in &script_keys {
                        if ui.selectable_label(&row.script == key, key).clicked() {
                            row.script = key.clone();
                            // Prefill outputs when script changes.
                            if let Some(meta) = metas.get(key) {
                                row.outputs = meta
                                    .outputs
                                    .iter()
                                    .map(|o| OutputBinding {
                                        name: o.name.clone(),
                                        ty: type_str(o.sample_type).to_string(),
                                        unit: o.unit.clone(),
                                    })
                                    .collect();
                                // Resize inputs to match new slot count.
                                let n = meta.inputs.len();
                                row.inputs.resize(n, String::new());
                            }
                        }
                    }
                });
        });

        // Determine input slot count from meta, fall back to current staged len.
        let slot_count = metas
            .get(&row.script)
            .map(|m| m.inputs.len())
            .unwrap_or(row.inputs.len());
        row.inputs.resize(slot_count, String::new());

        // Collect slot-level info before splitting borrows. Label each slot
        // generically ("input channel", numbered when the script takes more
        // than one) rather than with the script's declared default channel
        // name, which read like a value.
        let slot_labels: Vec<String> = (0..slot_count)
            .map(|s| {
                if slot_count == 1 {
                    "input channel".to_string()
                } else {
                    format!("input channel {s}")
                }
            })
            .collect();
        let slot_keys: Vec<String> = (0..slot_count).map(|s| format!("{id}#{s}")).collect();

        for (slot, (k, label)) in slot_keys.iter().zip(slot_labels.iter()).enumerate() {
            ui.push_id(k, |ui| {
                // Type directly into the input field. A ComboBox popup can't hold
                // a text field — the field takes focus and egui dismisses the
                // popup as you type — so we render fuzzy suggestions inline
                // instead, only while the field is focused and its text is not
                // yet an exact channel name.
                let resp = ui
                    .horizontal(|ui| {
                        ui.label(format!("{label}:"));
                        let row = state.staged.get_mut(id).unwrap();
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut row.inputs[slot])
                                .hint_text("type to search channels")
                                .desired_width(180.0),
                        );
                        if !input_is_valid(&row.inputs[slot], channel_names) {
                            ui.colored_label(
                                egui::Color32::from_rgb(0xB0, 0x60, 0x00),
                                "unknown channel",
                            );
                        }
                        r
                    })
                    .inner;

                let row = state.staged.get_mut(id).unwrap();
                if resp.has_focus() && !input_is_valid(&row.inputs[slot], channel_names) {
                    for name in
                        fuzzy_rank(&row.inputs[slot], channel_names).into_iter().take(8)
                    {
                        if ui.selectable_label(false, name).clicked() {
                            row.inputs[slot] = name.clone();
                        }
                    }
                }
            });
        }

        // Output rows
        let row = state.staged.get_mut(id).unwrap();
        for (oi, out) in row.outputs.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("out[{oi}] name:"));
                ui.text_edit_singleline(&mut out.name);
                ui.label("type:");
                egui::ComboBox::from_id_source(format!("{id}#out#{oi}#type"))
                    .selected_text(&out.ty)
                    .show_ui(ui, |ui| {
                        for ty in &["float", "int", "bool"] {
                            if ui.selectable_label(&out.ty == *ty, *ty).clicked() {
                                out.ty = ty.to_string();
                            }
                        }
                    });
                ui.label("unit:");
                ui.text_edit_singleline(&mut out.unit);
            });
        }

        // Apply / Remove buttons
        let row = state.staged.get(id).unwrap();
        let all_inputs_valid =
            row.inputs.iter().all(|name| input_is_valid(name, channel_names));
        // Allow Apply even when there are no input slots (scripts with 0 inputs).
        let apply_enabled = all_inputs_valid || slot_count == 0;

        ui.horizontal(|ui| {
            if ui.add_enabled(apply_enabled, egui::Button::new("Apply")).clicked() {
                let inst = state.staged.get(id).unwrap().to_instance();
                cmds.push(PanelCommand::Upsert(inst));
            }
            if ui.button("Remove").clicked() {
                cmds.push(PanelCommand::Remove(id.clone()));
                state.staged.remove(id);
            }
        });

        // Status line
        if let Some(s) = states.iter().find(|s| s.name == *id) {
            ui.small(status_line(s));
        }
    }

    ui.separator();

    // Add-instance form
    ui.label("Add instance:");
    ui.horizontal(|ui| {
        ui.label("id:");
        ui.text_edit_singleline(&mut state.new_id);
        ui.label("script:");
        let mut script_keys: Vec<String> = metas.keys().cloned().collect();
        script_keys.sort();
        egui::ComboBox::from_id_source("new_instance#script")
            .selected_text(if state.new_script.is_empty() {
                "<select>".to_string()
            } else {
                state.new_script.clone()
            })
            .show_ui(ui, |ui| {
                for key in &script_keys {
                    if ui.selectable_label(&state.new_script == key, key).clicked() {
                        state.new_script = key.clone();
                    }
                }
            });
    });

    let can_add = !state.new_id.is_empty()
        && !state.new_script.is_empty()
        && !instance_ids.contains(&state.new_id);

    if ui.add_enabled(can_add, egui::Button::new("Add")).clicked() {
        let outputs = metas
            .get(&state.new_script)
            .map(|m| {
                m.outputs
                    .iter()
                    .map(|o| OutputBinding {
                        name: o.name.clone(),
                        ty: type_str(o.sample_type).to_string(),
                        unit: o.unit.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let input_count =
            metas.get(&state.new_script).map(|m| m.inputs.len()).unwrap_or(0);
        let staged = StagedInstance {
            id: state.new_id.clone(),
            script: state.new_script.clone(),
            inputs: vec![String::new(); input_count],
            outputs,
            enabled: true,
        };
        state.staged.insert(state.new_id.clone(), staged);
        state.new_id.clear();
        state.new_script.clear();
    }

    ui.separator();
    if ui.button("Save to config").clicked() {
        cmds.push(PanelCommand::SaveConfig);
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

    #[test]
    fn fuzzy_rank_empty_query_returns_all() {
        let c = vec!["load/ch0".to_string(), "load/ch1".to_string()];
        assert_eq!(fuzzy_rank("", &c), vec![&c[0], &c[1]]);
    }

    #[test]
    fn fuzzy_rank_subsequence_case_insensitive() {
        let c =
            vec!["load/ch0".to_string(), "sys/temp".to_string(), "load/ch1".to_string()];
        let got = fuzzy_rank("lc1", &c);
        assert_eq!(got, vec![&c[2]]); // only "load/ch1" has l..c..1 as a subsequence
    }

    #[test]
    fn fuzzy_rank_ranks_shorter_match_span_first() {
        let c = vec!["aXXXb".to_string(), "ab".to_string()];
        let got = fuzzy_rank("ab", &c);
        assert_eq!(got, vec![&c[1], &c[0]]); // tighter span ranks first
    }

    #[test]
    fn staged_instance_round_trips_to_command() {
        let row = StagedInstance {
            id: "r".into(),
            script: "sine_rms".into(),
            inputs: vec!["load/ch0".into()],
            outputs: vec![OutputBinding {
                name: "{in0.stem}.rms".into(),
                ty: "float".into(),
                unit: String::new(),
            }],
            enabled: true,
        };
        let inst = row.to_instance();
        assert_eq!(inst.id, "r");
        assert_eq!(inst.inputs, Some(vec!["load/ch0".to_string()]));
        assert_eq!(inst.outputs.as_ref().unwrap()[0].name, "{in0.stem}.rms");
    }

    #[test]
    fn input_unresolved_when_not_in_channel_list() {
        let names = vec!["load/ch0".to_string()];
        assert!(input_is_valid("load/ch0", &names));
        assert!(!input_is_valid("load/ch9", &names));
        assert!(!input_is_valid("", &names));
    }

    fn staged(id: &str) -> StagedInstance {
        StagedInstance {
            id: id.into(),
            script: "s".into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            enabled: true,
        }
    }

    #[test]
    fn render_order_includes_staged_only_added_instance() {
        // "existing" is a committed instance; "fresh" was just Add-ed and has no
        // committed counterpart. Both must render, committed first, or the new
        // row (and its Apply button) would be invisible.
        let committed = vec!["existing".to_string()];
        let mut st = HashMap::new();
        st.insert("existing".to_string(), staged("existing"));
        st.insert("fresh".to_string(), staged("fresh"));

        let order = render_order(&committed, &st);
        assert_eq!(order, vec!["existing".to_string(), "fresh".to_string()]);
    }

    #[test]
    fn render_order_no_duplicates_and_committed_first() {
        let committed = vec!["b".to_string(), "a".to_string()];
        let mut st = HashMap::new();
        for id in ["a", "b", "z", "m"] {
            st.insert(id.to_string(), staged(id));
        }
        // Committed keep config order (b, a); staged-only are appended sorted (m, z).
        assert_eq!(
            render_order(&committed, &st),
            vec!["b".to_string(), "a".to_string(), "m".to_string(), "z".to_string()]
        );
    }
}
