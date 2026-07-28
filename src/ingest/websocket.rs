use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
}

impl WsSource {
    /// Create a new WebSocket source from `config`, pre-seeding the topic map
    /// from any `mqtt_topic` channels already in `registry`.
    pub fn new(config: WsConfig, registry: &ChannelRegistry) -> Self {
        Self { config, topic_map: topic_map_from_registry(registry) }
    }
}

impl DataSource for WsSource {
    fn name(&self) -> &str {
        "websocket"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let discovered: Arc<Mutex<BTreeMap<String, String>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>> = Arc::new(Mutex::new(None));
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));

        let listen = self.config.listen.clone();
        let map = self.topic_map.clone();
        let disc = discovered.clone();
        let rec = record_sender.clone();
        let state = conn_state.clone();
        std::thread::spawn(move || {
            accept_loop(listen, map, disc, store, rec, state);
        });

        SourceHandle {
            name: "websocket".to_string(),
            conn_state,
            record_sender,
            discovery: Some(Discovery { discovered, topic_map: self.topic_map }),
            schema_bytes: None,
        }
    }
}

/// Bind and accept connections, one serving thread per client. A bind failure
/// logs and exits the thread (app keeps running, conn_state stays CONNECTING).
fn accept_loop(
    listen: String,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    conn_state: Arc<AtomicU8>,
) {
    let listener = match TcpListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("websocket: bind {listen} failed: {e}");
            return;
        }
    };
    let clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("websocket: accept failed: {e}");
                continue;
            }
        };
        let map = topic_map.clone();
        let disc = discovered.clone();
        let store = store.clone();
        let rec = record_sender.clone();
        let state = conn_state.clone();
        let clients = clients.clone();
        std::thread::spawn(move || {
            serve_client(stream, map, disc, store, rec, state, clients);
        });
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
) {
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("websocket: handshake failed: {e}");
            return;
        }
    };
    if clients.fetch_add(1, Ordering::Relaxed) == 0 {
        conn_state.store(LIVE, Ordering::Relaxed);
    }

    let mut ingest = ScalarIngest::new(discovered, topic_map, store, record_sender);
    loop {
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
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
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
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, s_map, s_disc, s_store, s_rec, s_state, s_clients);
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
}
