use std::{collections::HashMap, sync::Arc};

struct CacheEntry {
    data: Arc<[u8]>,
    access_count: usize,
}

/// LRU block cache for decompressed PGZF blocks.
///
/// Caches decompressed block data indexed by block number. When the cache is
/// full, the least-recently-used entry is evicted. Setting capacity to 0
/// disables caching entirely.
pub(crate) struct BlockCache {
    entries: HashMap<usize, CacheEntry>,
    capacity: usize,
    counter: usize,
}

impl BlockCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity.min(1024)),
            capacity,
            counter: 0,
        }
    }

    /// Retrieve cached data for `block_index`, updating access time on hit.
    /// Returns an `Arc` clone (cheap reference count bump, no data copy).
    pub(crate) fn get(&mut self, block_index: usize) -> Option<Arc<[u8]>> {
        if self.capacity == 0 {
            return None;
        }
        if let Some(entry) = self.entries.get_mut(&block_index) {
            self.counter += 1;
            entry.access_count = self.counter;
            Some(Arc::clone(&entry.data))
        } else {
            None
        }
    }

    /// Insert decompressed `data` for `block_index`. Evicts the LRU entry if full.
    pub(crate) fn insert(&mut self, block_index: usize, data: Arc<[u8]>) {
        if self.capacity == 0 {
            return;
        }
        // If updating an existing entry, just replace
        if let Some(entry) = self.entries.get_mut(&block_index) {
            self.counter += 1;
            entry.access_count = self.counter;
            entry.data = data;
            return;
        }
        // Evict LRU if at capacity
        if self.entries.len() >= self.capacity {
            self.evict_lru();
        }
        self.counter += 1;
        self.entries.insert(
            block_index,
            CacheEntry {
                data,
                access_count: self.counter,
            },
        );
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn evict_lru(&mut self) {
        if let Some((&lru_key, _)) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.access_count)
        {
            self.entries.remove(&lru_key);
        }
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
}
