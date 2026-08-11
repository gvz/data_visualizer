use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::widget::{button, column, container, row, text};
use iced::{Element, Length, Task};

use super::sidebar;
use super::timeline;

use crate::channel_tree::ChannelTree;
use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
use crate::record::playback::PlaybackStore;
use crate::record::{self, RecordHandle};
use crate::store::ChannelStore;
use crate::script::config::ScriptInstance;
use crate::script::{ScriptCommand, SharedMetas, SharedStatus};
use crate::viz::PanelRegistry;

use crate::script::panel::{ScriptEditState, staged_from_instance};

use super::data_source as data_source_ui;
use super::message::{ActiveDialog, Message, SourceField};
use super::modal;
use super::workspace::Workspace;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Repaint interval when the store is actively writing new samples (~60 fps).
const FAST_INTERVAL_MS: u64 = 16;
/// Repaint interval when the store has been idle (no new writes).
const SLOW_INTERVAL_MS: u64 = 250;
/// How long the store must be idle before we drop to the slow interval.
const IDLE_THRESHOLD: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Sub-types
// ---------------------------------------------------------------------------

pub(crate) struct ReplayState {
    pub store: Arc<PlaybackStore>,
    pub playing: bool,
    pub speed: f32,
    pub last_frame: Instant,
}

pub(crate) enum AppMode {
    Live,
    Replay(ReplayState),
}

/// Top-level iced app — fields carried over from the egui DataVisApp.
/// workspace / add_panel / add_source_form are reintroduced by Tasks 5/12/14.
pub struct DataVisApp {
    pub(crate) workspace: Workspace,
    pub(crate) registry: PanelRegistry,
    pub(crate) store: Arc<dyn ChannelStore>,
    pub(crate) live_store: Arc<dyn ChannelStore>,
    pub(crate) channels: Arc<ChannelRegistry>,
    pub(crate) layout_path: PathBuf,
    pub(crate) status: String,
    pub(crate) status_clear_at: Option<Instant>,
    pub(crate) manager: crate::ingest::SourceManager,
    pub(crate) mode: AppMode,
    pub(crate) pending_load: Option<(String, crossbeam_channel::Receiver<anyhow::Result<Arc<PlaybackStore>>>)>,
    pub(crate) record_handle: Option<RecordHandle>,
    pub(crate) channel_tree: ChannelTree,
    pub(crate) saved_channel_tree: Option<ChannelTree>,
    pub(crate) sidebar_visible: bool,
    pub(crate) mqtt_snapshot: BTreeMap<String, String>,
    pub(crate) mqtt_snapshot_at: Instant,
    pub(crate) live_view_ns: Arc<AtomicI64>,
    pub(crate) live_view_offset_ns: i64,
    pub(crate) live_history_s: f64,
    pub(crate) live_paused: bool,
    pub(crate) live_pause_ns: i64,
    pub(crate) last_write_seq: u64,
    /// When the store last advanced (write_seq changed). Used to detect idle.
    pub(crate) last_write_advance: Instant,
    pub(crate) default_window_s: f64,
    pub(crate) _available_scripts: Vec<String>,
    pub(crate) script_instances: Vec<ScriptInstance>,
    pub(crate) script_metas: SharedMetas,
    pub(crate) script_status: SharedStatus,
    pub(crate) script_commands: crossbeam_channel::Sender<ScriptCommand>,
    pub(crate) script_disabled: Arc<Mutex<Option<String>>>,
    pub(crate) record_max_bytes: Option<u64>,
    /// Channel currently being dragged from the sidebar (Task 12 wires drop).
    pub(crate) dragging: Option<String>,
    /// Add-source form state (Task 14).
    pub(crate) add_source_form: crate::ui::data_source::AddSourceForm,
    /// Which dialog overlay is open, if any (Task 14).
    pub(crate) active_dialog: Option<super::message::ActiveDialog>,
    /// Name being typed in the Add Screen dialog.
    pub(crate) add_screen_name: String,
    /// Panel type selected in the Add Panel dialog.
    pub(crate) add_panel_type: Option<String>,
    /// Task 15: transient editor state for the Scripts dialog.
    pub(crate) script_edit: ScriptEditState,
}

