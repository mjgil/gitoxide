use std::sync::atomic::{AtomicBool, Ordering};

use gix_features::{parallel, progress::DynNestedProgress};

use super::Error;
use crate::{
    cache::delta::traverse,
    index::{self, traverse::Outcome, util::index_entries_sorted_by_offset_ascending},
};

/// Traversal options for [`traverse_with_index()`][index::File::traverse_with_index()]
#[derive(Default)]
pub struct Options {
    /// If `Some`, only use the given number of threads. Otherwise, the number of threads to use will be selected based on
    /// the number of available logical cores.
    pub thread_limit: Option<usize>,
    /// The kinds of safety checks to perform.
    pub check: crate::index::traverse::SafetyCheck,
}

/// The progress ids used in [`index::File::traverse_with_index()`].
///
/// Use this information to selectively extract the progress of interest in case the parent application has custom visualization.
#[derive(Debug, Copy, Clone)]
pub enum ProgressId {
    /// The amount of bytes currently processed to generate a checksum of the *pack data file*.
    HashPackDataBytes,
    /// The amount of bytes currently processed to generate a checksum of the *pack index file*.
    HashPackIndexBytes,
    /// Collect all object hashes into a vector and sort it by their pack offset.
    CollectSortedIndexEntries,
    /// Count the objects processed when building a cache tree from all objects in a pack index.
    TreeFromOffsetsObjects,
    /// The amount of objects which were decoded.
    DecodedObjects,
    /// The amount of bytes that were decoded in total, as the sum of all bytes to represent all decoded objects.
    DecodedBytes,
}

impl From<ProgressId> for gix_features::progress::Id {
    fn from(v: ProgressId) -> Self {
        match v {
            ProgressId::HashPackDataBytes => *b"PTHP",
            ProgressId::HashPackIndexBytes => *b"PTHI",
            ProgressId::CollectSortedIndexEntries => *b"PTCE",
            ProgressId::TreeFromOffsetsObjects => *b"PTDI",
            ProgressId::DecodedObjects => *b"PTRO",
            ProgressId::DecodedBytes => *b"PTDB",
        }
    }
}

