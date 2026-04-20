use std::{io, sync::atomic::AtomicBool};

pub use error::Error;
use gix_features::progress::{self, prodash::DynNestedProgress, Count, Progress};

use crate::cache::delta::{traverse, Tree};

mod error;
pub(crate) mod external_sort;

const SORT_BATCH_SIZE: usize = 1024;

#[derive(Clone, Copy)]
pub(crate) struct IndexEntry {
    pub id: gix_hash::ObjectId,
    pub crc32: u32,
    pub offset: crate::data::Offset,
}

/// Information gathered while executing [`write_data_iter_to_stream()`][crate::index::File::write_data_iter_to_stream]
#[derive(PartialEq, Eq, Debug, Hash, Ord, PartialOrd, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Outcome {
    /// The version of the verified index
    pub index_version: crate::index::Version,
    /// The verified checksum of the verified index
    pub index_hash: gix_hash::ObjectId,

    /// The hash of the '.pack' file, also found in its trailing bytes
    pub data_hash: gix_hash::ObjectId,
    /// The amount of objects that were verified, always the amount of objects in the pack.
    pub num_objects: u32,
}

/// The progress ids used in [`write_data_iter_from_stream()`][crate::index::File::write_data_iter_to_stream()].
///
/// Use this information to selectively extract the progress of interest in case the parent application has custom visualization.
#[derive(Debug, Copy, Clone)]
pub enum ProgressId {
    /// Counts the amount of objects that were index thus far.
    IndexObjects,
    /// The amount of bytes that were decompressed while decoding pack entries.
    ///
    /// This is done to determine entry boundaries.
    DecompressedBytes,
    /// The amount of objects whose hashes were computed.
    ///
    /// This is done by decoding them, which typically involves decoding delta objects.
    ResolveObjects,
    /// The amount of bytes that were decoded in total, as the sum of all bytes to represent all resolved objects.
    DecodedBytes,
    /// The amount of bytes written to the index file.
    IndexBytesWritten,
}

impl From<ProgressId> for gix_features::progress::Id {
    fn from(v: ProgressId) -> Self {
        match v {
            ProgressId::IndexObjects => *b"IWIO",
            ProgressId::DecompressedBytes => *b"IWDB",
            ProgressId::ResolveObjects => *b"IWRO",
            ProgressId::DecodedBytes => *b"IWDB",
            ProgressId::IndexBytesWritten => *b"IWBW",
        }
    }
}