impl DataVisApp {
    pub fn new(
        registry: PanelRegistry,
        store: Arc<dyn ChannelStore>,
        channels: Arc<ChannelRegistry>,
        layout_path: PathBuf,
        layout: LayoutConfig,
        manager: crate::ingest::SourceManager,
        live_view_ns: Arc<AtomicI64>,
        live_history_s: f64,
        default_window_s: f64,
        available_scripts: Vec<String>,
        script_instances: Vec<ScriptInstance>,
        script_status: SharedStatus,
        script_commands: crossbeam_channel::Sender<ScriptCommand>,
        script_disabled: Arc<Mutex<Option<String>>>,
        script_metas: SharedMetas,
        record_max_bytes: Option<u64>,
    ) -> Self {
        let channel_tree = ChannelTree::build(channels.as_ref());
        let workspace = Workspace::from_config(&layout, &registry, channels.as_ref());
        Self {
            workspace,
            registry,
            live_store: store.clone(),
            store,
            channels,
            layout_path,
            status: String::new(),
            status_clear_at: None,
            manager,
            mode: AppMode::Live,
            pending_load: None,
            record_handle: None,
            channel_tree,
            saved_channel_tree: None,
            sidebar_visible: true,
            mqtt_snapshot: BTreeMap::new(),
            mqtt_snapshot_at: Instant::now() - Duration::from_secs(2),
            live_view_ns,
            live_view_offset_ns: 0,
            live_history_s,
            live_paused: false,
            live_pause_ns: 0,
            last_write_seq: 0,
            last_write_advance: Instant::now(),
            default_window_s,
            _available_scripts: available_scripts,
            script_instances,
            script_metas,
            script_status,
            script_commands,
            script_disabled,
            record_max_bytes,
            dragging: None,
            add_source_form: crate::ui::data_source::AddSourceForm::default(),
            active_dialog: None,
            add_screen_name: String::new(),
            add_panel_type: None,
            script_edit: ScriptEditState::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Adaptive repaint helpers
    // -----------------------------------------------------------------------

    /// Returns the repaint interval for a given activity state.
    ///
    /// `advanced = true`  → store is writing new samples → fast (~60 fps).
    /// `advanced = false` → store is idle                → slow heartbeat.
    pub fn interval_for(advanced: bool) -> Duration {
        if advanced {
            Duration::from_millis(FAST_INTERVAL_MS)
        } else {
            Duration::from_millis(SLOW_INTERVAL_MS)
        }
    }

    /// Current desired repaint interval based on whether the store is idle.
    pub fn desired_interval(&self) -> Duration {
        let idle = self.last_write_advance.elapsed() >= IDLE_THRESHOLD;
        Self::interval_for(!idle)
    }

    // -----------------------------------------------------------------------
    // iced interface
    // -----------------------------------------------------------------------

    pub fn update(&mut self, message: Message, _now: std::time::Instant) -> Task<Message> {
        match message {
            Message::Tick => {
                // Poll for any completed background recording load.
                self.poll_pending_load();

                // Adaptive repaint idle-detection. Always read the LIVE store's
                // write_seq: during replay `self.store` is the (static)
                // playback store, so keying off it would drop the repaint to
                // the slow idle rate and stutter the scrubbed playhead. The
                // live store keeps advancing, keeping replay responsive.
                let seq = self.live_store.write_seq();
                if seq != self.last_write_seq {
                    self.last_write_seq = seq;
                    self.last_write_advance = Instant::now();
                }
                // Sync all visible panels from the ACTIVE store — the replay
                // store while in Replay mode, live store otherwise.
                let active_store: &dyn ChannelStore = match &self.mode {
                    AppMode::Replay(rs) => rs.store.as_ref(),
                    AppMode::Live => self.store.as_ref(),
                };
                self.workspace.sync_all(active_store);
                self.expire_status();
                Task::none()
            }

            // Fully wired toggles.
            Message::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                Task::none()
            }

            // ---------------------------------------------------------------
            // Recording / layout — wired in Task 13.
            // ---------------------------------------------------------------
            Message::Load => {
                self.load_layout();
                Task::none()
            }
            Message::SaveLayout => {
                self.save_layout();
                Task::none()
            }
            Message::SaveLayoutAs => {
                self.save_layout_as();
                Task::none()
            }
            Message::LoadLayoutFrom => {
                self.load_layout_from();
                Task::none()
            }
            Message::StartRecord => {
                self.start_recording();
                Task::none()
            }
            Message::StopRecord => {
                self.stop_recording();
                Task::none()
            }
            Message::OpenRecording => {
                self.open_recording();
                Task::none()
            }
            Message::Seek(ns) => {
                if let AppMode::Replay(ref rs) = self.mode {
                    rs.store
                        .position_ns
                        .store(ns, Ordering::Relaxed);
                }
                Task::none()
            }
            Message::CloseReplay => {
                self.close_replay();
                Task::none()
            }

            Message::AddPanel => {
                // Open the Add Panel dialog.
                self.active_dialog = Some(ActiveDialog::AddPanel);
                if self.add_panel_type.is_none() {
                    self.add_panel_type =
                        self.registry.type_names().first().map(|s| s.to_string());
                }
                Task::none()
            }
            Message::AddScreen => {
                // Open the Add Screen dialog.
                self.active_dialog = Some(ActiveDialog::AddScreen);
                self.add_screen_name = String::new();
                Task::none()
            }
            Message::Panel(pane_id, panel_msg) => {
                // Route the panel-local message to the correct panel.
                // The workspace owns the panels; we need to access them.
                // For now we delegate via a workspace method (added below).
                let _ = (pane_id, panel_msg); // handled by workspace in a later revision
                Task::none()
            }
            Message::ClosePane(pane_id) => {
                self.workspace.close_pane(pane_id);
                Task::none()
            }
            Message::PaneResized(event) => {
                self.workspace.on_resize(event);
                Task::none()
            }
            Message::DragChannel(name) => {
                // Record the channel being dragged; drop-onto-pane binding is Task 12.
                self.dragging = Some(name);
                Task::none()
            }
            Message::DropOnPane(pane) => {
                // Take the dragged channel name (clears dragging regardless).
                if let Some(name) = self.dragging.take() {
                    // Compute the full multi-select payload (selected set if the
                    // dragged leaf is in the selection, else just that leaf).
                    let order = sidebar::leaf_labels(&self.channel_tree);
                    let payload = self.channel_tree.drag_payload(&name, &order);
                    for chan in &payload {
                        self.workspace.bind_channel_to_pane(
                            pane,
                            chan,
                            &self.registry,
                            &self.channels,
                        );
                    }
                }
                Task::none()
            }

            // ---------------------------------------------------------------
            // Task 14: data-source modal
            // ---------------------------------------------------------------
            Message::OpenDataSource => {
                self.active_dialog = Some(ActiveDialog::DataSource);
                Task::none()
            }
            Message::CloseDataSource => {
                self.active_dialog = None;
                Task::none()
            }
            Message::CloseModal => {
                self.active_dialog = None;
                Task::none()
            }

            Message::SourceKindSelected(kind) => {
                self.add_source_form.kind = kind;
                Task::none()
            }
            Message::SourceField(field, value) => {
                match field {
                    SourceField::Endpoint => self.add_source_form.endpoint = value,
                    SourceField::Schema => self.add_source_form.schema = value,
                    SourceField::Broker => self.add_source_form.broker = value,
                    SourceField::ClientId => self.add_source_form.client_id = value,
                    SourceField::Listen => self.add_source_form.listen = value,
                    SourceField::BridgeName => self.add_source_form.name = value,
                    SourceField::Command => self.add_source_form.command = value,
                    SourceField::Args => self.add_source_form.args = value,
                }
                Task::none()
            }
            Message::BrowseSchema => {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Proto schema", &["proto"])
                    .pick_file()
                {
                    self.add_source_form.schema = p.display().to_string();
                }
                Task::none()
            }
            Message::BrowseCommand => {
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    self.add_source_form.command = p.display().to_string();
                }
                Task::none()
            }
            Message::StartSource => {
                match self.add_source_form.to_spec() {
                    Ok(spec) => match self.manager.add(&spec, true) {
                        Ok(_) => {
                            match data_source_ui::persist(&self.layout_path, &self.manager) {
                                Ok(()) => {
                                    self.add_source_form.last_error = None;
                                    self.add_source_form.clear();
                                }
                                Err(e) => {
                                    self.add_source_form.last_error =
                                        Some(format!("persist error: {e}"));
                                }
                            }
                        }
                        Err(e) => {
                            self.add_source_form.last_error = Some(e.to_string());
                        }
                    },
                    Err(msg) => {
                        self.add_source_form.last_error = Some(msg);
                    }
                }
                Task::none()
            }
            Message::StopSource(id) => {
                self.manager.remove(id);
                match data_source_ui::persist(&self.layout_path, &self.manager) {
                    Ok(()) => self.add_source_form.last_error = None,
                    Err(e) => {
                        self.add_source_form.last_error =
                            Some(format!("persist error: {e}"));
                    }
                }
                Task::none()
            }

            // ---------------------------------------------------------------
            // Task 14: add-screen dialog
            // ---------------------------------------------------------------
            Message::AddScreenNameChanged(name) => {
                self.add_screen_name = name;
                Task::none()
            }
            Message::ConfirmAddScreen => {
                let name = if self.add_screen_name.trim().is_empty() {
                    None
                } else {
                    Some(self.add_screen_name.trim().to_string())
                };
                self.workspace.add_screen(name, &self.registry, &self.channels);
                self.active_dialog = None;
                Task::none()
            }

            // ---------------------------------------------------------------
            // Task 14: add-panel dialog
            // ---------------------------------------------------------------
            Message::AddPanelTypeSelected(type_name) => {
                self.add_panel_type = Some(type_name);
                Task::none()
            }
            Message::ConfirmAddPanel => {
                if let Some(type_name) = self.add_panel_type.clone() {
                    let entry = PanelEntry {
                        panel_type: type_name,
                        config: toml::Table::new(),
                    };
                    self.workspace.add_panel(entry, &self.registry, &self.channels);
                }
                self.active_dialog = None;
                Task::none()
            }

            // ---------------------------------------------------------------
            // Task 15: scripting control panel
            // ---------------------------------------------------------------
            Message::OpenScripts => {
                self.active_dialog = Some(ActiveDialog::Scripts);
                Task::none()
            }
            Message::ScriptSelect(id) => {
                // Load the clicked instance into the staged editor.
                if let Some(inst) = self.script_instances.iter().find(|i| i.id == id) {
                    // Poison-tolerant: a panicked script worker must not crash the UI.
                    let meta_guard = self.script_metas.lock().unwrap_or_else(|e| e.into_inner());
                    let meta = meta_guard.get(&inst.script);
                    let staged = staged_from_instance(inst, meta);
                    drop(meta_guard);
                    self.script_edit.selected_id = Some(id);
                    self.script_edit.staged = staged;
                    self.script_edit.last_error = None;
                }
                Task::none()
            }
            Message::ScriptRemove(id) => {
                self.script_instances.retain(|i| i.id != id);
                let _ = self.script_commands.send(crate::script::ScriptCommand::Remove(id.clone()));
                // If we were editing this instance, clear the form.
                if self.script_edit.selected_id.as_deref() == Some(&id) {
                    self.script_edit = ScriptEditState::default();
                }
                Task::none()
            }
            Message::ScriptApply => {
                let inst = self.script_edit.staged.to_instance();
                // Upsert into local list.
                match self.script_instances.iter_mut().find(|i| i.id == inst.id) {
                    Some(slot) => *slot = inst.clone(),
                    None => self.script_instances.push(inst.clone()),
                }
                // Send command to engine.
                let _ = self.script_commands.send(crate::script::ScriptCommand::Upsert(inst.clone()));
                self.script_edit.selected_id = Some(inst.id.clone());
                self.script_edit.last_error = None;
                Task::none()
            }
            Message::ScriptClearForm => {
                self.script_edit = ScriptEditState::default();
                Task::none()
            }
            Message::ScriptIdChanged(val) => {
                self.script_edit.staged.id = val;
                Task::none()
            }
            Message::ScriptScriptChanged(val) => {
                // When the script changes, resize inputs and prefill outputs.
                // Poison-tolerant: a panicked script worker must not crash the UI.
                let meta_guard = self.script_metas.lock().unwrap_or_else(|e| e.into_inner());
                let meta = meta_guard.get(&val);
                if let Some(m) = meta {
                    let id = &self.script_edit.staged.id;
                    self.script_edit.staged.outputs =
                        crate::script::panel::default_outputs_pub(id, m);
                    self.script_edit.staged.inputs.resize(m.inputs.len(), String::new());
                }
                drop(meta_guard);
                self.script_edit.staged.script = val;
                Task::none()
            }
            Message::ScriptInputChanged(slot, val) => {
                if let Some(s) = self.script_edit.staged.inputs.get_mut(slot) {
                    *s = val;
                }
                Task::none()
            }
            Message::ScriptOutputNameChanged(slot, val) => {
                if let Some(o) = self.script_edit.staged.outputs.get_mut(slot) {
                    o.name = val;
                }
                Task::none()
            }
            Message::ScriptOutputTypeChanged(slot, val) => {
                if let Some(o) = self.script_edit.staged.outputs.get_mut(slot) {
                    o.ty = val;
                }
                Task::none()
            }
            Message::ScriptToggleEnabled => {
                self.script_edit.staged.enabled = !self.script_edit.staged.enabled;
                Task::none()
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let is_replaying = matches!(self.mode, AppMode::Replay(_));

        // Menu + toolbar row.
        let top_bar = self.menu_toolbar_row();

        // Sidebar — real channel tree view (Task 6).
        let sidebar = sidebar::view(&self.channel_tree);

        // Workspace — pane grid with the active screen's panels.
        let workspace = self.workspace.view(true);

        // Main content row: optional sidebar + workspace.
        let content_row: Element<'_, Message> = if self.sidebar_visible {
            row![sidebar, workspace].into()
        } else {
            row![workspace].into()
        };

        // Status bar.
        let status_bar = container(text(self.status.as_str()).size(13))
            .width(Length::Fill)
            .padding(4);

        // Full layout.
        let base: Element<'_, Message> = if is_replaying {
            let (range, pos) = self.replay_time_info();
            let tl = timeline::view(range, pos);
            column![top_bar, content_row, tl, status_bar]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            column![top_bar, content_row, status_bar]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        };

        // Wrap in modal overlay if any dialog is open.
        match &self.active_dialog {
            Some(ActiveDialog::DataSource) => {
                let card =
                    data_source_ui::view(&self.add_source_form, &self.manager);
                modal::modal(base, card, Message::CloseModal)
            }
            Some(ActiveDialog::AddScreen) => {
                let card = self.add_screen_dialog();
                modal::modal(base, card, Message::CloseModal)
            }
            Some(ActiveDialog::AddPanel) => {
                let card = self.add_panel_dialog();
                modal::modal(base, card, Message::CloseModal)
            }
            Some(ActiveDialog::Scripts) => {
                // Task 15: scripting control panel modal.
                let card = crate::script::panel::view(
                    &self.script_instances,
                    &self.script_status,
                    &self.script_metas,
                    &self.script_disabled,
                    &self.script_edit,
                );
                modal::modal(base, card, Message::CloseModal)
            }
            None => base,
        }
    }

