use super::DecodeEntry;

#[cfg(feature = "pack-cache-lru-dynamic")]
mod memory {
    use std::num::NonZeroUsize;

    use clru::WeightScale;
    use gix_features::budget::{MemoryBudget, Reservation};

    use super::DecodeEntry;
    use crate::cache::set_vec_to_slice;

    struct Entry {
        data: Vec<u8>,
        kind: gix_object::Kind,
        compressed_size: usize,
        /// Keeps `data.len()` bytes accounted against the shared
        /// [`MemoryBudget`]. Dropped when this entry is evicted (or when the
        /// whole cache is dropped), releasing those bytes back. Never read.
        _reservation: Reservation,
    }

    type Key = (u32, u64);
    struct CustomScale;

    impl WeightScale<Key, Entry> for CustomScale {
        fn weight(&self, _key: &Key, value: &Entry) -> usize {
            value.data.len()
        }
    }

    /// An LRU cache with hash map backing and an eviction rule based on the memory usage for object data in bytes.
    pub struct MemoryCappedHashmap {
        inner: clru::CLruCache<Key, Entry, std::collections::hash_map::RandomState, CustomScale>,
        free_list: Vec<Vec<u8>>,
        debug: gix_features::cache::Debug,
        /// Shared byte budget consulted on every [`put`](DecodeEntry::put).
        /// Typically cloned from the [`Repository`](../../../gix/struct.Repository.html)
        /// that owns this cache. When the cache is constructed via the
        /// legacy [`new`](Self::new) entrypoint this is
        /// [unlimited](MemoryBudget::unlimited), preserving the pre-budget
        /// behaviour byte-for-byte.
        budget: MemoryBudget,
    }

    impl MemoryCappedHashmap {
        /// Return a new instance which evicts least recently used items if it uses more than `memory_cap_in_bytes`
        /// object data.
        ///
        /// Equivalent to
        /// [`with_memory_budget`](Self::with_memory_budget)`(memory_cap_in_bytes,
        /// `[`MemoryBudget::unlimited()`]`)`: no shared accounting, every put
        /// succeeds as before. Retained so callers that don't thread a
        /// repository-scoped budget continue to compile unchanged.
        pub fn new(memory_cap_in_bytes: usize) -> MemoryCappedHashmap {
            Self::with_memory_budget(memory_cap_in_bytes, MemoryBudget::unlimited())
        }

        /// Like [`new`](Self::new) but additionally participates in a shared
        /// [`MemoryBudget`].
        ///
        /// Every [`put`](DecodeEntry::put) tries to reserve `data.len()` bytes
        /// from `budget` before handing the entry to the underlying LRU. If
        /// the reservation would exceed the budget cap the entry is dropped
        /// silently — treated as a cache miss rather than an error, since
        /// this is an opportunistic cache and the caller will simply
        /// re-decode next time.
        ///
        /// `memory_cap_in_bytes` remains the *local* hard cap (the `clru`
        /// cache still evicts on its own terms). The shared budget is an
        /// additional constraint on top, letting multiple caches (pack
        /// cache, object cache, ...) cooperate under a single process- or
        /// repository-wide ceiling.
        pub fn with_memory_budget(memory_cap_in_bytes: usize, budget: MemoryBudget) -> MemoryCappedHashmap {
            MemoryCappedHashmap {
                inner: clru::CLruCache::with_config(
                    clru::CLruCacheConfig::new(NonZeroUsize::new(memory_cap_in_bytes).expect("non zero"))
                        .with_scale(CustomScale),
                ),
                free_list: Vec::new(),
                debug: gix_features::cache::Debug::new(format!("MemoryCappedHashmap({memory_cap_in_bytes}B)")),
                budget,
            }
        }
    }

