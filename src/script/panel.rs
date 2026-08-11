//! Iced scripting control panel.
//!
//! Provides [`view`] — a pure function that builds the modal body for the
//! Scripts dialog — and [`ScriptEditState`], the transient editor state held
//! by `DataVisApp`.
//!
//! The scripting ENGINE (`mod.rs`, `runner.rs`, `python.rs`, `config.rs`) is
//! reused unchanged. The UI is built with iced widgets.
//!
//! The panel is feature-agnostic: when `disabled` is `Some(reason)` (which
//! main.rs sets when the `scripting` feature is off), the panel shows a
//! prominent notice and nothing else.

use std::sync::{Arc, Mutex};

use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input};
use iced::{Element, Length};

use crate::script::config::{OutputBinding, ScriptInstance};
use crate::script::runner::ScriptState;
use crate::script::types::ScriptMeta;
use crate::script::{ScriptStatus, SharedMetas, SharedStatus};
use crate::types::SampleType;

use crate::ui::message::Message;

// ── Pure helpers ─────────────────────────────────────────────────────────────

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

/// The default output bindings for an instance: the channel name defaults to
/// `scripts/<id>` (`scripts/<id>_N` when the script declares more than one
/// output), with types/units taken from the script. Editable afterwards.
pub fn default_outputs_pub(id: &str, meta: &ScriptMeta) -> Vec<OutputBinding> {
    default_outputs(id, meta)
}

fn default_outputs(id: &str, meta: &ScriptMeta) -> Vec<OutputBinding> {
    let n = meta.outputs.len();
    meta.outputs
        .iter()
        .enumerate()
        .map(|(i, o)| OutputBinding {
            name: if n == 1 { format!("scripts/{id}") } else { format!("scripts/{id}_{i}") },
            ty: type_str(o.sample_type).to_string(),
            unit: o.unit.clone(),
        })
        .collect()
}

/// Seed a `StagedInstance` from a committed `ScriptInstance`. Instances that
/// omit `inputs`/`outputs` in config rely on the script's declared defaults —
/// materialize those from the peeked `meta` (when known) so they are visible
/// and editable rather than showing as empty.
pub fn staged_from_instance(inst: &ScriptInstance, meta: Option<&ScriptMeta>) -> StagedInstance {
    let inputs = inst
        .inputs
        .clone()
        .unwrap_or_else(|| meta.map(|m| m.inputs.clone()).unwrap_or_default());
    let outputs = inst
        .outputs
        .clone()
        .unwrap_or_else(|| meta.map(|m| default_outputs(&inst.id, m)).unwrap_or_default());
    StagedInstance {
        id: inst.id.clone(),
        script: inst.script.clone(),
        inputs,
        outputs,
        enabled: inst.enabled,
    }
}

// ── Editor state ─────────────────────────────────────────────────────────────

/// A row being edited in the panel before Apply commits it.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedInstance {
    pub id: String,
    pub script: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<OutputBinding>,
    pub enabled: bool,
}

