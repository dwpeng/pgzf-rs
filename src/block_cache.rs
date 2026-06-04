use std::sync::Arc;

use lru::LruCache;

/// LRU block cache for decompressed PGZF blocks.
///
/// Caches decompressed block data indexed by block number. When the cache is
/// full (by count or memory), the least-recently-used entry is evicted.
/// Setting capacity to 0 disables caching entirely.
pub(crate) struct BlockCache {
    entries: LruCache<usize, CacheEntry>,
    capacity: usize,
    max_memory_bytes: Option<usize>,
    current_memory_bytes: usize,
}

struct CacheEntry {
    data: Arc<[u8]>,
    size: usize,
}

impl BlockCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: LruCache::new(std::num::NonZeroUsize::new(capacity.max(1)).unwrap_or(std::num::NonZeroUsize::MIN)),
            capacity,
            max_memory_bytes: None,
            current_memory_bytes: 0,
        }
    }

    /// Set the maximum memory limit for the cache in bytes.
    pub(crate) fn with_memory_limit(mut self, max_bytes: usize) -> Self {
        self.max_memory_bytes = Some(max_bytes);
        self
    }

    /// Retrieve cached data for `block_index`, updating access time on hit.
    /// Returns an `Arc` clone (cheap reference count bump, no data copy).
    pub(crate) fn get(&mut self, block_index: usize) -> Option<Arc<[u8]>> {
        if self.capacity == 0 {
            return None;
        }
        self.entries.get(&block_index).map(|entry| Arc::clone(&entry.data))
    }

    /// Insert decompressed `data` for `block_index`. Evicts the LRU entry if full.
    pub(crate) fn insert(&mut self, block_index: usize, data: Arc<[u8]>) {
        if self.capacity == 0 {
            return;
        }

        let size = data.len();

        // Remove existing entry if updating
        if let Some(old_entry) = self.entries.pop(&block_index) {
            self.current_memory_bytes -= old_entry.size;
        }

        // Evict LRU entries to satisfy memory limit
        if let Some(max_bytes) = self.max_memory_bytes {
            while self.current_memory_bytes + size > max_bytes && !self.entries.is_empty() {
                self.evict_one();
            }
        }

        // Evict LRU entries to satisfy capacity limit
        while self.entries.len() >= self.capacity {
            self.evict_one();
        }

        // Insert new entry
        self.current_memory_bytes += size;
        self.entries.put(block_index, CacheEntry { data, size });
    }

    /// Evict one LRU entry.
    fn evict_one(&mut self) {
        if let Some((_, entry)) = self.entries.pop_lru() {
            self.current_memory_bytes -= entry.size;
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns the current memory usage in bytes.
    pub(crate) fn memory_usage(&self) -> usize {
        self.current_memory_bytes
    }

    /// Returns the maximum memory limit in bytes, if set.
    #[allow(dead_code)]
    pub(crate) fn memory_limit(&self) -> Option<usize> {
        self.max_memory_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_basic_insert_get() {
        let mut cache = BlockCache::new(4);
        cache.insert(0, Arc::from(vec![1, 2, 3]));
        cache.insert(1, Arc::from(vec![4, 5, 6]));

        assert_eq!(cache.get(0).unwrap().as_ref(), &[1, 2, 3]);
        assert_eq!(cache.get(1).unwrap().as_ref(), &[4, 5, 6]);
        assert!(cache.get(2).is_none());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_lru_eviction() {
        let mut cache = BlockCache::new(2);
        cache.insert(0, Arc::from(vec![0]));
        cache.insert(1, Arc::from(vec![1]));
        // Cache full, inserting 2 should evict 0 (least recently used)
        cache.insert(2, Arc::from(vec![2]));

        assert!(cache.get(0).is_none());
        assert_eq!(cache.get(1).unwrap().as_ref(), &[1]);
        assert_eq!(cache.get(2).unwrap().as_ref(), &[2]);
    }

    #[test]
    fn test_access_refreshes_lru() {
        let mut cache = BlockCache::new(2);
        cache.insert(0, Arc::from(vec![0]));
        cache.insert(1, Arc::from(vec![1]));
        // Access block 0 to make it recently used
        cache.get(0);
        // Inserting 2 should evict 1 (now the LRU)
        cache.insert(2, Arc::from(vec![2]));

        assert_eq!(cache.get(0).unwrap().as_ref(), &[0]);
        assert!(cache.get(1).is_none());
        assert_eq!(cache.get(2).unwrap().as_ref(), &[2]);
    }

    #[test]
    fn test_zero_capacity_disabled() {
        let mut cache = BlockCache::new(0);
        cache.insert(0, Arc::from(vec![1, 2, 3]));
        assert!(cache.get(0).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_update_existing_entry() {
        let mut cache = BlockCache::new(2);
        cache.insert(0, Arc::from(vec![0]));
        cache.insert(0, Arc::from(vec![99]));
        assert_eq!(cache.get(0).unwrap().as_ref(), &[99]);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_memory_limit_eviction() {
        let mut cache = BlockCache::new(100).with_memory_limit(1024);

        // Insert 1KB data
        cache.insert(0, Arc::from(vec![0u8; 1024]));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.memory_usage(), 1024);

        // Insert another 1KB, should evict first entry
        cache.insert(1, Arc::from(vec![0u8; 1024]));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(0).is_none());
        assert!(cache.get(1).is_some());
        assert_eq!(cache.memory_usage(), 1024);
    }

    #[test]
    fn test_memory_tracking() {
        let mut cache = BlockCache::new(10).with_memory_limit(10240);

        cache.insert(0, Arc::from(vec![0u8; 100]));
        cache.insert(1, Arc::from(vec![0u8; 200]));
        assert_eq!(cache.memory_usage(), 300);

        cache.insert(2, Arc::from(vec![0u8; 300]));
        assert_eq!(cache.memory_usage(), 600);

        // Update existing entry
        cache.insert(0, Arc::from(vec![0u8; 50]));
        assert_eq!(cache.memory_usage(), 550);
    }
}