    impl DecodeEntry for MemoryCappedHashmap {
        fn put(&mut self, pack_id: u32, offset: u64, data: &[u8], kind: gix_object::Kind, compressed_size: usize) {
            self.debug.put();
            let Some(data) = set_vec_to_slice(self.free_list.pop().unwrap_or_default(), data) else {
                return;
            };
            // Consult the shared budget before committing the entry to
            // `clru`. Failure to reserve is semantically identical to the
            // existing "too big for local cap" branch: we recycle the
            // allocation via `free_list` and return without inserting.
            let reservation = match self.budget.reserve(data.len()) {
                Ok(r) => r,
                Err(_) => {
                    self.free_list.push(data);
                    return;
                }
            };
            let res = self.inner.put_with_weight(
                (pack_id, offset),
                Entry {
                    data,
                    kind,
                    compressed_size,
                    _reservation: reservation,
                },
            );
            match res {
                Ok(Some(previous_entry)) => self.free_list.push(previous_entry.data),
                Ok(None) => {}
                Err((_key, value)) => self.free_list.push(value.data),
            }
            // In the Err / evicted-previous paths, the Entry's `_reservation`
            // field drops here with the rest of the struct, releasing its
            // bytes back to the shared budget. The raw `Vec` capacity held
            // in `free_list` is unaccounted but bounded (~1 entry of churn).
        }

        fn get(&mut self, pack_id: u32, offset: u64, out: &mut Vec<u8>) -> Option<(gix_object::Kind, usize)> {
            let res = self.inner.get(&(pack_id, offset)).and_then(|e| {
                set_vec_to_slice(out, &e.data)?;
                Some((e.kind, e.compressed_size))
            });
            if res.is_some() {
                self.debug.hit();
            } else {
                self.debug.miss();
            }
            res
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn put_over_budget_is_dropped_silently() {
            // Shared budget too small for even one entry; puts must be
            // silently rejected and leave no trace on the shared counter.
            let budget = MemoryBudget::bytes(10);
            let mut c = MemoryCappedHashmap::with_memory_budget(1024, budget.clone());

            c.put(0, 0, &vec![0u8; 100], gix_object::Kind::Blob, 100);

            let mut out = Vec::new();
            assert!(
                c.get(0, 0, &mut out).is_none(),
                "a put that cannot be accounted for must not land in the cache",
            );
            assert_eq!(
                budget.used(),
                0,
                "a rejected put must not leave bytes reserved against the budget",
            );
        }

        #[test]
        fn put_within_budget_reserves_exactly_data_len_bytes() {
            let budget = MemoryBudget::bytes(1024);
            let mut c = MemoryCappedHashmap::with_memory_budget(1024, budget.clone());

            let data = vec![7u8; 100];
            c.put(0, 0, &data, gix_object::Kind::Blob, data.len());

            let mut out = Vec::new();
            assert!(c.get(0, 0, &mut out).is_some(), "stored entry should be retrievable");
            assert_eq!(out, data);
            assert_eq!(
                budget.used(),
                100,
                "per-entry accounting must match the clru weight function (data.len())",
            );
        }

        #[test]
        fn dropping_cache_releases_all_reservations() {
            let budget = MemoryBudget::bytes(1024);
            {
                let mut c = MemoryCappedHashmap::with_memory_budget(1024, budget.clone());
                c.put(0, 0, &vec![3u8; 100], gix_object::Kind::Blob, 100);
                assert_eq!(budget.used(), 100);
            }
            assert_eq!(
                budget.used(),
                0,
                "dropping the cache must drop every Entry and release its Reservation",
            );
        }

        #[test]
        fn legacy_new_participates_in_no_external_budget() {
            // `new` uses MemoryBudget::unlimited() internally; entries
            // therefore never interact with any caller's budget.
            let external_budget = MemoryBudget::bytes(10);
            let mut c = MemoryCappedHashmap::new(1024);
            c.put(0, 0, &vec![0u8; 200], gix_object::Kind::Blob, 200);
            let mut out = Vec::new();
            assert!(
                c.get(0, 0, &mut out).is_some(),
                "legacy constructor must store entries regardless of any external budget",
            );
            assert_eq!(
                external_budget.used(),
                0,
                "legacy constructor must not touch unrelated budgets",
            );
        }
    }
}

#[cfg(feature = "pack-cache-lru-dynamic")]
pub use memory::MemoryCappedHashmap;

#[cfg(feature = "pack-cache-lru-static")]
mod _static {
    use gix_features::budget::{MemoryBudget, Reservation};

    use super::DecodeEntry;
    use crate::cache::set_vec_to_slice;
    struct Entry {
        pack_id: u32,
        offset: u64,
        data: Vec<u8>,
        kind: gix_object::Kind,
        compressed_size: usize,
        /// Keeps `data.len()` bytes accounted against the shared
        /// [`MemoryBudget`]. Dropped when this entry is evicted (either
        /// into `last_evicted` via `.data`-move — which leaves the rest of
        /// the struct to drop individually — or when the whole cache is
        /// dropped). Never read.
        _reservation: Reservation,
    }

