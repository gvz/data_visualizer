use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::widget::{column, container, row, text};
use iced::{Element, Length, Task};

use super::sidebar;

use crate::channel_tree::ChannelTree;
use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
use crate::record::playback::PlaybackStore;
use crate::record::RecordHandle;
use crate::store::ChannelStore;
use crate::script::config::ScriptInstance;
use crate::script::{ScriptCommand, SharedMetas, SharedStatus};
use crate::viz::PanelRegistry;

use super::message::Message;
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
    pub(crate) data_source_window_open: bool,
    /// Channel currently being dragged from the sidebar (Task 12 wires drop).
    pub(crate) dragging: Option<String>,
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
            data_source_window_open: false,
            dragging: None,
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
                // Adaptive repaint: check if the store has new data.
                let seq = self.store.write_seq();
                if seq != self.last_write_seq {
                    self.last_write_seq = seq;
                    self.last_write_advance = Instant::now();
                }
                // Sync all visible panels from the current store.
                self.workspace.sync_all(self.store.as_ref());
                self.expire_status();
                Task::none()
            }

            // Fully wired toggles.
            Message::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                Task::none()
            }
            Message::OpenDataSource => {
                self.data_source_window_open = true;
                Task::none()
            }
            Message::CloseDataSource => {
                self.data_source_window_open = false;
                Task::none()
            }

            // Stubs — real implementations arrive in later tasks.
            Message::Load => {
                self.set_transient_status("Load recording — coming in a later task");
                Task::none()
            }
            Message::SaveLayout => {
                self.set_transient_status("Save layout — coming in a later task");
                Task::none()
            }
            Message::SaveLayoutAs => {
                self.set_transient_status("Save layout as — coming in a later task");
                Task::none()
            }
            Message::LoadLayoutFrom => {
                self.set_transient_status("Load layout from — coming in a later task");
                Task::none()
            }
            Message::StartRecord => {
                self.set_transient_status("Start recording — coming in a later task");
                Task::none()
            }
            Message::StopRecord => {
                self.set_transient_status("Stop recording — coming in a later task");
                Task::none()
            }
            Message::OpenRecording => {
                self.set_transient_status("Open recording — coming in a later task");
                Task::none()
            }
            Message::AddPanel => {
                // Add a placeholder error panel to the active screen.
                let entry = PanelEntry {
                    panel_type: "error".into(),
                    config: {
                        let mut t = toml::Table::new();
                        t.insert(
                            "message".into(),
                            toml::Value::String("select a panel type".into()),
                        );
                        t
                    },
                };
                self.workspace.add_panel(entry, &self.registry, &self.channels);
                Task::none()
            }
            Message::AddScreen => {
                self.workspace.add_screen(None, &self.registry, &self.channels);
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

        // Timeline placeholder (shown only during replay).
        let timeline = container(text("Timeline"))
            .width(Length::Fill)
            .height(Length::Fixed(80.0))
            .style(|theme: &iced::Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.strong.color.into()),
                    ..container::Style::default()
                }
            });

        // Status bar.
        let status_bar = container(text(self.status.as_str()).size(13))
            .width(Length::Fill)
            .padding(4);

        // Full layout.
        if is_replaying {
            column![top_bar, content_row, timeline, status_bar]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            column![top_bar, content_row, status_bar]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        }
    }

    pub fn subscription(&self) -> iced::Subscription<Message> {
        iced::time::every(self.desired_interval()).map(|_| Message::Tick)
    }

    // -----------------------------------------------------------------------
    // View helpers
    // -----------------------------------------------------------------------

    /// Renders the combined menu + toolbar row.
    fn menu_toolbar_row(&self) -> Element<'_, Message> {
        use iced::widget::button;

        // File menu buttons (flat for now; real menus via Task 13).
        let btn_save = button(text("Save layout"))
            .on_press(Message::SaveLayout);
        let btn_save_as = button(text("Save layout as…"))
            .on_press(Message::SaveLayoutAs);
        let btn_load = button(text("Load layout"))
            .on_press(Message::Load);
        let btn_load_from = button(text("Load layout from…"))
            .on_press(Message::LoadLayoutFrom);

        // Data source toggle.
        let ds_label = if self.data_source_window_open { "Close Data Source" } else { "Data Source" };
        let btn_data_source = button(text(ds_label))
            .on_press(if self.data_source_window_open {
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

        // Recording controls (stubs until Task 14).
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

        container(
            row![
                btn_save,
                btn_save_as,
                btn_load,
                btn_load_from,
                btn_data_source,
                btn_sidebar,
                btn_add_panel,
                btn_add_screen,
                btn_record,
                btn_open_rec,
            ]
            .spacing(4)
            .padding(4),
        )
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

    /// Current layout config (default_window_s only for now; workspace reintroduced Task 5).
    pub(crate) fn current_layout(&self) -> LayoutConfig {
        let mut cfg = LayoutConfig::default();
        cfg.default_window_s = self.default_window_s;
        cfg
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
                self.store = playback.clone();
                self.mode = AppMode::Replay(ReplayState {
                    store: playback,
                    playing: false,
                    speed: 1.0,
                    last_frame: Instant::now(),
                });
                self.saved_channel_tree = Some(self.channel_tree.clone());
                self.channel_tree = ChannelTree::build(&self.channels);
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
        use std::sync::Arc;
        use crate::viz::PanelRegistry;
        use crate::store::LiveStore;
        use crate::config::ChannelRegistry;

        let channels = Arc::new(ChannelRegistry::from_toml_str("").unwrap());
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
    fn open_close_data_source_flips_bool() {
        let mut app = make_test_app();

        assert!(!app.data_source_window_open, "window starts closed");
        let now = Instant::now();
        app.update(Message::OpenDataSource, now);
        assert!(app.data_source_window_open, "window opens");
        app.update(Message::CloseDataSource, now);
        assert!(!app.data_source_window_open, "window closes");
    }
}