impl Default for StagedInstance {
    fn default() -> Self {
        Self {
            id: String::new(),
            script: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            enabled: true,
        }
    }
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

/// Transient editor state held in `DataVisApp` for the Scripts modal.
///
/// Holds a single `staged` for the currently selected/new instance.
/// This reduces UI complexity at the cost of not preserving unsaved edits for
/// multiple instances simultaneously.
#[derive(Default)]
pub struct ScriptEditState {
    /// The instance currently being edited (None = "add new" form).
    pub selected_id: Option<String>,
    /// The staged row being edited.
    pub staged: StagedInstance,
    /// Reserved for a future fuzzy channel/script filter field (wired to
    /// nothing yet). Kept so the state shape is stable for that follow-up.
    #[allow(dead_code)]
    pub script_query: String,
    /// Error message from last Apply (cleared on next Apply).
    pub last_error: Option<String>,
}

// ── Status helper (pure) ─────────────────────────────────────────────────────

fn status_line(s: &ScriptStatus) -> String {
    match &s.state {
        ScriptState::Healthy => "running".to_string(),
        ScriptState::Waiting(what) => format!("waiting for {what}"),
        ScriptState::Failed(msg) => format!("FAILED: {msg}"),
    }
}

// ── View ─────────────────────────────────────────────────────────────────────

/// Build the modal body for the Scripts dialog.
///
/// * `instances`  — committed instances (from `DataVisApp::script_instances`).
/// * `status`     — live per-script status from the engine thread.
/// * `metas`      — peeked script metadata (for auto-filling inputs/outputs).
/// * `disabled`   — `Some(reason)` when scripting is off; `None` when enabled.
/// * `state`      — transient editor state held by `DataVisApp`.
pub fn view<'a>(
    instances: &'a [ScriptInstance],
    status: &SharedStatus,
    metas: &SharedMetas,
    disabled: &Arc<Mutex<Option<String>>>,
    state: &'a ScriptEditState,
) -> Element<'a, Message> {
    // ── Scripting-disabled notice (only thing shown when feature is off) ──
    // Frame-path locks use a poison-tolerant read: if a worker thread panicked
    // while holding one of these locks, `.unwrap()` would crash the render loop
    // on the next repaint. `unwrap_or_else(|e| e.into_inner())` recovers the
    // last good value instead.
    if let Some(reason) = disabled.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let notice = container(
            column![
                text("Scripting not enabled").size(18),
                text(reason.clone()).size(14),
            ]
            .spacing(8),
        )
        .padding(20);
        return card_container(notice.into());
    }

    let statuses = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let metas_guard = metas.lock().unwrap_or_else(|e| e.into_inner());

    // ── Instance list ─────────────────────────────────────────────────────
    let mut instance_rows: Vec<Element<'_, Message>> = Vec::new();
    instance_rows.push(text("Script Instances").size(16).into());

    if instances.is_empty() {
        instance_rows.push(text("No instances — use the form below to add one.").size(13).into());
    } else {
        for inst in instances {
            let st = statuses.iter().find(|s| s.name == inst.id);
            let status_str = match st {
                Some(s) => status_line(s),
                None => {
                    if inst.enabled { "pending…".to_string() } else { "disabled".to_string() }
                }
            };

            let is_selected = state.selected_id.as_deref() == Some(&inst.id);
            let select_btn = if is_selected {
                button(text("editing")).on_press(Message::ScriptSelect(inst.id.clone()))
            } else {
                button(text("edit")).on_press(Message::ScriptSelect(inst.id.clone()))
            };
            let remove_btn =
                button(text("remove")).on_press(Message::ScriptRemove(inst.id.clone()));

            let inst_row = row![
                text(inst.id.clone()).width(Length::Fixed(120.0)),
                text(status_str).width(Length::Fill),
                select_btn,
                remove_btn,
            ]
            .spacing(8)
            .padding(2);
            instance_rows.push(inst_row.into());
        }
    }

    // ── Editor form ───────────────────────────────────────────────────────
    let editing_title = if state.selected_id.is_some() { "Edit Instance" } else { "Add Instance" };
    let mut form_rows: Vec<Element<'_, Message>> = Vec::new();
    form_rows.push(text(editing_title).size(16).into());

    // ID field (always shown; editable only for new instances)
    let id_row = if state.selected_id.is_some() {
        // Editing a committed instance — show id as label text
        row![
            text("ID:").width(Length::Fixed(80.0)),
            text(state.staged.id.clone())
        ].spacing(8)
    } else {
        let id_input = text_input("instance id", &state.staged.id)
            .on_input(Message::ScriptIdChanged)
            .width(Length::Fixed(200.0));
        row![
            text("ID:").width(Length::Fixed(80.0)),
            id_input
        ].spacing(8)
    };
    form_rows.push(id_row.into());

    // Script picker (pick_list from discovered metas)
    let mut script_names: Vec<String> = metas_guard.keys().cloned().collect();
    script_names.sort();
    let selected_script =
        if state.staged.script.is_empty() { None } else { Some(state.staged.script.clone()) };
    let script_picker =
        pick_list(script_names, selected_script, Message::ScriptScriptChanged).width(Length::Fixed(200.0));
    form_rows
        .push(row![text("Script:").width(Length::Fixed(80.0)), script_picker].spacing(8).into());

    // Input slots
    if !state.staged.inputs.is_empty() {
        for (i, inp) in state.staged.inputs.iter().enumerate() {
            let label = if state.staged.inputs.len() == 1 {
                "Input:".to_string()
            } else {
                format!("Input {i}:")
            };
            let input_field = text_input("channel name", inp)
                .on_input(move |v| Message::ScriptInputChanged(i, v))
                .width(Length::Fixed(200.0));
            form_rows.push(
                row![text(label).width(Length::Fixed(80.0)), input_field].spacing(8).into(),
            );
        }
    }

    // Output rows (simplified: just show name/type/unit as text; no inline editing
    // to keep the modal compact — full per-slot editor is a future enhancement)
    if !state.staged.outputs.is_empty() {
        form_rows.push(text("Outputs:").size(13).into());
        for (i, out) in state.staged.outputs.iter().enumerate() {
            let out_name_input = text_input("output name", &out.name)
                .on_input(move |v| Message::ScriptOutputNameChanged(i, v))
                .width(Length::Fixed(180.0));
            let type_options = vec!["float".to_string(), "int".to_string(), "bool".to_string()];
            let out_type_picker = pick_list(
                type_options,
                Some(out.ty.clone()),
                move |v| Message::ScriptOutputTypeChanged(i, v),
            )
            .width(Length::Fixed(80.0));
            form_rows.push(
                row![
                    text(format!("[{i}]")).width(Length::Fixed(30.0)),
                    out_name_input,
                    out_type_picker,
                ]
                .spacing(6)
                .into(),
            );
        }
    }

    // Enable checkbox (row with two buttons as substitute — iced 0.14 checkbox)
    {
        let enabled_label = if state.staged.enabled { "enabled" } else { "disabled" };
        let toggle_btn =
            button(text(enabled_label)).on_press(Message::ScriptToggleEnabled);
        form_rows.push(row![text("State:").width(Length::Fixed(80.0)), toggle_btn].spacing(8).into());
    }

    // Error display
    if let Some(err) = &state.last_error {
        form_rows.push(text(format!("Error: {err}")).size(12).into());
    }

    // Apply / Clear buttons. Gate Apply behind a non-empty id AND script so a
    // blank instance can never be upserted and sent to the engine. When
    // disabled we render the button without `on_press`, which iced draws as inert.
    let can_apply = !state.staged.id.is_empty() && !state.staged.script.is_empty();
    let apply_btn = if can_apply {
        button(text("Apply")).on_press(Message::ScriptApply)
    } else {
        button(text("Apply"))
    };
    let clear_btn = button(text("New")).on_press(Message::ScriptClearForm);
    let close_btn = button(text("Close")).on_press(Message::CloseModal);
    form_rows.push(
        row![apply_btn, clear_btn, close_btn].spacing(8).into(),
    );

    // ── Compose ───────────────────────────────────────────────────────────
    let instances_col = column(instance_rows).spacing(4);
    let form_col = column(form_rows).spacing(8);

    let body = scrollable(
        column![
            instances_col,
            form_col,
        ]
        .spacing(16)
        .padding(20)
        .width(Length::Fixed(520.0)),
    )
    .height(Length::Fixed(480.0));

    card_container(body.into())
}

