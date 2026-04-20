//! This module is a bit 'misplaced' if spelled out like '`gix_pack::cache::object::`*' but is best placed here for code reuse and
//! general usefulness.
use crate::cache;

#[cfg(feature = "object-cache-dynamic")]
mod memory {
    use std::num::NonZeroUsize;

    use clru::WeightScale;
    use gix_features::budget::{MemoryBudget, Reservation};

    use crate::{cache, cache::set_vec_to_slice};

    struct Entry {
        data: Vec<u8>,
        kind: gix_object::Kind,
        /// Accounts `data.len() + size_of::<Entry>() + key_len` bytes against
        /// the shared [`MemoryBudget`]. Dropped on eviction or cache-drop to
        /// return those bytes to the pool. Never read.
        _reservation: Reservation,
    }

    type Key = gix_hash::ObjectId;

    struct CustomScale;

    impl WeightScale<Key, Entry> for CustomScale {
        fn weight(&self, key: &Key, value: &Entry) -> usize {
            value.data.len() + std::mem::size_of::<Entry>() + key.as_bytes().len()
        }
    }

    /// An LRU cache with hash map backing and an eviction rule based on the memory usage for object data in bytes.
    pub struct MemoryCappedHashmap {
        inner: clru::CLruCache<Key, Entry, gix_hashtable::hash::Builder, CustomScale>,
        free_list: Vec<Vec<u8>>,
        debug: gix_features::cache::Debug,
        /// Shared byte budget consulted on every [`put`](cache::Object::put).
        /// Typically cloned from the owning `Repository`. When the cache is
        /// constructed via the legacy [`new`](Self::new) entrypoint this is
        /// [unlimited](MemoryBudget::unlimited), preserving the pre-budget
        /// behaviour byte-for-byte.
        budget: MemoryBudget,
    }

    impl MemoryCappedHashmap {
        /// The amount of bytes we can hold in total, or the value we saw in `new(…)`.
        pub fn capacity(&self) -> usize {
            self.inner.capacity()
        }
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
        /// Every [`put`](cache::Object::put) tries to reserve
        /// `data.len() + size_of::<Entry>() + id.as_bytes().len()` bytes
        /// from `budget` — mirroring the weight function the underlying
        /// `clru` LRU uses for eviction, so the two capacities track the
        /// same concept. If the reservation would exceed the budget cap the
        /// entry is dropped silently (cache miss, not an error).
        ///
        /// `memory_cap_in_bytes` remains the *local* hard cap. The shared
        /// budget is an additional constraint that lets multiple caches
        /// cooperate under a single process- or repository-wide ceiling.
        pub fn with_memory_budget(memory_cap_in_bytes: usize, budget: MemoryBudget) -> MemoryCappedHashmap {
            MemoryCappedHashmap {
                inner: clru::CLruCache::with_config(
                    clru::CLruCacheConfig::new(NonZeroUsize::new(memory_cap_in_bytes).expect("non zero"))
                        .with_hasher(gix_hashtable::hash::Builder)
                        .with_scale(CustomScale),
                ),
                free_list: Vec::new(),
                debug: gix_features::cache::Debug::new(format!("MemoryCappedObjectHashmap({memory_cap_in_bytes}B)")),
                budget,
            }
        }
    }

    impl cache::Object for MemoryCappedHashmap {
        /// Put the object going by `id` of `kind` with `data` into the cache.
        fn put(&mut self, id: gix_hash::ObjectId, kind: gix_object::Kind, data: &[u8]) {
            self.debug.put();
            let Some(data) = set_vec_to_slice(self.free_list.pop().unwrap_or_default(), data) else {
                return;
            };
            // Mirror `CustomScale::weight` so the budget accounting and the
            // clru eviction accounting track the same quantity. If the
            // reservation fails we recycle the Vec via `free_list` and
            // return without inserting — same shape as the existing "clru
            // rejected the entry" branch below.
            let weight = data.len() + std::mem::size_of::<Entry>() + id.as_bytes().len();
            let reservation = match self.budget.reserve(weight) {
                Ok(r) => r,
                Err(_) => {
                    self.free_list.push(data);
                    return;
                }
            };
            let res = self.inner.put_with_weight(
                id,
                Entry {
                    data,
                    kind,
                    _reservation: reservation,
                },
            );
            match res {
                Ok(Some(previous_entry)) => self.free_list.push(previous_entry.data),
                Ok(None) => {}
                Err((_key, value)) => self.free_list.push(value.data),
            }
            // `_reservation` on any dropped/evicted Entry releases back to
            // the shared budget automatically via Drop.
        }

