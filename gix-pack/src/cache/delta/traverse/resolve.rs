use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
        Arc,
    },
};

use gix_features::{
    budget::{MemoryBudget, Reservation},
    progress::Progress,
    threading, zlib,
};

use crate::{
    cache::delta::{
        item_store::ItemMetadata,
        traverse::{spool::SpoolHandle, Context, Error},
    },
    data,
    data::EntryRange,
};

use super::spool::SpoolFile;

struct DecodedDelta {
    entry: data::Entry,
    entry_end: u64,
    storage: DeltaStorage,
}

enum DeltaStorage {
    InMemory {
        bytes: Vec<u8>,
        _reservation: Reservation,
    },
    Spilled {
        spool: Arc<SpoolFile>,
        offset: u64,
        len: usize,
    },
}

impl DecodedDelta {
    fn store(
        entry: data::Entry,
        entry_end: u64,
        bytes: Vec<u8>,
        budget: &MemoryBudget,
        spool_handle: &SpoolHandle,
    ) -> Result<Self, Error> {
        match budget.reserve(bytes.len()) {
            Ok(reservation) => Ok(Self {
                entry,
                entry_end,
                storage: DeltaStorage::InMemory {
                    bytes,
                    _reservation: reservation,
                },
            }),
            Err(_out_of_budget) => {
                let spool = spool_handle.get_or_create().map_err(Error::SpoolIo)?;
                let len = bytes.len();
                let offset = spool.append(&bytes).map_err(Error::SpoolIo)?;
                drop(bytes);
                Ok(Self {
                    entry,
                    entry_end,
                    storage: DeltaStorage::Spilled { spool, offset, len },
                })
            }
        }
    }

    fn into_parts(self) -> Result<(data::Entry, u64, Vec<u8>), Error> {
        let bytes = match self.storage {
            DeltaStorage::InMemory { bytes, .. } => bytes,
            DeltaStorage::Spilled { spool, offset, len } => {
                spool.read_exact(offset, len).map_err(Error::SpoolIo)?
            }
        };
        Ok((self.entry, self.entry_end, bytes))
    }
}

pub(crate) mod root {
    use crate::cache::delta::item_store::ItemMetadata;

    pub(crate) struct Node<'a> {
        idx: u32,
        metadata: &'a ItemMetadata<'a>,
    }

    impl<'a> Node<'a> {
        #[allow(unsafe_code)]
        pub(super) unsafe fn new(
            idx: u32,
            metadata: &'a ItemMetadata<'a>,
        ) -> Self {
            Node { idx, metadata }
        }
    }

    impl<'a> Node<'a> {
        pub fn offset(&self) -> u64 {
            self.metadata.offset(self.idx)
        }

        pub fn entry_slice(&self) -> crate::data::EntryRange {
            self.metadata.offset(self.idx)..self.metadata.next_offset(self.idx)
        }

        pub fn has_children(&self) -> bool {
            !self.metadata.children(self.idx).is_empty()
        }

        pub fn into_child_iter(self) -> impl Iterator<Item = Node<'a>> + 'a {
            let metadata = self.metadata;
            #[allow(unsafe_code)]
            self.metadata.children(self.idx).iter().map(move |&child_idx| {
                unsafe { Node::new(child_idx, metadata) }
            })
        }
    }
}

pub(super) struct State<'items, F, SINK, A> {
    pub delta_bytes: Vec<u8>,
    pub fully_resolved_delta_bytes: Vec<u8>,
    pub progress: Box<dyn Progress>,
    pub resolve: F,
    pub sink: SINK,
    pub metadata: &'items ItemMetadata<'items>,
    pub memory_budget: MemoryBudget,
    pub spool: Arc<SpoolHandle>,
    pub accumulator: A,
}