/// Wrap content in a styled card container (same style as other dialogs).
fn card_container(content: Element<'_, Message>) -> Element<'_, Message> {
    container(content)
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
    use std::collections::HashMap;
    use super::*;

    // ── Pure helper tests ────────────────────────────────────────────────

    fn mk_status(st: ScriptState) -> ScriptStatus {
        ScriptStatus { name: "x".into(), state: st }
    }

    #[test]
    fn status_line_formats_each_state() {
        assert!(status_line(&mk_status(ScriptState::Healthy)).contains("running"));
        assert!(status_line(&mk_status(ScriptState::Waiting("in.a".into()))).contains("in.a"));
        assert!(status_line(&mk_status(ScriptState::Failed("boom".into()))).contains("boom"));
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

    #[test]
    fn staged_from_instance_materializes_script_defaults() {
        use crate::script::types::OutputSpec;
        let inst = ScriptInstance {
            id: "rms".into(),
            script: "sine_rms".into(),
            inputs: None,
            outputs: None,
            enabled: true,
        };
        let meta = ScriptMeta {
            inputs: vec!["load/ch0".into()],
            outputs: vec![OutputSpec {
                name: "scripts.{in0.stem}_rms".into(),
                sample_type: SampleType::Float,
                unit: "g".into(),
            }],
        };
        let staged = staged_from_instance(&inst, Some(&meta));
        assert_eq!(staged.inputs, vec!["load/ch0".to_string()]);
        assert_eq!(staged.outputs.len(), 1);
        // Single-output default is named scripts/<id>, not the script template;
        // type/unit still come from the script.
        assert_eq!(staged.outputs[0].name, "scripts/rms");
        assert_eq!(staged.outputs[0].ty, "float");
        assert_eq!(staged.outputs[0].unit, "g");
    }

    #[test]
    fn staged_from_instance_keeps_explicit_bindings_over_defaults() {
        let inst = ScriptInstance {
            id: "x".into(),
            script: "s".into(),
            inputs: Some(vec!["a/b".into()]),
            outputs: Some(vec![OutputBinding {
                name: "explicit".into(),
                ty: "int".into(),
                unit: String::new(),
            }]),
            enabled: true,
        };
        let meta = ScriptMeta { inputs: vec!["default".into()], outputs: vec![] };
        let staged = staged_from_instance(&inst, Some(&meta));
        assert_eq!(staged.inputs, vec!["a/b".to_string()]);
        assert_eq!(staged.outputs[0].name, "explicit");
    }

    // ── View build tests ─────────────────────────────────────────────────

    /// Building the Element value (without rendering it) must not panic for
    /// the disabled/scripting-off path.
    #[test]
    fn view_builds_without_panic_when_disabled() {
        let instances: Vec<ScriptInstance> = vec![];
        let status: SharedStatus = Arc::new(Mutex::new(vec![]));
        let metas: SharedMetas = Arc::new(Mutex::new(HashMap::new()));
        let disabled: Arc<Mutex<Option<String>>> =
            Arc::new(Mutex::new(Some("scripting feature not enabled".to_string())));
        let state = ScriptEditState::default();

        // Building the element must not panic.
        let _el: Element<'_, Message> = view(&instances, &status, &metas, &disabled, &state);
    }

    /// Building the Element value must not panic when scripting is enabled and
    /// there are some committed instances.
    #[test]
    fn view_builds_without_panic_when_enabled_with_instances() {
        let instances = vec![
            ScriptInstance {
                id: "inst1".into(),
                script: "myscript".into(),
                inputs: Some(vec!["ch/a".into()]),
                outputs: None,
                enabled: true,
            },
            ScriptInstance {
                id: "inst2".into(),
                script: "other".into(),
                inputs: None,
                outputs: None,
                enabled: false,
            },
        ];
        let status: SharedStatus = Arc::new(Mutex::new(vec![ScriptStatus {
            name: "inst1".into(),
            state: ScriptState::Healthy,
        }]));
        let metas: SharedMetas = Arc::new(Mutex::new({
            let mut m = HashMap::new();
            m.insert("myscript".to_string(), ScriptMeta { inputs: vec!["ch/a".into()], outputs: vec![] });
            m
        }));
        let disabled: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let mut state = ScriptEditState::default();
        state.selected_id = Some("inst1".to_string());
        state.staged = StagedInstance {
            id: "inst1".into(),
            script: "myscript".into(),
            inputs: vec!["ch/a".into()],
            outputs: vec![],
            enabled: true,
        };

        // Building the element must not panic.
        let _el: Element<'_, Message> = view(&instances, &status, &metas, &disabled, &state);
    }
}
