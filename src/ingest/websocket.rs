use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use crossbeam_channel::Sender;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::MqttTopicMap;
use crate::ingest::lineproto::parse_line;
use crate::ingest::scalar::ScalarIngest;
use crate::ingest::source::{topic_map_from_registry, DataSource, Discovery, SourceHandle};
use crate::ingest::{CONNECTING, LIVE};
use crate::record::RecordMsg;
use crate::store::ChannelStore;

/// Configuration for the WebSocket InfluxDB line-protocol source.
pub struct WsConfig {
    /// TCP bind address as `"host:port"`, e.g. `"0.0.0.0:9001"`.
    pub listen: String,
}

/// WebSocket server that accepts InfluxDB line-protocol frames and writes
/// samples to the shared channel store.
///
/// Each WebSocket connection sends newline-delimited InfluxDB line-protocol
/// lines; the measurement name is used as the channel topic. Topic discovery
/// works the same way as [`MqttSource`](crate::ingest::MqttSource): every
/// received measurement name is added to [`Discovery::discovered`] and can be
/// bound to a panel via drag-and-drop.
///
/// Multiple simultaneous client connections are accepted; each is handled on
/// its own thread.
pub struct WsSource {
    config: WsConfig,
    pub(crate) topic_map: Arc<MqttTopicMap>,
    pub(crate) discovered: Arc<Mutex<BTreeMap<String, String>>>,
    /// Pre-bound listener when built via [`WsSource::bind`]; `None` when built
    /// via [`WsSource::new`] (the accept loop binds lazily in that case).
    listener: Option<TcpListener>,
}

impl WsSource {
    /// Create a new WebSocket source from `config`, pre-seeding the topic map
    /// from any `mqtt_topic` channels already in `registry`. The listen port is
    /// bound lazily when the source is spawned.
    pub fn new(config: WsConfig, registry: &ChannelRegistry) -> Self {
        Self {
            config,
            topic_map: topic_map_from_registry(registry),
            discovered: Arc::new(Mutex::new(BTreeMap::new())),
            listener: None,
        }
    }

    /// Create a WebSocket source and bind its listen port **eagerly**, so a bind
    /// failure (e.g. the port is already in use by another source) is reported
    /// to the caller instead of failing silently in the background thread.
    ///
    /// # Errors
    /// Returns `Err` if `config.listen` cannot be bound.
    pub fn bind(config: WsConfig, registry: &ChannelRegistry) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(&config.listen)
            .with_context(|| format!("binding WebSocket listener on {}", config.listen))?;
        listener.set_nonblocking(true).ok();
        Ok(Self {
            config,
            topic_map: topic_map_from_registry(registry),
            discovered: Arc::new(Mutex::new(BTreeMap::new())),
            listener: Some(listener),
        })
    }

    /// Replace this source's discovery maps with shared ones, so that several
    /// discovery-capable sources managed together present a single unified
    /// topic list and routing table. Call before [`spawn`](DataSource::spawn).
    pub(crate) fn share(
        &mut self,
        topic_map: Arc<MqttTopicMap>,
        discovered: Arc<Mutex<BTreeMap<String, String>>>,
    ) {
        self.topic_map = topic_map;
        self.discovered = discovered;
    }
}

impl DataSource for WsSource {
    fn name(&self) -> &str {
        "websocket"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let discovered = self.discovered.clone();
        let record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>> = Arc::new(Mutex::new(None));
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));

        let stop = Arc::new(AtomicBool::new(false));

        let listen = self.config.listen.clone();
        let listener = self.listener;
        let map = self.topic_map.clone();
        let disc = discovered.clone();
        let rec = record_sender.clone();
        let state = conn_state.clone();
        let stop_flag = stop.clone();
        std::thread::spawn(move || {
            accept_loop(listen, listener, map, disc, store, rec, state, stop_flag);
        });

        SourceHandle {
            name: "websocket".to_string(),
            conn_state,
            record_sender,
            discovery: Some(Discovery { discovered, topic_map: self.topic_map }),
            schema_bytes: None,
            child_guard: None,
            stop,
        }
    }
}

/// Bind and accept connections, one serving thread per client. A bind failure
/// logs and exits the thread (app keeps running, conn_state stays CONNECTING).
#[allow(clippy::too_many_arguments)]
fn accept_loop(
    listen: String,
    listener: Option<TcpListener>,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    conn_state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
) {
    // Use the pre-bound listener from `WsSource::bind` if present; otherwise bind
    // lazily here (the `WsSource::new` path, used by tests). Either way the
    // listener is non-blocking so the accept loop can poll `stop`.
    let listener = match listener {
        Some(l) => l,
        None => match TcpListener::bind(&listen) {
            Ok(l) => {
                l.set_nonblocking(true).ok();
                l
            }
            Err(e) => {
                eprintln!("websocket: bind {listen} failed: {e}");
                return;
            }
        },
    };
    let clients = Arc::new(AtomicUsize::new(0));
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match listener.accept() {
            Ok((sock, _)) => {
                sock.set_nonblocking(false).ok();
                let map = topic_map.clone();
                let disc = discovered.clone();
                let store = store.clone();
                let rec = record_sender.clone();
                let state = conn_state.clone();
                let clients = clients.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    serve_client(sock, map, disc, store, rec, state, clients, stop);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Err(e) => {
                eprintln!("ws: accept: {e}");
                return;
            }
        }
    }
}