/// Traversal with index
impl index::File {
    /// Iterate through all _decoded objects_ in the given `pack` and handle them with a `Processor`, using an index to reduce waste
    /// at the cost of memory.
    ///
    /// For more details, see the documentation on the [`traverse()`][index::File::traverse()] method.
    pub fn traverse_with_index<Processor, E>(
        &self,
        pack: &crate::data::File,
        processor: Processor,
        progress: &mut dyn DynNestedProgress,
        should_interrupt: &AtomicBool,
        Options { check, thread_limit }: Options,
    ) -> Result<Outcome, Error<E>>
    where
        Processor: FnMut(gix_object::Kind, &[u8], &index::Entry, &dyn gix_features::progress::Progress) -> Result<(), E>
            + Send
            + Clone,
        E: std::error::Error + Send + Sync + 'static,
    {
        let (verify_result, traversal_result) = parallel::join(
            {
                let mut pack_progress = progress.add_child_with_id(
                    format!(
                        "Hash of pack '{}'",
                        pack.path().file_name().expect("pack has filename").to_string_lossy()
                    ),
                    ProgressId::HashPackDataBytes.into(),
                );
                let mut index_progress = progress.add_child_with_id(
                    format!(
                        "Hash of index '{}'",
                        self.path.file_name().expect("index has filename").to_string_lossy()
                    ),
                    ProgressId::HashPackIndexBytes.into(),
                );
                move || {
                    let res =
                        self.possibly_verify(pack, check, &mut pack_progress, &mut index_progress, should_interrupt);
                    if res.is_err() {
                        should_interrupt.store(true, Ordering::SeqCst);
                    }
                    res
                }
            },
            || -> Result<_, Error<_>> {
                let sorted_entries = index_entries_sorted_by_offset_ascending(
                    self,
                    &mut progress.add_child_with_id(
                        "collecting sorted index".into(),
                        ProgressId::CollectSortedIndexEntries.into(),
                    ),
                );
                let tree = crate::cache::delta::Tree::from_offsets_in_pack(
                    pack.path(),
                    sorted_entries.iter().cloned(),
                    &|e: &index::Entry| e.pack_offset,
                    &|id| self.lookup(id).map(|idx| self.pack_offset_at_index(idx)),
                    &mut progress.add_child_with_id("indexing".into(), ProgressId::TreeFromOffsetsObjects.into()),
                    should_interrupt,
                    self.object_hash,
                )?;
                let stats = std::sync::Mutex::new(index::traverse::Statistics::default());
                let num_nodes_counter = std::sync::atomic::AtomicU64::new(0);
                let processor = std::sync::Mutex::new(processor);
                tree.traverse(
                    |slice, pack: &crate::data::File, buf: &mut Vec<u8>| {
                        match pack.entry_slice(slice) {
                            Some(bytes) => {
                                buf.clear();
                                buf.extend_from_slice(bytes);
                                true
                            }
                            None => false,
                        }
                    },
                    pack,
                    pack.pack_end() as u64,
                    |offset: crate::data::Offset,
                     progress: &dyn gix_features::progress::Progress,
                     traverse::Context {
                         entry: pack_entry,
                         entry_end,
                         decompressed: bytes,
                         level,
                     },
                     _acc: &()|
                     -> Result<(), Error<E>> {
                        let object_kind = pack_entry.header.as_kind().expect("non-delta object");
                        let index_entry = sorted_entries
                            .binary_search_by_key(&offset, |e| e.pack_offset)
                            .map(|i| &sorted_entries[i])
                            .expect("every traversed offset has a matching index entry");
                        let compressed_size = entry_end - pack_entry.data_offset;
                        let decompressed_size = pack_entry.decompressed_size;
                        let object_size = bytes.len() as u64;
                        let result = index::traverse::process_entry(
                            check,
                            object_kind,
                            bytes,
                            index_entry,
                            || {
                                gix_features::hash::crc32(
                                    pack.entry_slice(index_entry.pack_offset..entry_end)
                                        .expect("slice pointing into the pack (by now data is verified)"),
                                )
                            },
                            progress,
                            &mut *processor.lock().expect("processor mutex must not be poisoned"),
                        );
                        match result {
                            Err(err @ Error::PackDecode { .. }) if !check.fatal_decode_error() => {
                                progress.info(format!("Ignoring decode error: {err}"));
                            }
                            Err(e) => return Err(e),
                            Ok(()) => {}
                        }
                        num_nodes_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let mut s = stats.lock().expect("stats mutex must not be poisoned");
                        s.total_compressed_entries_size += compressed_size;
                        s.total_decompressed_entries_size += decompressed_size;
                        s.total_object_size += object_size;
                        *s.objects_per_chain_length.entry(u32::from(level)).or_insert(0) += 1;
                        s.average.decompressed_size += decompressed_size;
                        s.average.compressed_size += compressed_size as usize;
                        s.average.object_size += object_size;
                        s.average.num_deltas += u32::from(level);
                        use gix_object::Kind::*;
                        match object_kind {
                            Blob => s.num_blobs += 1,
                            Tree => s.num_trees += 1,
                            Tag => s.num_tags += 1,
                            Commit => s.num_commits += 1,
                        }
                        Ok(())
                    },
                    || (),
                    traverse::Options {
                        object_progress: Box::new(
                            progress.add_child_with_id("Resolving".into(), ProgressId::DecodedObjects.into()),
                        ),
                        size_progress:
                            &mut progress.add_child_with_id("Decoding".into(), ProgressId::DecodedBytes.into()),
                        thread_limit,
                        should_interrupt,
                        object_hash: self.object_hash,
                        memory_budget: gix_features::budget::MemoryBudget::unlimited(),
                    },
                )?;
                let mut outcome = stats
                    .into_inner()
                    .expect("stats mutex must not be poisoned on into_inner");
                let num_nodes =
                    num_nodes_counter.load(std::sync::atomic::Ordering::Relaxed) as usize;
                finalize_statistics_averages(&mut outcome, num_nodes);
                outcome.pack_size = pack.data_len() as u64;
                Ok(outcome)
            },
        );
        Ok(Outcome {
            actual_index_checksum: verify_result?,
            statistics: traversal_result?,
        })
    }
}

fn finalize_statistics_averages(stats: &mut index::traverse::Statistics, num_nodes: usize) {
    stats.average.decompressed_size /= num_nodes as u64;
    stats.average.compressed_size /= num_nodes;
    stats.average.object_size /= num_nodes as u64;
    stats.average.num_deltas /= num_nodes as u32;
}
