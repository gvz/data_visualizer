use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::decode_buf::DecodedChunk;

struct Entry {
    chunk: Arc<DecodedChunk>,
    bytes: usize,
    last_used: u64,
}

struct Inner {
    map: HashMap<(usize, usize), Entry>,
    tick: u64,
    retained: usize,
}

/// Byte-budgeted LRU of decoded chunks keyed by `(source, chunk)`. Bounds
/// worst-case resident RAM independent of file size: once the retained byte
/// total exceeds `cap_bytes`, least-recently-used entries are evicted (never
/// the entry just inserted).
pub struct ChunkCache {
    cap_bytes: usize,
    inner: Mutex<Inner>,
}

impl ChunkCache {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            cap_bytes,
            inner: Mutex::new(Inner { map: HashMap::new(), tick: 0, retained: 0 }),
        }
    }

    pub fn retained_bytes(&self) -> usize {
        self.inner.lock().unwrap().retained
    }

    pub fn get_or_insert_with(
        &self,
        key: (usize, usize),
        make: impl FnOnce() -> DecodedChunk,
    ) -> Arc<DecodedChunk> {
        let mut inner = self.inner.lock().unwrap();
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(e) = inner.map.get_mut(&key) {
            e.last_used = tick;
            return e.chunk.clone();
        }
        let chunk = Arc::new(make());
        let bytes = chunk.bytes();
        inner.retained += bytes;
        inner.map.insert(key, Entry { chunk: chunk.clone(), bytes, last_used: tick });
        // Evict least-recently-used until under cap, but never the entry we
        // just inserted (its last_used == tick is the maximum).
        while inner.retained > self.cap_bytes {
            let Some((&victim, _)) = inner
                .map
                .iter()
                .filter(|(&k, _)| k != key)
                .min_by_key(|(_, e)| e.last_used)
            else {
                break; // only the fresh entry remains
            };
            if let Some(e) = inner.map.remove(&victim) {
                inner.retained -= e.bytes;
            }
        }
        chunk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::lazy::decode_buf::DecodedChunk;

    // A decoded chunk of a chosen byte weight: N float samples ≈ N*16 bytes.
    fn chunk(n: usize) -> DecodedChunk {
        // Build via ChunkDecodeBuf so bytes() matches production accounting.
        use crate::config::ChannelRegistry;
        use crate::store::ChannelStore;
        use crate::types::NumericVal;
        let r = ChannelRegistry::from_toml_str(
            r#"
[channels."a.f"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#,
        )
        .unwrap();
        let buf = crate::record::lazy::decode_buf::ChunkDecodeBuf::new(&r);
        let id = r.id("a.f").unwrap();
        for i in 0..n as i64 {
            buf.write_numeric(id, i, NumericVal::Float(i as f64));
        }
        buf.freeze()
    }

    #[test]
    fn caches_and_reuses() {
        let cache = ChunkCache::new(10 * 1024 * 1024);
        let mut built = 0;
        let a = cache.get_or_insert_with((0, 0), || {
            built += 1;
            chunk(100)
        });
        let b = cache.get_or_insert_with((0, 0), || {
            built += 1;
            chunk(100)
        });
        assert_eq!(built, 1, "second get must hit the cache");
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn evicts_to_stay_under_cap() {
        // Cap holds ~one 1000-sample chunk (16KB). Insert three distinct chunks.
        let cap = chunk(1000).bytes() + 8;
        let cache = ChunkCache::new(cap);
        let _ = cache.get_or_insert_with((0, 0), || chunk(1000));
        let _ = cache.get_or_insert_with((0, 1), || chunk(1000));
        let _ = cache.get_or_insert_with((0, 2), || chunk(1000));
        assert!(
            cache.retained_bytes() <= cap,
            "retained {} > cap {}",
            cache.retained_bytes(),
            cap
        );
    }
}
