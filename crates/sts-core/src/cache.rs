//! Deterministic LRU translation cache keyed by `(source_lang, target_lang, text)`.

use std::num::NonZeroUsize;

use lru::LruCache;
use sha2::{Digest, Sha256};

fn cache_key(source_lang: &str, target_lang: &str, text: &str) -> u64 {
    let mut hasher = Sha256::new();
    for part in [source_lang, target_lang, text] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = digest;
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

/// In-memory LRU cache mapping `(source_lang, target_lang, text)` hashes to translated strings.
#[derive(Debug)]
pub struct TranslationCache {
    inner: LruCache<u64, String>,
}

impl TranslationCache {
    /// Creates a new LRU cache with capacity clamped to at least `1`.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: LruCache::new(cap),
        }
    }

    /// Inserts or updates a translation for `(source_lang, target_lang, text)`.
    pub fn insert(&mut self, source_lang: &str, target_lang: &str, text: &str, translated: String) {
        let key = cache_key(source_lang, target_lang, text);
        self.inner.put(key, translated);
    }

    /// Looks up a cached translation, marking it as most-recently-used on hit.
    pub fn get(&mut self, source_lang: &str, target_lang: &str, text: &str) -> Option<&str> {
        let key = cache_key(source_lang, target_lang, text);
        self.inner.get(&key).map(String::as_str)
    }

    /// Returns the current number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` when the cache contains no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the maximum capacity of the cache.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.cap().get()
    }

    /// Removes all entries from the cache.
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translation_cache_hits_same_text_and_langs_and_evicts_lru() {
        let mut cache = TranslationCache::new(2);
        cache.insert("ja", "vi", "おはよう", "Chào buổi sáng".into());
        cache.insert("ja", "vi", "ありがとう", "Cảm ơn".into());
        assert_eq!(
            cache.get("ja", "vi", "おはよう").map(str::to_owned),
            Some("Chào buổi sáng".into())
        );
        assert_eq!(cache.get("en", "vi", "おはよう"), None);
        cache.insert("ja", "vi", "さようなら", "Tạm biệt".into());
        assert_eq!(cache.get("ja", "vi", "ありがとう"), None); // evicted
        assert_eq!(cache.get("ja", "vi", "さようなら"), Some("Tạm biệt"));
    }

    #[test]
    fn translation_cache_clamps_zero_capacity_to_one() {
        let mut cache = TranslationCache::new(0);
        assert_eq!(cache.capacity(), 1);
        assert!(cache.is_empty());

        cache.insert("en", "vi", "hello", "xin chào".into());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("en", "vi", "hello"), Some("xin chào"));

        cache.insert("en", "vi", "world", "thế giới".into());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("en", "vi", "hello"), None);
        assert_eq!(cache.get("en", "vi", "world"), Some("thế giới"));
    }

    #[test]
    fn translation_cache_distinguishes_key_field_boundaries() {
        let mut cache = TranslationCache::new(8);
        cache.insert("a", "bc", "d", "first".into());
        cache.insert("ab", "c", "d", "second".into());
        cache.insert("a", "b", "cd", "third".into());

        assert_eq!(cache.get("a", "bc", "d"), Some("first"));
        assert_eq!(cache.get("ab", "c", "d"), Some("second"));
        assert_eq!(cache.get("a", "b", "cd"), Some("third"));

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get("a", "bc", "d"), None);
    }
}
