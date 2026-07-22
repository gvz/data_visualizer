use std::sync::Arc;
use crossbeam_channel::{Receiver, Sender};

/// A frame queued for the MCAP recorder.
///
/// `Proto` carries a message encoded against the shared schema registered when
/// recording starts (the ZMQ ingest path). `DynamicProto` carries its own
/// self-contained protobuf schema (the MQTT path, where schemas are generated
/// per topic at runtime).
#[derive(Debug, Clone)]
pub enum RecordMsg {
    Proto {
        topic: Arc<str>,
        data: Vec<u8>,
        ts: i64,
    },
    DynamicProto {
        topic: Arc<str>,
        schema: Arc<[u8]>,
        data: Vec<u8>,
        ts: i64,
    },
}

pub const QUEUE_CAP: usize = 8192;

pub fn record_channel() -> (Sender<RecordMsg>, Receiver<RecordMsg>) {
    crossbeam_channel::bounded(QUEUE_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_roundtrip() {
        let (tx, rx) = record_channel();
        let topic: Arc<str> = Arc::from("accel");
        let data = vec![1u8, 2, 3];
        tx.try_send(RecordMsg::Proto { topic: topic.clone(), data: data.clone(), ts: 42_000_000 })
            .unwrap();
        match rx.try_recv().unwrap() {
            RecordMsg::Proto { topic: t, data: d, ts } => {
                assert_eq!(t.as_ref(), "accel");
                assert_eq!(d, data);
                assert_eq!(ts, 42_000_000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn queue_full_returns_err_does_not_block() {
        let (tx, _rx) = record_channel();
        for i in 0..QUEUE_CAP {
            tx.try_send(RecordMsg::Proto { topic: Arc::from("t"), data: vec![i as u8], ts: i as i64 })
                .unwrap();
        }
        assert!(tx
            .try_send(RecordMsg::Proto { topic: Arc::from("t"), data: vec![], ts: 0 })
            .is_err());
    }
}