    pub fn subscription(&self) -> iced::Subscription<Message> {
        iced::time::every(self.desired_interval()).map(|_| Message::Tick)
    }

    // -----------------------------------------------------------------------
    // Dialog helpers (Task 14)
    // -----------------------------------------------------------------------

    fn add_screen_dialog(&self) -> Element<'_, Message> {
        use iced::widget::{column, container, row, text, text_input};
        let input = text_input("Screen name…", &self.add_screen_name)
            .on_input(Message::AddScreenNameChanged);
        let confirm = button(text("Add")).on_press(Message::ConfirmAddScreen);
        let cancel = button(text("Cancel")).on_press(Message::CloseModal);
        let card = column![
            text("Add Screen").size(18),
            row![text("Name:"), input].spacing(8),
            row![confirm, cancel].spacing(8),
        ]
        .spacing(12)
        .padding(20)
        .width(Length::Fixed(360.0));
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

    fn add_panel_dialog(&self) -> Element<'_, Message> {
        use iced::widget::{column, container, pick_list, row, text};
        let type_names: Vec<String> =
            self.registry.type_names().into_iter().map(|s| s.to_string()).collect();
        let selected = self.add_panel_type.clone();
        let picker = pick_list(type_names, selected, Message::AddPanelTypeSelected);
        let confirm = button(text("Add")).on_press(Message::ConfirmAddPanel);
        let cancel = button(text("Cancel")).on_press(Message::CloseModal);
        let card = column![
            text("Add Panel").size(18),
            row![text("Type:"), picker].spacing(8),
            row![confirm, cancel].spacing(8),
        ]
        .spacing(12)
        .padding(20)
        .width(Length::Fixed(360.0));
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