    /// A cache using a least-recently-used implementation capable of storing the `SIZE` most recent objects.
    /// The cache must be small as the search is 'naive' and the underlying data structure is a linked list.
    /// Values of 64 seem to improve performance.
    pub struct StaticLinkedList<const SIZE: usize> {
        inner: uluru::LRUCache<Entry, SIZE>,
        last_evicted: Vec<u8>,
        debug: gix_features::cache::Debug,
        /// the amount of bytes we are currently holding, taking into account the capacities of all Vecs we keep.
        mem_used: usize,
        /// The total amount of memory we should be able to hold with all entries combined.
        mem_limit: usize,
        /// Shared byte budget consulted on every [`put`](DecodeEntry::put).
        /// Cloned from the owning `Repository` in the budget-aware
        /// construction path. When the cache is built via the legacy
        /// [`new`](Self::new) entrypoint (or via [`Default`]) this is
        /// [unlimited](MemoryBudget::unlimited), preserving the
        /// pre-budget behaviour byte-for-byte.
        ///
        /// Note: unlike `mem_used`/`mem_limit`, which track `Vec::
        /// capacity()` including the transient `last_evicted` recycle
        /// buffer, budget accounting covers only `data.len()` for
        /// entries currently held in `inner`. The recycle buffer is
        /// bounded by design (at most one entry's worth of bytes) and
        /// is treated as transient, matching the `free_list` policy in
        /// `MemoryCappedHashmap`.
        budget: MemoryBudget,
    }

    impl<const SIZE: usize> StaticLinkedList<SIZE> {
        /// Create a new list with a memory limit of `mem_limit` in bytes. If 0, there is no memory limit.
        ///
        /// Equivalent to [`with_memory_budget`](Self::with_memory_budget)`(mem_limit,
        /// `[`MemoryBudget::unlimited()`]`)`: no shared accounting, every put succeeds
        /// as before. Retained so pre-budget callers continue to compile unchanged.
        pub fn new(mem_limit: usize) -> Self {
            Self::with_memory_budget(mem_limit, MemoryBudget::unlimited())
        }

        /// Like [`new`](Self::new) but additionally participates in a shared
        /// [`MemoryBudget`].
        ///
        /// Every [`put`](DecodeEntry::put) tries to reserve `data.len()` bytes
        /// from `budget` after confirming the entry fits this cache's local
        /// `mem_limit`. If the reservation would exceed the budget cap the
        /// entry is dropped silently — treated as a cache miss rather than an
        /// error, since this is an opportunistic cache and the caller will
        /// simply re-decode next time.
        ///
        /// `mem_limit` remains the *local* cap. The shared budget is an
        /// additional constraint on top, letting multiple caches cooperate
        /// under a single process- or repository-wide ceiling.
        pub fn with_memory_budget(mem_limit: usize, budget: MemoryBudget) -> Self {
            StaticLinkedList {
                inner: Default::default(),
                last_evicted: Vec::new(),
                debug: gix_features::cache::Debug::new(format!("StaticLinkedList<{SIZE}>")),
                mem_used: 0,
                mem_limit: if mem_limit == 0 { usize::MAX } else { mem_limit },
                budget,
            }
        }
    }

    impl<const SIZE: usize> Default for StaticLinkedList<SIZE> {
        fn default() -> Self {
            Self::new(96 * 1024 * 1024)
        }
    }