        /// Try to retrieve the object named `id` and place its data into `out` if available and return `Some(kind)` if found.
        fn get(&mut self, id: &gix_hash::ObjectId, out: &mut Vec<u8>) -> Option<gix_object::Kind> {
            let res = self.inner.get(id).and_then(|e| {
                set_vec_to_slice(out, &e.data)?;
                Some(e.kind)
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
        // `cache::Object` provides the put/get methods; unlike lru.rs's
        // DecodeEntry, it isn't `use`-imported into `mod memory`, so bring
        // it into scope explicitly for the tests.
        use crate::cache::Object;

        fn id_with_first_byte(b: u8) -> gix_hash::ObjectId {
            let mut bytes = [0u8; 20];
            bytes[0] = b;
            gix_hash::ObjectId::from_bytes_or_panic(&bytes)
        }

        #[test]
        fn put_over_budget_is_dropped_silently() {
            let budget = MemoryBudget::bytes(16); // smaller than even one entry's weight
            let mut c = MemoryCappedHashmap::with_memory_budget(1024, budget.clone());

            c.put(id_with_first_byte(1), gix_object::Kind::Blob, &vec![0u8; 100]);

            let mut out = Vec::new();
            assert!(
                c.get(&id_with_first_byte(1), &mut out).is_none(),
                "a put that cannot be accounted for must not land in the cache",
            );
            assert_eq!(
                budget.used(),
                0,
                "a rejected put must not leave bytes reserved against the budget",
            );
        }

        #[test]
        fn put_within_budget_reserves_bytes_matching_clru_weight() {
            let budget = MemoryBudget::bytes(10 * 1024);
            let mut c = MemoryCappedHashmap::with_memory_budget(10 * 1024, budget.clone());

            let data = vec![7u8; 100];
            let id = id_with_first_byte(1);
            c.put(id, gix_object::Kind::Blob, &data);

            let mut out = Vec::new();
            assert!(c.get(&id, &mut out).is_some(), "stored entry should be retrievable");
            assert_eq!(out, data);
            // Budget accounting must mirror `CustomScale::weight`:
            //   data.len() + size_of::<Entry>() + key_len
            // Keep the assertion tolerant to Entry size changes but tight
            // enough to prove it's more than just data.len().
            let min = data.len() + id.as_bytes().len();
            assert!(
                budget.used() > min as u64,
                "expected > {} used (data + key), got {}",
                min,
                budget.used(),
            );
        }

        #[test]
        fn dropping_cache_releases_all_reservations() {
            let budget = MemoryBudget::bytes(10 * 1024);
            {
                let mut c = MemoryCappedHashmap::with_memory_budget(10 * 1024, budget.clone());
                c.put(id_with_first_byte(1), gix_object::Kind::Blob, &[0u8; 100]);
                assert!(budget.used() > 0);
            }
            assert_eq!(
                budget.used(),
                0,
                "dropping the cache must drop every Entry and release its Reservation",
            );
        }

        #[test]
        fn legacy_new_participates_in_no_external_budget() {
            let external_budget = MemoryBudget::bytes(16);
            let mut c = MemoryCappedHashmap::new(10 * 1024);
            c.put(id_with_first_byte(1), gix_object::Kind::Blob, &vec![0u8; 200]);
            let mut out = Vec::new();
            assert!(
                c.get(&id_with_first_byte(1), &mut out).is_some(),
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
#[cfg(feature = "object-cache-dynamic")]
pub use memory::MemoryCappedHashmap;

/// A cache implementation that doesn't do any caching.
pub struct Never;

impl cache::Object for Never {
    /// Noop
    fn put(&mut self, _id: gix_hash::ObjectId, _kind: gix_object::Kind, _data: &[u8]) {}

    /// Noop
    fn get(&mut self, _id: &gix_hash::ObjectId, _out: &mut Vec<u8>) -> Option<gix_object::Kind> {
        None
    }
}

impl<T: cache::Object + ?Sized> cache::Object for Box<T> {
    fn put(&mut self, id: gix_hash::ObjectId, kind: gix_object::Kind, data: &[u8]) {
        use std::ops::DerefMut;
        self.deref_mut().put(id, kind, data);
    }

    fn get(&mut self, id: &gix_hash::ObjectId, out: &mut Vec<u8>) -> Option<gix_object::Kind> {
        use std::ops::DerefMut;
        self.deref_mut().get(id, out)
    }
}