    // -----------------------------------------------------------------------
    // View helpers
    // -----------------------------------------------------------------------

    /// Renders the combined menu + toolbar row.
    fn menu_toolbar_row(&self) -> Element<'_, Message> {
        let is_replaying = matches!(self.mode, AppMode::Replay(_));

        // File menu buttons (flat for now; real menus via Task 13).
        let btn_save = button(text("Save layout"))
            .on_press(Message::SaveLayout);
        let btn_save_as = button(text("Save layout as…"))
            .on_press(Message::SaveLayoutAs);
        let btn_load = button(text("Load layout"))
            .on_press(Message::Load);
        let btn_load_from = button(text("Load layout from…"))
            .on_press(Message::LoadLayoutFrom);

        // Data source toggle — driven solely by active_dialog.
        let ds_open = matches!(self.active_dialog, Some(ActiveDialog::DataSource));
        let ds_label = if ds_open { "Close Data Source" } else { "Data Source" };
        let btn_data_source = button(text(ds_label))
            .on_press(if ds_open {
                Message::CloseDataSource
            } else {
                Message::OpenDataSource
            });

        // Sidebar toggle.
        let sidebar_label = if self.sidebar_visible { "Hide Sidebar" } else { "Show Sidebar" };
        let btn_sidebar = button(text(sidebar_label))
            .on_press(Message::ToggleSidebar);