/// Various ways of writing an index file from pack entries
impl crate::index::File {
    /// Write information about `entries` as obtained from a pack data file into a pack index file via the `out` stream.
    /// The resolver produced by `make_resolver` must resolve pack entries from the same pack data file that produced the
    /// `entries` iterator.
    ///
    /// * `kind` is the version of pack index to produce, use [`crate::index::Version::default()`] if in doubt.
    /// * `tread_limit` is used for a parallel tree traversal for obtaining object hashes with optimal performance.
    /// * `root_progress` is the top-level progress to stay informed about the progress of this potentially long-running
    ///   computation.
    /// * `object_hash` defines what kind of object hash we write into the index file.
    /// * `pack_version` is the version of the underlying pack for which `entries` are read. It's used in case none of these objects are provided
    ///   to compute a pack-hash.
    /// * `memory_budget` is the shared byte budget consulted by
    ///   budget-aware allocation sites inside the delta-tree traversal.
    ///   As of step 5.2 of the bounded-memory plan, the delta-chain
    ///   cache reserves against it and exhaustion surfaces as
    ///   [`cache::delta::traverse::Error::OutOfBudget`][crate::cache::delta::traverse::Error::OutOfBudget].
    ///   Use [`gix_features::budget::MemoryBudget::unlimited`] when you
    ///   don't care — this preserves pre-budget behaviour byte-for-byte.
    ///
    /// # Remarks
    ///
    /// * neither in-pack nor out-of-pack Ref Deltas are supported here, these must have been resolved beforehand.
    /// * `make_resolver()` will only be called after the iterator stopped returning elements and produces a function that
    ///   provides all bytes belonging to a pack entry writing them to the given mutable output `Vec`.
    ///   It should return `None` if the entry cannot be resolved from the pack that produced the `entries` iterator, causing
    ///   the write operation to fail.
    #[allow(clippy::too_many_arguments)]
    pub fn write_data_iter_to_stream<F, F2, R>(
        version: crate::index::Version,
        make_resolver: F,
        entries: &mut dyn Iterator<Item = Result<crate::data::input::Entry, crate::data::input::Error>>,
        thread_limit: Option<usize>,
        root_progress: &mut dyn DynNestedProgress,
        out: &mut dyn io::Write,
        should_interrupt: &AtomicBool,
        object_hash: gix_hash::Kind,
        pack_version: crate::data::Version,
        memory_budget: gix_features::budget::MemoryBudget,
    ) -> Result<Outcome, Error>
    where
        F: FnOnce() -> io::Result<(F2, R)>,
        R: Send + Sync,
        F2: for<'r> Fn(crate::data::EntryRange, &'r R) -> Option<&'r [u8]> + Send + Sync + Clone,
    {
        if version != crate::index::Version::default() {
            return Err(Error::Unsupported(version));
        }
        let mut num_objects: usize = 0;
        let mut last_seen_trailer = None;
        let (anticipated_num_objects, upper_bound) = entries.size_hint();
        let worst_case_num_objects_after_thin_pack_resolution = upper_bound.unwrap_or(anticipated_num_objects);
        let mut tree = Tree::with_capacity(
            worst_case_num_objects_after_thin_pack_resolution,
            memory_budget.clone(),
        )?;
        let indexing_start = std::time::Instant::now();

        root_progress.init(Some(4), progress::steps());
        let mut objects_progress = root_progress.add_child_with_id("indexing".into(), ProgressId::IndexObjects.into());
        objects_progress.init(Some(anticipated_num_objects), progress::count("objects"));
        let mut decompressed_progress =
            root_progress.add_child_with_id("decompressing".into(), ProgressId::DecompressedBytes.into());
        decompressed_progress.init(None, progress::bytes());
        let mut pack_entries_end: u64 = 0;

        for entry in entries {
            let crate::data::input::Entry {
                header,
                pack_offset,
                crc32: _,
                header_size,
                compressed: _,
                compressed_size,
                decompressed_size,
                trailer,
            } = entry?;

            decompressed_progress.inc_by(decompressed_size as usize);

            let entry_len = u64::from(header_size) + compressed_size;
            pack_entries_end = pack_offset + entry_len;

            use crate::data::entry::Header::*;
            match header {
                Tree | Blob | Commit | Tag => {
                    tree.add_root(pack_offset)?;
                }
                RefDelta { .. } => return Err(Error::IteratorInvariantNoRefDelta),
                OfsDelta { base_distance } => {
                    let base_pack_offset =
                        crate::data::entry::Header::verified_base_pack_offset(pack_offset, base_distance).ok_or(
                            Error::IteratorInvariantBaseOffset {
                                pack_offset,
                                distance: base_distance,
                            },
                        )?;
                    tree.add_child(base_pack_offset, pack_offset)?;
                }
            }
            last_seen_trailer = trailer;
            num_objects += 1;
            objects_progress.inc();
        }
        let num_objects: u32 = num_objects
            .try_into()
            .map_err(|_| Error::IteratorInvariantTooManyObjects(num_objects))?;

        objects_progress.show_throughput(indexing_start);
        decompressed_progress.show_throughput(indexing_start);
        drop(objects_progress);
        drop(decompressed_progress);

        root_progress.inc();

        let (resolver, pack) = make_resolver().map_err(gix_hash::io::Error::from)?;

        // Clone the budget so it outlives the traverse call (which
        // moves it into Options) and remains available for the
        // external sort phase. Both clones share the same Arc-backed
        // counter — reservations from the traverse path and from the
        // sort path are charged against the same cap.
        let memory_budget_for_sort = memory_budget.clone();

        let sorted_entries: external_sort::SortedIter = {
            let sort_spool = std::sync::Arc::new(
                crate::cache::delta::traverse::spool::SpoolHandle::new(),
            );
            let sorter = external_sort::ExternalSorter::with_budget(
                &memory_budget_for_sort,
                sort_spool,
            )?;
            let sorter = std::sync::Mutex::new(sorter);

            let resolver_for_crc = resolver.clone();
            let remaining_buffers = tree.traverse(
                resolver,
                &pack,
                pack_entries_end,
                {
                    let hash_kind = version.hash();
                    let sorter_ref = &sorter;
                    let pack_ref = &pack;
                    let resolver_ref = &resolver_for_crc;
                    move |offset: crate::data::Offset,
                          _progress: &dyn gix_features::progress::Progress,
                          traverse::Context {
                              entry: pack_entry,
                              entry_end,
                              decompressed: bytes,
                              ..
                          },
                          acc: &std::sync::Mutex<Vec<IndexEntry>>|
                          -> Result<(), std::io::Error> {
                        let object_kind = pack_entry
                            .header
                            .as_kind()
                            .expect("base object as source of iteration");
                        let id = gix_object::compute_hash(hash_kind, object_kind, bytes)
                            .map_err(std::io::Error::other)?;
                        let raw_entry = resolver_ref(offset..entry_end, pack_ref)
                            .expect("resolver must succeed for traversed entries");
                        let crc32 = gix_features::hash::crc32(raw_entry);
                        let mut buf = acc.lock()
                            .expect("batch buffer mutex must not be poisoned");
                        buf.push(IndexEntry { id, crc32, offset });
                        if buf.len() >= SORT_BATCH_SIZE {
                            let mut s = sorter_ref.lock()
                                .expect("sorter mutex must not be poisoned");
                            for entry in buf.drain(..) {
                                s.push(entry).map_err(std::io::Error::other)?;
                            }
                        }
                        Ok(())
                    }
                },
                || std::sync::Mutex::new(Vec::with_capacity(SORT_BATCH_SIZE)),
                traverse::Options {
                    object_progress: Box::new(
                        root_progress.add_child_with_id("Resolving".into(), ProgressId::ResolveObjects.into()),
                    ),
                    size_progress: &mut root_progress
                        .add_child_with_id("Decoding".into(), ProgressId::DecodedBytes.into()),
                    thread_limit,
                    should_interrupt,
                    object_hash,
                    memory_budget,
                },
            )?;
            root_progress.inc();

            {
                let mut s = sorter.lock()
                    .expect("sorter mutex must not be poisoned");
                for buf_mutex in &remaining_buffers {
                    let mut buf = buf_mutex.lock()
                        .expect("batch buffer mutex must not be poisoned");
                    for entry in buf.drain(..) {
                        s.push(entry)?;
                    }
                }
            }

            let sorter = sorter
                .into_inner()
                .expect("sorter mutex must not be poisoned on into_inner");
            let sorted = sorter.finish()?;
            root_progress.inc();
            sorted
        };

        let pack_hash = match last_seen_trailer {
            Some(ph) => ph,
            None if num_objects == 0 => {
                let header = crate::data::header::encode(pack_version, 0);
                let mut hasher = gix_hash::hasher(object_hash);
                hasher.update(&header);
                hasher.try_finalize().map_err(gix_hash::io::Error::from)?
            }
            None => return Err(Error::IteratorInvariantTrailer),
        };
        let index_hash = crate::index::encode::write_to(
            out,
            sorted_entries,
            num_objects,
            &pack_hash,
            version,
            &mut root_progress.add_child_with_id("writing index file".into(), ProgressId::IndexBytesWritten.into()),
        )?;
        root_progress.show_throughput_with(
            indexing_start,
            num_objects as usize,
            progress::count("objects").expect("unit always set"),
            progress::MessageLevel::Success,
        );
        Ok(Outcome {
            index_version: version,
            index_hash,
            data_hash: pack_hash,
            num_objects,
        })
    }
}

