use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::channel_tree::ChannelTree;
use crate::config::{ChannelRegistry, LayoutConfig};
use crate::record::playback::PlaybackStore;
use crate::record::RecordHandle;
use crate::store::ChannelStore;
use crate::script::config::ScriptInstance;
use crate::script::{ScriptCommand, SharedMetas, SharedStatus};

use super::message::Message;

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
/// registry / workspace / add_panel / add_source_form are dropped here
/// and reintroduced by Tasks 3/5/12/14.
pub struct DataVisApp {
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
    pub(crate) default_window_s: f64,
    pub(crate) _available_scripts: Vec<String>,
    pub(crate) script_instances: Vec<ScriptInstance>,
    pub(crate) script_metas: SharedMetas,
    pub(crate) script_status: SharedStatus,
    pub(crate) script_commands: crossbeam_channel::Sender<ScriptCommand>,
    pub(crate) script_disabled: Arc<Mutex<Option<String>>>,
    pub(crate) record_max_bytes: Option<u64>,
    pub(crate) data_source_window_open: bool,
}

impl DataVisApp {
    pub fn new(
        store: Arc<dyn ChannelStore>,
        channels: Arc<ChannelRegistry>,
        layout_path: PathBuf,
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
        Self {
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
            default_window_s,
            _available_scripts: available_scripts,
            script_instances,
            script_metas,
            script_status,
            script_commands,
            script_disabled,
            record_max_bytes,
            data_source_window_open: false,
        }
    }

    pub fn update(&mut self, message: Message, _now: std::time::Instant) -> iced::Task<Message> {
        match message {
            Message::Tick => {
                self.expire_status();
                iced::Task::none()
            }
        }
    }

    pub fn view(&self) -> iced::Element<'_, Message> {
        // Task 4 fills the real layout; for now a labelled placeholder proves launch.
        iced::widget::container(iced::widget::text(format!("datavis — {}", self.status)))
            .center(iced::Length::Fill)
            .into()
    }

    pub fn subscription(&self) -> iced::Subscription<Message> {
        iced::time::every(std::time::Duration::from_millis(250)).map(|_| Message::Tick)
    }

    fn expire_status(&mut self) {
        if let Some(t) = self.status_clear_at {
            if std::time::Instant::now() >= t {
                self.status.clear();
                self.status_clear_at = None;
            }
        }
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