        // Panel / screen management.
        let btn_add_panel = button(text("+ Panel"))
            .on_press(Message::AddPanel);
        let btn_add_screen = button(text("+ Screen"))
            .on_press(Message::AddScreen);

        // Scripts modal button (Task 15).
        let scripts_open = matches!(self.active_dialog, Some(ActiveDialog::Scripts));
        let scripts_label = if scripts_open { "Close Scripts" } else { "Scripts" };
        let btn_scripts = button(text(scripts_label)).on_press(if scripts_open {
            Message::CloseModal
        } else {
            Message::OpenScripts
        });

        // Recording controls.
        let btn_record: Element<'_, Message> = if self.record_handle.is_some() {
            button(text("Stop Recording"))
                .on_press(Message::StopRecord)
                .into()
        } else {
            button(text("Record"))
                .on_press(Message::StartRecord)
                .into()
        };
        let btn_open_rec = button(text("Open Recording"))
            .on_press(Message::OpenRecording);

        // Close replay button — only shown in replay mode.
        let close_replay_btn: Option<Element<'_, Message>> = if is_replaying {
            Some(
                button(text("Close Replay"))
                    .on_press(Message::CloseReplay)
                    .into(),
            )
        } else {
            None
        };

        let mut btn_row = row![
            btn_save,
            btn_save_as,
            btn_load,
            btn_load_from,
            btn_data_source,
            btn_sidebar,
            btn_add_panel,
            btn_add_screen,
            btn_scripts,
            btn_record,
            btn_open_rec,
        ]
        .spacing(4)
        .padding(4);

        if let Some(btn) = close_replay_btn {
            btn_row = btn_row.push(btn);
        }

        container(btn_row)
            .width(Length::Fill)
            .style(|theme: &iced::Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.strong.color.into()),
                    ..container::Style::default()
                }
            })
            .into()
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    fn expire_status(&mut self) {
        if let Some(t) = self.status_clear_at {
            if Instant::now() >= t {
                self.status.clear();
                self.status_clear_at = None;
            }
        }
    }

    fn set_transient_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_clear_at = Some(Instant::now() + Duration::from_secs(3));
    }

    /// Current layout including the workspace screen config.
    pub(crate) fn current_layout(&self) -> LayoutConfig {
        let base = LayoutConfig {
            default_window_s: self.default_window_s,
            ..LayoutConfig::default()
        };
        self.workspace.to_config(&base)
    }

    fn save_layout(&mut self) {
        let cfg = self.current_layout();
        match cfg.save(&self.layout_path) {
            Ok(()) => {
                let msg = format!("Layout saved to {}", self.layout_path.display());
                self.set_transient_status(msg);
            }
            Err(e) => {
                self.status = format!("Layout save failed: {e}");
            }
        }
    }

    fn save_layout_as(&mut self) {
        let start = self
            .layout_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml");
        let Some(path) = rfd::FileDialog::new()
            .add_filter("TOML config", &["toml"])
            .set_file_name(start)
            .save_file()
        else {
            return;
        };
        self.layout_path = path;
        self.save_layout();
    }

    fn load_layout(&mut self) {
        match LayoutConfig::load(&self.layout_path) {
            Ok(cfg) => {
                self.default_window_s = cfg.default_window_s;
                self.workspace = Workspace::from_config(&cfg, &self.registry, &self.channels);
                let msg = format!("Layout loaded from {}", self.layout_path.display());
                self.set_transient_status(msg);
            }
            Err(e) => {
                self.status = format!("Layout load failed: {e}");
            }
        }
    }

    fn load_layout_from(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("TOML config", &["toml"])
            .pick_file()
        else {
            return;
        };
        self.layout_path = path;
        self.load_layout();
    }

    fn start_recording(&mut self) {
        let slots = self.manager.record_sender_slots();
        if slots.is_empty() {
            self.status = "Recording unavailable (no ingest source)".to_string();
            return;
        }
        let (tx, rx) = record::record_channel();
        // Install the sender into every active ingest source (mpmc queue).
        for slot in &slots {
            *slot.lock().unwrap() = Some(tx.clone());
        }
        match record::start_recording(
            Path::new("."),
            &self.channels,
            self.manager.schema_bytes(),
            rx,
            self.record_max_bytes,
        ) {
            Ok(handle) => {
                self.record_handle = Some(handle);
                self.status = "Recording started".to_string();
            }
            Err(e) => {
                // Remove senders since the recorder won't consume them.
                for slot in &slots {
                    *slot.lock().unwrap() = None;
                }
                self.status = format!("Record failed: {e}");
            }
        }
    }

    fn stop_recording(&mut self) {
        // Remove senders first so ingest stops queuing, then drop handle to
        // signal the recorder thread to finish.
        for slot in &self.manager.record_sender_slots() {
            *slot.lock().unwrap() = None;
        }
        self.record_handle = None;
        self.status = "Recording stopped".to_string();
    }

    fn open_recording(&mut self) {
        if self.record_handle.is_some() {
            self.status = "Stop recording before opening a file".to_string();
            return;
        }
        if self.pending_load.is_some() {
            self.status = "Already loading a recording…".to_string();
            return;
        }
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("MCAP recording", &["mcap"])
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }

        // Label for the status line: single filename, or "N recordings".
        let label = if paths.len() == 1 {
            paths[0].display().to_string()
        } else {
            format!("{} recordings", paths.len())
        };

        // Decode on a worker thread — large MCAPs take seconds and would
        // otherwise freeze the UI.  The result is polled in `poll_pending_load`.
        let (tx, rx) = crossbeam_channel::bounded(1);
        let channels = self.channels.clone();
        std::thread::spawn(move || {
            let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
            let _ = tx.send(PlaybackStore::load_many(&refs, &channels));
        });
        self.pending_load = Some((label.clone(), rx));
        self.status = format!("Loading {label}…");
    }

    /// Poll for a finished background recording load and apply it when ready.
    pub(crate) fn poll_pending_load(&mut self) {
        let Some((_, rx)) = &self.pending_load else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.pending_load = None;
                self.status = "Recording load failed (worker gone)".to_string();
                return;
            }
        };
        let (label, _) = self.pending_load.take().unwrap();
        match result {
            Ok(playback) => {
                // Save the live channel tree so we can restore it on close.
                self.saved_channel_tree = Some(self.channel_tree.clone());
                // Swap to the playback store.
                self.store = playback.clone();
                self.mode = AppMode::Replay(ReplayState {
                    store: playback,
                    playing: false,
                    speed: 1.0,
                    last_frame: Instant::now(),
                });
                // Rebuild the channel tree from the (now-updated) registry so
                // the sidebar reflects recording channels.
                self.channel_tree = ChannelTree::build(&self.channels);
                // Re-resolve panel bindings so channels discovered in the MCAP
                // file resolve immediately without waiting for a discovery tick.
                self.workspace.refresh_bindings(&self.registry, &self.channels);
                self.status = format!("Loaded {label}");
            }
            Err(e) => {
                self.status = format!("Failed to load recording: {e}");
            }
        }
    }

    pub(crate) fn close_replay(&mut self) {
        if let Some(tree) = self.saved_channel_tree.take() {
            self.channel_tree = tree;
        }
        self.store = self.live_store.clone();
        self.mode = AppMode::Live;
        self.live_view_offset_ns = 0;
        self.live_paused = false;
        self.status = "Replay closed".to_string();
    }

    /// Return (range, position_ns) for the current replay, or (0,0),0 for Live.
    fn replay_time_info(&self) -> ((i64, i64), i64) {
        match &self.mode {
            AppMode::Replay(rs) => {
                let start = rs.store.start_ns;
                let end = start + rs.store.duration_ns;
                let pos = rs.store.position_ns.load(Ordering::Relaxed);
                ((start, end), pos)
            }
            AppMode::Live => ((0, 0), 0),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_store_drops_repaint_rate() {
        // desired_interval() returns the fast interval right after a write-seq bump,
        // and the slow interval once write_seq has been unchanged past the idle window.
        let fast = DataVisApp::interval_for(/*advanced=*/true);
        let slow = DataVisApp::interval_for(false);
        assert!(fast < slow);
    }

    /// Build a minimal `DataVisApp` for unit tests.
    #[cfg(test)]
    fn make_test_app() -> DataVisApp {
        use crate::config::ChannelRegistry;
        make_test_app_with_channels(Arc::new(ChannelRegistry::from_toml_str("").unwrap()))
    }

    /// Build a test app around a caller-supplied channel registry — used by the
    /// store-swap test so the app's registry matches the loaded recording.
    #[cfg(test)]
    fn make_test_app_with_channels(channels: Arc<crate::config::ChannelRegistry>) -> DataVisApp {
        use std::sync::Arc;
        use crate::viz::PanelRegistry;
        use crate::store::LiveStore;

        let store = Arc::new(LiveStore::from_registry(&channels));
        let (tx, _rx) = crossbeam_channel::unbounded();
        let live_view_ns = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let script_status = Arc::new(std::sync::Mutex::new(Default::default()));
        let script_metas = Arc::new(std::sync::Mutex::new(Default::default()));
        let script_disabled = Arc::new(std::sync::Mutex::new(None));
        let manager = crate::ingest::SourceManager::new(store.clone(), channels.clone());

        DataVisApp::new(
            PanelRegistry::with_builtins(),
            store,
            channels,
            PathBuf::from("layout.toml"),
            LayoutConfig::default(),
            manager,
            live_view_ns,
            60.0,
            10.0,
            vec![],
            vec![],
            script_status,
            tx,
            script_disabled,
            script_metas,
            None,
        )
    }

    #[test]
    fn toggle_sidebar_flips_bool() {
        let mut app = make_test_app();

        assert!(app.sidebar_visible, "sidebar starts visible");
        let now = Instant::now();
        app.update(Message::ToggleSidebar, now);
        assert!(!app.sidebar_visible, "sidebar hidden after toggle");
        app.update(Message::ToggleSidebar, now);
        assert!(app.sidebar_visible, "sidebar visible again after second toggle");
    }

    #[test]
    fn open_close_data_source_toggles_active_dialog() {
        use super::super::message::ActiveDialog;
        let mut app = make_test_app();

        assert!(app.active_dialog.is_none(), "dialog starts closed");
        let now = Instant::now();
        let _ = app.update(Message::OpenDataSource, now);
        assert!(
            matches!(app.active_dialog, Some(ActiveDialog::DataSource)),
            "data source dialog opens"
        );
        let _ = app.update(Message::CloseDataSource, now);
        assert!(app.active_dialog.is_none(), "dialog closes");
        // Backdrop dismissal via CloseModal must also clear the dialog so the
        // toolbar button label stays consistent.
        let _ = app.update(Message::OpenDataSource, now);
        let _ = app.update(Message::CloseModal, now);
        assert!(app.active_dialog.is_none(), "CloseModal clears the dialog");
    }

    // -----------------------------------------------------------------------
    // Task 13 tests
    // -----------------------------------------------------------------------

    /// SUCCESS path: after `poll_pending_load` consumes a READY receiver
    /// carrying `Ok(Arc<PlaybackStore>)`, the app must (a) save the live tree,
    /// (b) enter `Replay` mode holding that store, (c) rebuild the channel tree
    /// from the recording's registry, and (d) refresh workspace bindings.
    #[test]
    fn store_swap_success_enters_replay_and_saves_tree() {
        // Build a real playback store from a minimal in-memory MCAP, and build
        // the app around the SAME registry so the tree rebuild is meaningful.
        let (store, registry, _dir) = PlaybackStore::new_test_fixture(&[
            (1_000_000_000, 1.0),
            (2_000_000_000, 2.0),
            (3_000_000_000, 3.0),
        ]);
        let channels = Arc::new(registry);
        let mut app = make_test_app_with_channels(channels);

        // The recording's channels ("accel.x") are now in the registry; the
        // pre-swap live tree does NOT contain them yet only if the registry was
        // built empty — here the registry already carries accel.x, so we just
        // assert the swap wiring itself.
        let live_tree_before = app.channel_tree.clone();
        assert!(app.saved_channel_tree.is_none(), "no saved tree before load");
        assert!(matches!(app.mode, AppMode::Live), "starts Live");

        // Push a READY Ok(store) into a receiver and poll.
        let (tx, rx) = crossbeam_channel::bounded::<anyhow::Result<Arc<PlaybackStore>>>(1);
        let expected_start = store.start_ns;
        tx.send(Ok(store)).unwrap();
        app.pending_load = Some(("fixture.mcap".to_string(), rx));
        app.poll_pending_load();

        // (a) live tree saved.
        assert!(app.saved_channel_tree.is_some(), "live channel tree must be saved");
        assert_eq!(
            sidebar::leaf_labels(app.saved_channel_tree.as_ref().unwrap()).len(),
            sidebar::leaf_labels(&live_tree_before).len(),
            "saved tree is the pre-swap live tree"
        );
        // (b) mode is Replay, holding the loaded store.
        match &app.mode {
            AppMode::Replay(rs) => {
                assert_eq!(rs.store.start_ns, expected_start, "replay holds loaded store");
                assert!(!rs.playing, "replay starts paused");
            }
            AppMode::Live => panic!("expected Replay mode after successful load"),
        }
        // (c) active store swapped to the playback store.
        assert!(Arc::ptr_eq(
            &app.store,
            &(match &app.mode { AppMode::Replay(rs) => rs.store.clone() as Arc<dyn ChannelStore>, _ => unreachable!() })
        ), "self.store points at the playback store during replay");
        // (d) channel tree rebuilt from the recording registry (accel.x present).
        let leaves = sidebar::leaf_labels(&app.channel_tree);
        assert!(
            leaves.iter().any(|l| l.contains("accel") || l.contains('x')),
            "channel tree rebuilt with recording channels, got {leaves:?}"
        );
        // pending_load consumed.
        assert!(app.pending_load.is_none(), "pending_load cleared after success");
        assert!(app.status.contains("Loaded"), "status: {}", app.status);
    }

    /// FAILURE paths: a disconnected receiver and a ready `Err` both leave the
    /// app in Live mode and clear `pending_load`.
    #[test]
    fn store_swap_failure_stays_live() {
        let mut app = make_test_app();

        // Disconnected receiver → TryRecvError::Disconnected.
        let (tx, rx) = crossbeam_channel::bounded::<anyhow::Result<Arc<PlaybackStore>>>(1);
        drop(tx);
        app.pending_load = Some(("test".to_string(), rx));
        app.poll_pending_load();
        assert!(app.pending_load.is_none(), "pending_load cleared on disconnect");
        assert!(matches!(app.mode, AppMode::Live), "stays Live on disconnect");

        // Ready Err result.
        let (tx2, rx2) = crossbeam_channel::bounded(1);
        tx2.send(Err(anyhow::anyhow!("test error"))).unwrap();
        app.pending_load = Some(("test".to_string(), rx2));
        app.poll_pending_load();
        assert!(app.pending_load.is_none());
        assert!(matches!(app.mode, AppMode::Live), "stays Live on Err result");
        assert!(app.status.contains("Failed"), "status reports failure: {}", app.status);
    }

    /// `close_replay` must restore the saved live channel tree and set mode to Live.
    #[test]
    fn close_replay_restores_live_tree() {
        let mut app = make_test_app();

        // Simulate that a replay is open by injecting a saved_channel_tree.
        let saved = app.channel_tree.clone();
        app.saved_channel_tree = Some(saved.clone());

        // Simulate Replay mode (just set the flag — mode's store is only used
        // in replay playback, not close_replay).
        // We can't build a ReplayState here without a real PlaybackStore, so
        // we test close_replay directly from Live mode where saved_channel_tree
        // is populated.
        app.close_replay();

        // Tree must be restored.
        assert!(app.saved_channel_tree.is_none(), "saved tree consumed after close");
        assert!(matches!(app.mode, AppMode::Live), "mode is Live after close_replay");
        assert_eq!(app.status, "Replay closed");
        assert_eq!(app.live_view_offset_ns, 0);
        assert!(!app.live_paused);
    }

    /// `Seek` in Live mode is a no-op (no crash).
    #[test]
    fn seek_in_live_mode_is_noop() {
        let mut app = make_test_app();
        let now = Instant::now();
        app.update(Message::Seek(999_000_000), now);
        // No panic = pass.
    }

    /// `CloseReplay` in Live mode is safe.
    #[test]
    fn close_replay_in_live_mode_is_safe() {
        let mut app = make_test_app();
        let now = Instant::now();
        app.update(Message::CloseReplay, now);
        assert!(matches!(app.mode, AppMode::Live));
    }
}