#[allow(clippy::too_many_arguments, unsafe_code)]
#[deny(unsafe_op_in_unsafe_fn)]
pub(super) unsafe fn deltas<F, SINK, E, R, A>(
    objects: gix_features::progress::StepShared,
    size: gix_features::progress::StepShared,
    root_idx: &mut u32,
    State {
        delta_bytes,
        fully_resolved_delta_bytes,
        progress,
        resolve,
        sink,
        metadata,
        memory_budget,
        spool,
        accumulator,
    }: &mut State<'_, F, SINK, A>,
    resolve_data: &R,
    hash_len: usize,
    threads_left: &AtomicIsize,
    should_interrupt: &AtomicBool,
) -> Result<(), Error>
where
    R: Send + Sync,
    F: for<'r> Fn(EntryRange, &'r R) -> Option<&'r [u8]> + Send + Clone,
    SINK: FnMut(crate::data::Offset, &dyn Progress, Context<'_>, &A) -> Result<(), E> + Send + Clone,
    E: std::error::Error + Send + Sync + 'static,
    A: Send + Sync,
{
    let mut decompressed_bytes_by_pack_offset: BTreeMap<u64, DecodedDelta> = BTreeMap::new();
    let mut inflate = zlib::Inflate::default();
    let mut decompress_from_resolver = |slice: EntryRange, out: &mut Vec<u8>| -> Result<(data::Entry, u64), Error> {
        let bytes = resolve(slice.clone(), resolve_data).ok_or(Error::ResolveFailed {
            pack_offset: slice.start,
        })?;
        let entry = data::Entry::from_bytes(bytes, slice.start, hash_len)?;
        let compressed = &bytes[entry.header_size()..];
        let decompressed_len = entry.decompressed_size as usize;
        decompress_all_at_once_with(&mut inflate, compressed, decompressed_len, out)?;
        Ok((entry, slice.end))
    };

    let root_level = 0;
    #[allow(unsafe_code)]
    let root_node = unsafe { root::Node::new(*root_idx, metadata) };
    let mut nodes: Vec<_> = vec![(root_level, root_node)];
    while let Some((level, base)) = nodes.pop() {
        if should_interrupt.load(Ordering::Relaxed) {
            return Err(Error::Interrupted);
        }
        let (base_entry, entry_end, base_bytes) = if level == root_level {
            let mut buf = Vec::new();
            let (a, b) = decompress_from_resolver(base.entry_slice(), &mut buf)?;
            (a, b, buf)
        } else {
            decompressed_bytes_by_pack_offset
                .remove(&base.offset())
                .expect("we store the resolved delta buffer when done")
                .into_parts()?
        };

        {
            let base_offset = base.offset();
            sink(
                base_offset,
                progress,
                Context {
                    entry: &base_entry,
                    entry_end,
                    decompressed: &base_bytes,
                    level,
                },
                accumulator,
            )
            .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
            objects.fetch_add(1, Ordering::Relaxed);
            size.fetch_add(base_bytes.len(), Ordering::Relaxed);
        }

        for child in base.into_child_iter() {
            let (mut child_entry, entry_end) = decompress_from_resolver(child.entry_slice(), delta_bytes)?;
            let (base_size, consumed) = data::delta::decode_header_size(delta_bytes);
            let mut header_ofs = consumed;
            assert_eq!(
                base_bytes.len(),
                base_size as usize,
                "recorded base size in delta does match the actual one"
            );
            let (result_size, consumed) = data::delta::decode_header_size(&delta_bytes[consumed..]);
            header_ofs += consumed;

            fully_resolved_delta_bytes.resize(result_size as usize, 0);
            data::delta::apply(&base_bytes, fully_resolved_delta_bytes, &delta_bytes[header_ofs..])?;

            // FIXME: this actually invalidates the "pack_offset()" computation, which is not obvious to consumers
            //        at all
            child_entry.header = base_entry.header;
            if child.has_children() {
                let entry = DecodedDelta::store(
                    child_entry,
                    entry_end,
                    std::mem::take(fully_resolved_delta_bytes),
                    memory_budget,
                    spool,
                )?;
                decompressed_bytes_by_pack_offset.insert(child.offset(), entry);
                nodes.push((level + 1, child));
            } else {
                let child_offset = child.offset();
                sink(
                    child_offset,
                    &progress,
                    Context {
                        entry: &child_entry,
                        entry_end,
                        decompressed: fully_resolved_delta_bytes,
                        level: level + 1,
                    },
                    accumulator,
                )
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
                objects.fetch_add(1, Ordering::Relaxed);
                size.fetch_add(base_bytes.len(), Ordering::Relaxed);
            }
        }

        if nodes.len() > 1 {
            if let Ok(initial_threads) =
                threads_left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |threads_available| {
                    (threads_available > 0).then_some(0)
                })
            {
                *delta_bytes = Vec::new();
                *fully_resolved_delta_bytes = Vec::new();
                return deltas_mt(
                    initial_threads,
                    decompressed_bytes_by_pack_offset,
                    objects,
                    size,
                    &progress,
                    nodes,
                    resolve.clone(),
                    resolve_data,
                    sink.clone(),
                    hash_len,
                    threads_left,
                    should_interrupt,
                    memory_budget.clone(),
                    Arc::clone(spool),
                    accumulator,
                );
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn deltas_mt<F, SINK, E, R, A>(
    mut threads_to_create: isize,
    decompressed_bytes_by_pack_offset: BTreeMap<u64, DecodedDelta>,
    objects: gix_features::progress::StepShared,
    size: gix_features::progress::StepShared,
    progress: &dyn Progress,
    nodes: Vec<(u16, root::Node<'_>)>,
    resolve: F,
    resolve_data: &R,
    sink: SINK,
    hash_len: usize,
    threads_left: &AtomicIsize,
    should_interrupt: &AtomicBool,
    memory_budget: MemoryBudget,
    spool: Arc<SpoolHandle>,
    accumulator: &A,
) -> Result<(), Error>
where
    R: Send + Sync,
    F: for<'r> Fn(EntryRange, &'r R) -> Option<&'r [u8]> + Send + Clone,
    SINK: FnMut(crate::data::Offset, &dyn Progress, Context<'_>, &A) -> Result<(), E> + Send + Clone,
    E: std::error::Error + Send + Sync + 'static,
    A: Send + Sync,
{
    let nodes = gix_features::threading::Mutable::new(nodes);
    let decompressed_bytes_by_pack_offset = gix_features::threading::Mutable::new(decompressed_bytes_by_pack_offset);
    threads_to_create += 1;
    let mut returned_ourselves = false;

    gix_features::parallel::threads(|s| -> Result<(), Error> {
        let mut threads = Vec::new();
        let poll_interval = std::time::Duration::from_millis(100);
        loop {
            for tid in 0..threads_to_create {
                let thread = gix_features::parallel::build_thread()
                    .name(format!("gix-pack.traverse_deltas.{tid}"))
                    .spawn_scoped(s, {
                        let nodes = &nodes;
                        let decompressed_bytes_by_pack_offset = &decompressed_bytes_by_pack_offset;
                        let resolve = resolve.clone();
                        let objects = &objects;
                        let size = &size;
                        let memory_budget = memory_budget.clone();
                        let spool = Arc::clone(&spool);
                        let mut sink = sink.clone();

                        move || -> Result<(), Error> {
                            let mut fully_resolved_delta_bytes = Vec::new();
                            let mut delta_bytes = Vec::new();
                            let mut inflate = zlib::Inflate::default();
                            let mut decompress_from_resolver =
                                |slice: EntryRange, out: &mut Vec<u8>| -> Result<(data::Entry, u64), Error> {
                                    let bytes = resolve(slice.clone(), resolve_data).ok_or(Error::ResolveFailed {
                                        pack_offset: slice.start,
                                    })?;
                                    let entry = data::Entry::from_bytes(bytes, slice.start, hash_len)?;
                                    let compressed = &bytes[entry.header_size()..];
                                    let decompressed_len = entry.decompressed_size as usize;
                                    decompress_all_at_once_with(&mut inflate, compressed, decompressed_len, out)?;
                                    Ok((entry, slice.end))
                                };

                            loop {
                                let (level, base) = match threading::lock(nodes).pop() {
                                    Some(v) => v,
                                    None => break,
                                };
                                if should_interrupt.load(Ordering::Relaxed) {
                                    return Err(Error::Interrupted);
                                }
                                let (base_entry, entry_end, base_bytes) = if level == 0 {
                                    let mut buf = Vec::new();
                                    let (a, b) = decompress_from_resolver(base.entry_slice(), &mut buf)?;
                                    (a, b, buf)
                                } else {
                                    let entry = threading::lock(decompressed_bytes_by_pack_offset)
                                        .remove(&base.offset())
                                        .expect("we store the resolved delta buffer when done");
                                    entry.into_parts()?
                                };

                                {
                                    let base_offset = base.offset();
                                    sink(
                                        base_offset,
                                        progress,
                                        Context {
                                            entry: &base_entry,
                                            entry_end,
                                            decompressed: &base_bytes,
                                            level,
                                        },
                                        accumulator,
                                    )
                                    .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
                                    objects.fetch_add(1, Ordering::Relaxed);
                                    size.fetch_add(base_bytes.len(), Ordering::Relaxed);
                                }

                                for child in base.into_child_iter() {
                                    let (mut child_entry, entry_end) =
                                        decompress_from_resolver(child.entry_slice(), &mut delta_bytes)?;
                                    let (base_size, consumed) = data::delta::decode_header_size(&delta_bytes);
                                    let mut header_ofs = consumed;
                                    assert_eq!(
                                        base_bytes.len(),
                                        base_size as usize,
                                        "recorded base size in delta does match the actual one"
                                    );
                                    let (result_size, consumed) =
                                        data::delta::decode_header_size(&delta_bytes[consumed..]);
                                    header_ofs += consumed;

                                    fully_resolved_delta_bytes.resize(result_size as usize, 0);
                                    data::delta::apply(
                                        &base_bytes,
                                        &mut fully_resolved_delta_bytes,
                                        &delta_bytes[header_ofs..],
                                    )?;

                                    child_entry.header = base_entry.header;
                                    if child.has_children() {
                                        let entry = DecodedDelta::store(
                                            child_entry,
                                            entry_end,
                                            std::mem::take(&mut fully_resolved_delta_bytes),
                                            &memory_budget,
                                            &spool,
                                        )?;
                                        threading::lock(decompressed_bytes_by_pack_offset)
                                            .insert(child.offset(), entry);
                                        threading::lock(nodes).push((level + 1, child));
                                    } else {
                                        let child_offset = child.offset();
                                        sink(
                                            child_offset,
                                            progress,
                                            Context {
                                                entry: &child_entry,
                                                entry_end,
                                                decompressed: &fully_resolved_delta_bytes,
                                                level: level + 1,
                                            },
                                            accumulator,
                                        )
                                        .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
                                        objects.fetch_add(1, Ordering::Relaxed);
                                        size.fetch_add(base_bytes.len(), Ordering::Relaxed);
                                    }
                                }
                            }
                            Ok(())
                        }
                    })?;
                threads.push(thread);
            }
            if threads_left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |threads_available: isize| {
                    (threads_available > 0).then(|| {
                        threads_to_create = threads_available.min(threading::lock(&nodes).len() as isize);
                        threads_available - threads_to_create
                    })
                })
                .is_err()
            {
                threads_to_create = 0;
            }

            std::thread::sleep(poll_interval);
            #[allow(clippy::redundant_closure_for_method_calls)]
            if threads.iter().any(|t| t.is_finished()) {
                let mut running_threads = Vec::new();
                for thread in threads.drain(..) {
                    if thread.is_finished() {
                        match thread.join() {
                            Ok(Err(err)) => return Err(err),
                            Ok(Ok(())) => {
                                if !returned_ourselves {
                                    returned_ourselves = true;
                                } else {
                                    threads_left.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            Err(err) => {
                                std::panic::resume_unwind(err);
                            }
                        }
                    } else {
                        running_threads.push(thread);
                    }
                }
                if running_threads.is_empty() && threading::lock(&nodes).is_empty() {
                    break;
                }
                threads = running_threads;
            }
        }

        Ok(())
    })
}

fn decompress_all_at_once_with(
    inflate: &mut zlib::Inflate,
    b: &[u8],
    decompressed_len: usize,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    out.resize(decompressed_len, 0);
    inflate.reset();
    inflate.once(b, out).map_err(|err| Error::ZlibInflate {
        source: err,
        message: "Failed to decompress entry",
    })?;
    Ok(())
}
