use std::sync::Arc;
use crossbeam_channel::{Receiver, Sender};

pub type RecordMsg = (Arc<str>, Vec<u8>, i64);

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
        tx.try_send((topic.clone(), data.clone(), 42_000_000)).unwrap();
        let (t, d, n) = rx.try_recv().unwrap();
        assert_eq!(t.as_ref(), "accel");
        assert_eq!(d, data);
        assert_eq!(n, 42_000_000);
    }

    #[test]
    fn queue_full_returns_err_does_not_block() {
        let (tx, _rx) = record_channel();
        for i in 0..QUEUE_CAP {
            tx.try_send((Arc::from("t"), vec![i as u8], i as i64)).unwrap();
        }
        assert!(tx.try_send((Arc::from("t"), vec![], 0)).is_err());
    }
}
