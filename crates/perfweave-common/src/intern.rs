//! String interning. Names (kernel names, API names, metric names) are
//! referenced by a 64-bit xxh3 hash stored in events; the first time an agent
//! emits a hash it also sends the full text, and the collector persists it in
//! the `strings` dictionary table. This keeps events narrow without paying a
//! lookup round-trip on the write path.

use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

use crate::proto::v1::StringIntern;

/// xxh3-64 hash of a string. Collisions at 64 bits are negligible at our
/// expected cardinality (<10^7 unique names per cluster-day).
#[inline]
pub fn hash(s: &str) -> u64 {
    xxh3_64(s.as_bytes())
}

/// Agent-side interner: tracks which ids have already been announced so we
/// only send `StringIntern` once per name per connection.
#[derive(Clone)]
pub struct Interner {
    seen: Arc<Mutex<HashSet<u64>>>,
}

impl Interner {
    pub fn new() -> Self {
        Self { seen: Arc::new(Mutex::new(HashSet::new())) }
    }

    /// Hash the text; return (id, optional_intern_record_to_send_first_time).
    pub fn record(&self, text: &str) -> (u64, Option<StringIntern>) {
        let id = hash(text);
        let mut seen = self.seen.lock();
        if seen.insert(id) {
            (id, Some(StringIntern { id, text: text.to_string() }))
        } else {
            (id, None)
        }
    }

    /// Reset after reconnection — the collector may have been restarted and
    /// lost its in-memory dictionary.
    pub fn reset(&self) {
        self.seen.lock().clear();
    }
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interner_emits_once() {
        let i = Interner::new();
        let (id1, rec1) = i.record("cudaMalloc");
        assert!(rec1.is_some());
        let (id2, rec2) = i.record("cudaMalloc");
        assert_eq!(id1, id2);
        assert!(rec2.is_none());
    }

    #[test]
    fn different_names_different_ids() {
        assert_ne!(hash("cudaMalloc"), hash("cudaFree"));
    }
}