    impl<const SIZE: usize> DecodeEntry for StaticLinkedList<SIZE> {
        fn put(&mut self, pack_id: u32, offset: u64, data: &[u8], kind: gix_object::Kind, compressed_size: usize) {
            // We cannot possibly hold this much.
            if data.len() > self.mem_limit {
                return;
            }
            // Consult the shared budget before mutating any cache state.
            // Failure to reserve is treated as a cache miss: the existing
            // code path is tolerant of puts that silently don't land
            // (the `data.len() > mem_limit` branch above is the same
            // shape), so callers will just re-decode next time. Taking
            // the reservation before eviction means we don't churn the
            // local cache for a put we can't accept; worst case is a
            // rare miss when eviction *could* have freed enough budget
            // from *this* cache's entries — acceptable, consistent with
            // MemoryCappedHashmap's policy.
            let reservation = match self.budget.reserve(data.len()) {
                Ok(r) => r,
                Err(_) => return,
            };
            // If we could hold it but are at limit, all we can do is make space.
            let mem_free = self.mem_limit - self.mem_used;
            if data.len() > mem_free {
                // prefer freeing free-lists instead of clearing our cache
                let free_list_cap = self.last_evicted.len();
                self.last_evicted = Vec::new();
                // still not enough? clear everything
                if data.len() > mem_free + free_list_cap {
                    // Clearing drops every Entry — and thus every
                    // `_reservation`, restoring their bytes to the
                    // shared budget automatically.
                    self.inner.clear();
                    self.mem_used = 0;
                } else {
                    self.mem_used -= free_list_cap;
                }
            }
            self.debug.put();
            let mut v = std::mem::take(&mut self.last_evicted);
            self.mem_used -= v.capacity();
            if set_vec_to_slice(&mut v, data).is_none() {
                // `reservation` drops here, releasing its bytes back.
                return;
            }
            self.mem_used += v.capacity();
            if let Some(previous) = self.inner.insert(Entry {
                offset,
                pack_id,
                data: v,
                kind,
                compressed_size,
                _reservation: reservation,
            }) {
                // `previous.data` moves into `last_evicted`; the other
                // fields of `previous` — including `_reservation` —
                // drop at the end of this block, releasing the evicted
                // entry's budget bytes back to the shared pool.
                // No need to adjust capacity as we already counted it.
                self.last_evicted = previous.data;
            }
        }

