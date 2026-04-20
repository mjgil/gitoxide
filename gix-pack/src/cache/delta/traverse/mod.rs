use std::sync::atomic::{AtomicBool, Ordering};

use gix_features::{
    budget::MemoryBudget,
    parallel::in_parallel_with_slice,
    progress::{self, DynNestedProgress, Progress},
    threading,
    threading::{Mutable, OwnShared},
};

use crate::{
    cache::delta::Tree,
    data::EntryRange,
};

mod resolve;
pub(crate) mod spool;

/// Returned by [`Tree::traverse()`]
#[derive(thiserror::Error, Debug)]
#[allow(missing_docs)]
pub enum Error {
    #[error("{message}")]
    ZlibInflate {
        source: gix_features::zlib::inflate::Error,
        message: &'static str,
    },
    #[error("The resolver failed to obtain the pack entry bytes for the entry at {pack_offset}")]
    ResolveFailed { pack_offset: u64 },
    #[error(transparent)]
    EntryType(#[from] crate::data::entry::decode::Error),
    #[error("One of the object inspectors failed")]
    Inspect(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("Interrupted")]
    Interrupted,
    #[error(
    "The base at {base_pack_offset} was referred to by a ref-delta, but it was never added to the tree as if the pack was still thin."
    )]
    OutOfPackRefDelta {
        /// The base's offset which was from a resolved ref-delta that didn't actually get added to the tree
        base_pack_offset: crate::data::Offset,
    },
    #[error("Failed to spawn thread when switching to work-stealing mode")]
    SpawnThread(#[from] std::io::Error),
    #[error(transparent)]
    Delta(#[from] crate::data::delta::apply::Error),
    /// The delta-chain cache inside the traversal tried to reserve more
    /// bytes from the shared [`MemoryBudget`] than were available.
    ///
    /// Step 5.2 of the bounded-memory plan. Callers who see this should
    /// retry with a wider budget (or with
    /// [`MemoryBudget::unlimited`][gix_features::budget::MemoryBudget::unlimited]
    /// to disable budgeting entirely); the state visible to them on
    /// failure is clean — every cached intermediate delta has been
    /// dropped, releasing its accounted bytes.
    #[error(transparent)]
    OutOfBudget(#[from] gix_features::budget::OutOfBudget),
    /// The delta-chain cache tried to spill bytes to an on-disk spool
    /// (because the shared [`MemoryBudget`] was exhausted) and the
    /// underlying I/O failed — typically "no space left on device" or
    /// "permission denied on `$TMPDIR`".
    ///
    /// Step 5.3 of the bounded-memory plan. Unlike
    /// [`Error::OutOfBudget`] this is a hard failure: there is no
    /// in-RAM or on-disk fallback beyond the spool, so the traversal
    /// cannot continue. State on failure is clean: scoped threads in
    /// [`resolve::deltas_mt`] unwind, the shared cache map drops,
    /// every `Reservation` drops, and the (partially-written) spool
    /// file is closed and unlinked by the kernel.
    #[error("delta-chain cache spill-to-disk failed")]
    SpoolIo(#[source] std::io::Error),
}

/// Additional context passed to the `sink` callback of the [`Tree::traverse()`] method.
pub struct Context<'a> {
    /// The pack entry describing the object
    pub entry: &'a crate::data::Entry,
    /// The offset at which `entry` ends in the pack, useful to learn about the exact range of `entry` within the pack.
    pub entry_end: u64,
    /// The decompressed object itself, ready to be decoded.
    pub decompressed: &'a [u8],
    /// The depth at which this object resides in the delta-tree. It represents the number of base objects, with 0 indicating
    /// an 'undeltified' object, and higher values indicating delta objects with the given number of bases.
    pub level: u16,
}

/// Options for [`Tree::traverse()`].
pub struct Options<'a, 's> {
    /// is a progress instance to track progress for each object in the traversal.
    pub object_progress: Box<dyn DynNestedProgress>,
    /// is a progress instance to track the overall progress.
    pub size_progress: &'s mut dyn Progress,
    /// If `Some`, only use the given number of threads. Otherwise, the number of threads to use will be selected based on
    /// the number of available logical cores.
    pub thread_limit: Option<usize>,
    /// Abort the operation if the value is `true`.
    pub should_interrupt: &'a AtomicBool,
    /// specifies what kind of hashes we expect to be stored in oid-delta entries, which is viable to decoding them
    /// with the correct size.
    pub object_hash: gix_hash::Kind,
    /// Shared memory budget consulted by budget-aware allocation sites
    /// inside the traversal.
    ///
    /// As of step 5.2 of the bounded-memory plan, this is load-bearing:
    /// the `decompressed_bytes_by_pack_offset` delta-chain cache in
    /// `resolve::deltas` and `resolve::deltas_mt` reserves bytes
    /// against this budget on every cached intermediate delta. If the
    /// budget is exhausted mid-traversal the whole operation returns
    /// [`Error::OutOfBudget`] cleanly; callers retry with a wider
    /// budget or with [`MemoryBudget::unlimited`].
    ///
    /// Use [`MemoryBudget::unlimited`] when you don't care — this
    /// preserves pre-budget behaviour byte-for-byte.
    pub memory_budget: MemoryBudget,
}

impl Tree {
    /// Traverse this tree of delta objects, calling `sink` for each resolved object.
    ///
    /// * `resolve(EntrySlice, &R) -> Option<&[u8]>` resolves the bytes in the pack for the given
    ///   `EntrySlice`. It returns `Some(bytes)` if the object existed in the pack, or `None` to
    ///   indicate a resolution error, which aborts the operation.
    /// * `pack_entries_end` marks one-past-the-last byte of the last entry in the pack, as the
    ///   last entry's size would otherwise be unknown (it's not part of the index file).
    /// * `sink(offset, progress, context, accumulator)` is called exactly once per resolved
    ///   object. The `accumulator` is per-worker state created by `new_accumulator` — one
    ///   instance per worker thread. The sink receives `&A` (shared ref) because work-stealing
    ///   sub-threads within a single root tree share the same accumulator; use interior
    ///   mutability (e.g. `Mutex`) if the accumulator needs mutation.
    /// * `new_accumulator` is called once per worker thread to create the per-worker
    ///   accumulator. After traversal, all accumulators are returned as `Vec<A>`.
    ///
    /// _Note_ that this method consumes the Tree to assure safe parallel traversal.
    pub fn traverse<F, SINK, E, R, A>(
        mut self,
        resolve: F,
        resolve_data: &R,
        pack_entries_end: u64,
        sink: SINK,
        new_accumulator: impl FnOnce() -> A + Send + Clone,
        Options {
            thread_limit,
            mut object_progress,
            size_progress,
            should_interrupt,
            object_hash,
            memory_budget,
        }: Options<'_, '_>,
    ) -> Result<Vec<A>, Error>
    where
        F: for<'r> Fn(EntryRange, &'r R) -> Option<&'r [u8]> + Send + Clone,
        R: Send + Sync,
        SINK: FnMut(crate::data::Offset, &dyn Progress, Context<'_>, &A) -> Result<(), E> + Send + Clone,
        E: std::error::Error + Send + Sync + 'static,
        A: Send + Sync,
    {
        self.set_pack_entries_end_and_resolve_ref_offsets(pack_entries_end)?;

        // `memory_budget` is consumed below by the resolve::State
        // factory closure; each worker clones the same Arc-backed
        // counter into its own State so the cap applies across
        // cooperating threads. Step 5.2 of the bounded-memory plan.
        //
        // Step 5.3: a shared `SpoolHandle` rides alongside the
        // budget. It is created eagerly here as an `Arc<SpoolHandle>`
        // so every worker sees the same lazy slot — the first worker
        // to exhaust its budget opens the underlying tempfile; all
        // other workers reuse it. Under [`MemoryBudget::unlimited`]
        // no worker ever needs to spill and the handle's
        // `Option<Arc<SpoolFile>>` stays `None`, so the only cost is
        // one Mutex-wrapped `Option` per worker.
        let spool = std::sync::Arc::new(spool::SpoolHandle::new());

        let num_objects = self.num_items();
        let object_counter = {
            let progress = &mut object_progress;
            progress.init(Some(num_objects), progress::count("objects"));
            progress.counter()
        };
        size_progress.init(None, progress::bytes());
        let size_counter = size_progress.counter();
        let object_progress = OwnShared::new(Mutable::new(object_progress));

        let start = std::time::Instant::now();
        let (store, is_root) = self.take_store_and_is_root();
        let num_items = store.num_items() as usize;
        debug_assert_eq!(is_root.len(), num_items);
        let metadata_owned = store.metadata();
        let metadata = &metadata_owned;
        // Root indices are the traversal unit: each worker pulls
        // a `&mut u32` root idx from the slice, builds a fresh
        // `Node` via the shared `metadata` + `data`, and runs
        // `resolve::deltas`. Derived from `is_root` since roots
        // and children interleave in the store's pack-offset
        // layout — they are NOT at indices `0..num_roots`.
        let mut root_indices: Vec<u32> = is_root
            .iter()
            .enumerate()
            .filter_map(|(i, &r)| r.then_some(i as u32))
            .collect();
        let accumulators = in_parallel_with_slice(
            &mut root_indices,
            thread_limit,
            {
                {
                    let object_progress = object_progress.clone();
                    let memory_budget = memory_budget.clone();
                    let spool = std::sync::Arc::clone(&spool);
                    let sink = sink.clone();
                    let new_accumulator = new_accumulator.clone();
                    move |thread_index| resolve::State {
                        delta_bytes: Vec::<u8>::with_capacity(4096),
                        fully_resolved_delta_bytes: Vec::<u8>::with_capacity(4096),
                        progress: Box::new(
                            threading::lock(&object_progress).add_child(format!("thread {thread_index}")),
                        ),
                        resolve: resolve.clone(),
                        metadata,
                        memory_budget: memory_budget.clone(),
                        spool: std::sync::Arc::clone(&spool),
                        sink: sink.clone(),
                        accumulator: new_accumulator(),
                    }
                }
            },
            {
                move |root_idx, state, threads_left, should_interrupt| {
                    // SAFETY: `root_idx` comes from the Vec<u32> built above;
                    // `metadata` and `data` carry the same lifetime as the
                    // store the roots belong to. The delta-tree's one-parent-
                    // per-child property guarantees `get_mut(idx)` uniqueness
                    // across workers.
                    #[allow(unsafe_code)]
                    unsafe {
                        resolve::deltas(
                            object_counter.clone(),
                            size_counter.clone(),
                            root_idx,
                            state,
                            resolve_data,
                            object_hash.len_in_bytes(),
                            threads_left,
                            should_interrupt,
                        )
                    }
                }
            },
            || (!should_interrupt.load(Ordering::Relaxed)).then(|| std::time::Duration::from_millis(50)),
            |s| s.accumulator,
        )?;

        threading::lock(&object_progress).show_throughput(start);
        size_progress.show_throughput(start);

        let _ = metadata_owned;
        drop(store);
        let _ = is_root;
        let _ = root_indices;
        let _ = num_items;
        Ok(accumulators)
    }
}