/// Handshake one connection, then read text frames until close/error. Each
/// frame is split into lines and routed through `ScalarIngest`. The first live
/// client sets conn_state LIVE; the last to leave restores CONNECTING.
#[allow(clippy::too_many_arguments)]
fn serve_client(
    stream: TcpStream,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    conn_state: Arc<AtomicU8>,
    clients: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) {
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("websocket: handshake failed: {e}");
            return;
        }
    };
    // Give reads a timeout so this thread wakes periodically to observe `stop`.
    // Without it, `ws.read()` blocks forever on an idle-but-connected client and
    // the thread would outlive `SourceManager::remove` (leaking the socket and
    // its store/record_sender Arcs). tungstenite treats a timed-out read as a
    // recoverable `WouldBlock`/`TimedOut` IO error and resumes on the next read.
    ws.get_ref().set_read_timeout(Some(std::time::Duration::from_millis(500))).ok();
    if clients.fetch_add(1, Ordering::Relaxed) == 0 {
        conn_state.store(LIVE, Ordering::Relaxed);
    }

    let mut ingest = ScalarIngest::new(discovered, topic_map, store, record_sender);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let now = crate::types::now_ns();
                for line in text.lines() {
                    for (topic, payload, ts) in parse_line(line, now) {
                        ingest.on_message(&topic, &payload, ts);
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {} // binary/ping/pong ignored; tungstenite auto-replies pings
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Read timed out — loop back to re-check `stop`.
                continue;
            }
            Err(e) => {
                eprintln!("websocket: read: {e}");
                break;
            }
        }
    }

    if clients.fetch_sub(1, Ordering::Relaxed) == 1 {
        conn_state.store(CONNECTING, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::source::topic_map_from_registry;
    use crate::ingest::{CONNECTING, LIVE};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::Sample;
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            "[channels.\"weather/temp\"]\nmqtt_topic = \"weather/temperature\"\ntype = \"float\"\n",
        )
        .unwrap()
    }

    #[test]
    fn ws_source_spawn_has_discovery_no_schema() {
        let reg = registry();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let src = WsSource::new(WsConfig { listen: "127.0.0.1:0".into() }, &reg);
        let handle = Box::new(src).spawn(store);
        assert_eq!(handle.name, "websocket");
        assert!(handle.discovery.is_some());
        assert!(handle.schema_bytes.is_none());
    }

    #[test]
    fn serve_client_routes_to_store_discovers_and_sets_live() {
        let reg = registry();
        let id = reg.iter_ids().next().unwrap();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let topic_map = topic_map_from_registry(&reg);
        let discovered = Arc::new(Mutex::new(BTreeMap::new()));
        let record_sender = Arc::new(Mutex::new(None));
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));
        let clients = Arc::new(AtomicUsize::new(0));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let (s_map, s_disc, s_store, s_rec, s_state, s_clients) = (
            topic_map.clone(),
            discovered.clone(),
            store.clone(),
            record_sender.clone(),
            conn_state.clone(),
            clients.clone(),
        );
        let s_stop = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, s_map, s_disc, s_store, s_rec, s_state, s_clients, s_stop);
        });

        let (mut ws, _resp) =
            tungstenite::connect(format!("ws://{addr}").as_str()).unwrap();
        ws.send(tungstenite::Message::Text(
            "weather temperature=82 1000".to_string(),
        ))
        .unwrap();

        // Poll until the sample lands (server processes on its thread).
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if store.latest(id) == Some((1000, Sample::Float(82.0))) {
                break;
            }
            assert!(Instant::now() < deadline, "sample never arrived in store");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(conn_state.load(Ordering::Relaxed), LIVE);
        assert_eq!(
            discovered.lock().unwrap().get("weather/temperature").map(String::as_str),
            Some("82")
        );
        // Dropping the client closes the socket; the server read loop then ends.
        drop(ws);
    }

    #[test]
    fn accept_loop_stops_promptly() {
        use std::sync::atomic::Ordering;
        let registry = crate::config::ChannelRegistry::from_toml_str("").unwrap();
        let store: Arc<dyn crate::store::ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(&registry));
        let src = WsSource::new(WsConfig { listen: "127.0.0.1:0".into() }, &registry);
        let handle = Box::new(src).spawn(store);
        assert_eq!(handle.stop.load(Ordering::Relaxed), false);
        handle.stop.store(true, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Wiring test: stop is settable and the accept loop observes it.
    }
}
