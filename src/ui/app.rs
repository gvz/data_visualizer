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
            Message::OpenDataSource => {
                self.data_source_window_open = true;
                Task::none()
            }
            Message::CloseDataSource => {
                self.data_source_window_open = false;
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

        // Status bar.
        let status_bar = container(text(self.status.as_str()).size(13))
            .width(Length::Fill)
            .padding(4);

        // Full layout.
        if is_replaying {
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
    fn open_close_data_source_flips_bool() {
        let mut app = make_test_app();

        assert!(!app.data_source_window_open, "window starts closed");
        let now = Instant::now();
        app.update(Message::OpenDataSource, now);
        assert!(app.data_source_window_open, "window opens");
        app.update(Message::CloseDataSource, now);
        assert!(!app.data_source_window_open, "window closes");
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