        fn get(&mut self, pack_id: u32, offset: u64, out: &mut Vec<u8>) -> Option<(gix_object::Kind, usize)> {
            let res = self.inner.lookup(|e: &mut Entry| {
                if e.pack_id == pack_id && e.offset == offset {
                    set_vec_to_slice(&mut *out, &e.data)?;
                    Some((e.kind, e.compressed_size))
                } else {
                    None
                }
            });
            if res.is_some() {
                self.debug.hit();
            } else {
                self.debug.miss();
            }
            res
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn no_limit() {
            let c = StaticLinkedList::<10>::new(0);
            assert_eq!(
                c.mem_limit,
                usize::MAX,
                "zero is automatically turned into a large limit that is equivalent to unlimited"
            );
        }

        #[test]
        fn journey() {
            let mut c = StaticLinkedList::<10>::new(100);
            assert_eq!(c.mem_limit, 100);
            assert_eq!(c.mem_used, 0);

            // enough memory for normal operation
            let mut last_mem_used = 0;
            for _ in 0..10 {
                c.put(0, 0, &[0], gix_object::Kind::Blob, 1);
                assert!(c.mem_used > last_mem_used);
                last_mem_used = c.mem_used;
            }
            assert_eq!(c.mem_used, 80, "there is a minimal vec size");
            assert_eq!(c.inner.len(), 10);
            assert_eq!(c.last_evicted.len(), 0);

            c.put(0, 0, &(0..20).collect::<Vec<_>>(), gix_object::Kind::Blob, 1);
            assert_eq!(c.inner.len(), 10);
            assert_eq!(c.mem_used, 80 + 20);
            assert_eq!(c.last_evicted.len(), 1);

            c.put(0, 0, &(0..50).collect::<Vec<_>>(), gix_object::Kind::Blob, 1);
            assert_eq!(c.inner.len(), 1, "cache clearance wasn't necessary");
            assert_eq!(c.last_evicted.len(), 0, "the free list was cleared");
            assert_eq!(c.mem_used, 50);

            c.put(0, 0, &(0..101).collect::<Vec<_>>(), gix_object::Kind::Blob, 1);
            assert_eq!(
                c.inner.len(),
                1,
                "objects that won't ever fit within the memory limit are ignored"
            );
        }

        #[test]
        fn put_over_budget_is_dropped_silently() {
            // Local mem_limit is generous; the shared budget is what
            // blocks the put.
            let budget = MemoryBudget::bytes(10);
            let mut c = StaticLinkedList::<10>::with_memory_budget(1_000_000, budget.clone());

            c.put(0, 0, &vec![0u8; 100], gix_object::Kind::Blob, 100);

            let mut out = Vec::new();
            assert!(
                c.get(0, 0, &mut out).is_none(),
                "a put that cannot be accounted for must not land in the cache",
            );
            assert_eq!(
                budget.used(),
                0,
                "a rejected put must not leave bytes reserved against the budget",
            );
            assert_eq!(
                c.inner.len(),
                0,
                "a budget rejection must not mutate the cache state",
            );
        }

        #[test]
        fn put_within_budget_reserves_data_len_bytes() {
            let budget = MemoryBudget::bytes(1024);
            let mut c = StaticLinkedList::<10>::with_memory_budget(1_000_000, budget.clone());

            c.put(0, 0, &vec![7u8; 100], gix_object::Kind::Blob, 100);
            let mut out = Vec::new();
            assert!(c.get(0, 0, &mut out).is_some(), "stored entry should be retrievable");
            assert_eq!(
                budget.used(),
                100,
                "per-entry accounting must match data.len() for StaticLinkedList",
            );
        }

        #[test]
        fn evicted_entry_releases_budget() {
            // Force an eviction: the uluru LRU has capacity SIZE=2, so
            // inserting three distinct entries evicts the oldest. The
            // evicted entry's `_reservation` must drop, returning its
            // bytes to the shared pool.
            let budget = MemoryBudget::bytes(1024);
            let mut c = StaticLinkedList::<2>::with_memory_budget(1_000_000, budget.clone());

            c.put(0, 0, &vec![1u8; 10], gix_object::Kind::Blob, 10);
            c.put(0, 1, &vec![2u8; 20], gix_object::Kind::Blob, 20);
            assert_eq!(budget.used(), 30, "two entries: 10 + 20 bytes");

            c.put(0, 2, &vec![3u8; 30], gix_object::Kind::Blob, 30);
            // The (0,0) entry was evicted; its 10-byte reservation
            // dropped. The (0,1) and (0,2) entries remain — 20 + 30.
            assert_eq!(
                budget.used(),
                50,
                "evicted entry must release its reserved bytes back to the shared budget",
            );
        }

        #[test]
        fn cache_clear_via_local_limit_releases_all_budget() {
            // Tight local mem_limit forces the `self.inner.clear()`
            // branch in `put` when a big-enough put arrives. That
            // branch must release every entry's budget bytes too.
            let budget = MemoryBudget::bytes(1024);
            let mut c = StaticLinkedList::<10>::with_memory_budget(200, budget.clone());

            c.put(0, 0, &vec![1u8; 20], gix_object::Kind::Blob, 20);
            c.put(0, 1, &vec![2u8; 30], gix_object::Kind::Blob, 30);
            assert!(budget.used() > 0);

            // ~190 bytes of data forces `data.len() > mem_free +
            // free_list_cap`, triggering `inner.clear()`.
            c.put(0, 2, &vec![3u8; 190], gix_object::Kind::Blob, 190);

            // Only the newly inserted 190-byte entry remains; the
            // previous two entries' reservations must have released.
            assert_eq!(
                budget.used(),
                190,
                "inner.clear() must drop every held Entry and release its reservation",
            );
        }

        #[test]
        fn dropping_cache_releases_all_reservations() {
            let budget = MemoryBudget::bytes(1024);
            {
                let mut c = StaticLinkedList::<10>::with_memory_budget(1_000_000, budget.clone());
                c.put(0, 0, &vec![1u8; 100], gix_object::Kind::Blob, 100);
                c.put(0, 1, &vec![2u8; 100], gix_object::Kind::Blob, 100);
                assert_eq!(budget.used(), 200);
            }
            assert_eq!(
                budget.used(),
                0,
                "dropping the cache must drop every Entry and release its Reservation",
            );
        }

        #[test]
        fn legacy_new_participates_in_no_external_budget() {
            // `new` and `Default` both use MemoryBudget::unlimited()
            // internally; entries therefore never interact with any
            // caller's budget.
            let external_budget = MemoryBudget::bytes(10);
            let mut c = StaticLinkedList::<10>::new(1_000_000);
            c.put(0, 0, &vec![0u8; 200], gix_object::Kind::Blob, 200);
            let mut out = Vec::new();
            assert!(
                c.get(0, 0, &mut out).is_some(),
                "legacy constructor must store entries regardless of any external budget",
            );
            assert_eq!(
                external_budget.used(),
                0,
                "legacy constructor must not touch unrelated budgets",
            );
        }
    }
}

#[cfg(feature = "pack-cache-lru-static")]
pub use _static::StaticLinkedList;
