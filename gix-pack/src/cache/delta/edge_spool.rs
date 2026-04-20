//! External merge sort for delta-tree `(base_offset, child_idx)`
//! edges, producing a sorted-by-`parent_idx` stream ready for
//! [`super::item_store::ItemStoreBuilder::finish`].
//!
//! # Why this exists
//!
//! Step 5.5-b of the bounded-memory plan. Step 5.5-a introduced
//! [`super::item_store::ItemStore`] as the disk-backed replacement
//! for `Vec<Item<T>>`, but `ItemStore::finish` needs its edges
//! already sorted by `parent_idx` — and during
//! [`super::tree::Tree::add_child`] the parent's index is often
//! not yet known (it's known only as a `base_offset` in the pack,
//! and the parent may not have been inserted yet; today that's
//! what the `future_child_offsets` queue exists for).
//!
//! This module replaces that in-memory queue with an externally
//! sortable spool: [`EdgeSpool::push`] records one edge per
//! `add_child` call, and [`EdgeSpool::resolve`] turns the resulting
//! bag of edges into a sorted-by-`parent_idx` iterator by
//! exploiting a structural invariant of `Tree`:
//!
//! > Items are inserted with strictly increasing `offset`. Hence
//! > `items[idx].offset` is monotonic in `idx`, and sorting edges
//! > by `base_offset` is *equivalent* to sorting them by
//! > `parent_idx`.
//!
//! So only **one** external sort is needed (by `base_offset`),
//! followed by a linear merge-join against
//! [`super::item_store::ItemMetadata`]. The output is the
//! `(parent_idx, child_idx)` stream `ItemStoreBuilder::finish`
//! consumes, and it comes out sorted by `parent_idx` for free.
//!
//! # Not yet wired
//!
//! Like [`super::item_store`] in step 5.5-a, this module ships in
//! isolation with unit tests; nothing in `gix-pack` calls it yet.
//! Wiring into [`super::tree::Tree`] happens in step 5.5-c.
//! The step-wise cadence matches what we did with
//! [`ExternalSorter`][crate::index::write::external_sort::ExternalSorter]:
//! the primitive lands in isolation first, integration follows.
//!
//! # Memory accounting
//!
//! The spool holds one in-RAM chunk whose capacity is sized from
//! the [`MemoryBudget`] at construction. One [`Reservation`] of
//! size `chunk_capacity * 16` bytes is held for the spool's
//! lifetime — 16 rather than 12 because the in-memory [`Edge`]
//! type has 4 bytes of trailing alignment padding (`u64` then
//! `u32`); the on-disk record is a packed 12 bytes. The accounting
//! tracks the honest in-RAM footprint rather than the serialized
//! one.
//!
//! Under [`MemoryBudget::unlimited`] the whole edge set lives in
//! one in-memory chunk, sorted in place, and [`resolve`][EdgeSpool::resolve]
//! never touches disk. Under a tight budget (down to zero) the
//! spool falls back to a [`MIN_CHUNK_EDGES`]-sized unaccounted
//! chunk — same structural floor as
//! [`ExternalSorter::with_budget`][crate::index::write::external_sort::ExternalSorter::with_budget].
//!
//! # Endianness
//!
//! The spool tempfile is process-local and dies with the process
//! (anonymous `tempfile::tempfile` — `O_TMPFILE` on Linux). Records
//! are written host-byte-order; no cross-process portability is a
//! goal. Same argument as in [`super::item_store`].

#![allow(dead_code)] // step 5.5-b ships in isolation; wiring in 5.5-c.

use std::{
    collections::{BinaryHeap, VecDeque},
    io,
    sync::Arc,
};

use gix_features::budget::{MemoryBudget, Reservation};

use super::{
    item_store::ItemMetadata,
    traverse::spool::{SpoolFile, SpoolHandle},
};

/// Serialized width of one [`Edge`] on the spool: `u64 base_offset`
/// (8 bytes) + `u32 child_idx` (4 bytes), no padding. Host byte
/// order on the wire; the spool file is process-local.
pub(super) const SERIALIZED_EDGE_SIZE: usize = 12;

/// In-RAM size of one [`Edge`]. Named rather than `size_of::<Edge>()`
/// so the budget reservation math stays readable: a chunk of
/// `chunk_capacity` edges reserves `chunk_capacity *
/// IN_RAM_EDGE_SIZE` bytes.
const IN_RAM_EDGE_SIZE: usize = std::mem::size_of::<Edge>();

const _: () = assert!(
    IN_RAM_EDGE_SIZE == 16,
    "Edge is expected to be 16 bytes (u64 + u32 + 4 bytes trailing pad)",
);

/// Minimum chunk size in edges. Below this the fixed per-chunk
/// cost (one [`ChunkRef`], one read-ahead buffer during merge)
/// dominates the payload and the sort degenerates into a
/// pathologically-wide k-way merge. 128 edges × 16 B ≈ 2 KiB —
/// same structural floor as
/// [`ExternalSorter`][crate::index::write::external_sort::ExternalSorter].
const MIN_CHUNK_EDGES: usize = 128;

/// Target chunk size in edges under unconstrained budget. 65,536
/// edges × 16 B = 1 MiB in RAM (768 KiB serialized). Large enough
/// to amortize the spool append syscall; small enough that most
/// real packs finish in a single chunk without ever spilling.
const DEFAULT_CHUNK_EDGES: usize = 65_536;

/// Number of edges buffered per cursor during k-way merge.
/// 128 × 12 B = 1.5 KiB per cursor; hundreds of cursors still fit
/// comfortably under a tight budget.
const CURSOR_BUFFER_EDGES: usize = 128;

/// Errors from the edge-spool path.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("edge-spool I/O failed")]
    SpoolIo(#[source] io::Error),
    #[error(transparent)]
    OutOfBudget(#[from] gix_features::budget::OutOfBudget),
    /// Returned from [`ResolvedEdges::next`] when an edge's
    /// `base_offset` doesn't appear in the supplied
    /// [`ItemMetadata`]. In practice this should only be reachable
    /// from test fixtures that feed the spool a mismatched
    /// metadata; the real wiring in step 5.5-c only pushes edges
    /// whose base is known to be in the pack.
    #[error("edge referenced base_offset {base_offset} not present in items metadata")]
    UnresolvedBaseOffset { base_offset: u64 },
}

/// One delta edge: "the item at `child_idx` depends on the item
/// whose pack offset is `base_offset`". Copy; packed on the spool,
/// padded in memory.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Edge {
    base_offset: u64,
    child_idx: u32,
}

/// Descriptor for one spilled, sorted chunk in the spool file.
/// `count` is the number of edges; the spool span is
/// `[offset, offset + count * SERIALIZED_EDGE_SIZE)`.
struct ChunkRef {
    offset: u64,
    count: usize,
}

/// Streaming builder. Call [`Self::push`] for every delta edge in
/// arbitrary order, then [`Self::resolve`] against the items'
/// metadata to get a sorted-by-`parent_idx` iterator.
pub(crate) struct EdgeSpool {
    spool: Arc<SpoolHandle>,
    chunk_capacity: usize,
    current_chunk: Vec<Edge>,
    _chunk_reservation: Reservation,
    spilled_chunks: Vec<ChunkRef>,
}

impl EdgeSpool {
    /// Open a new spool. `capacity_hint` caps the *initial* chunk
    /// target — useful when the caller knows ahead of time that
    /// the pack has few deltas so [`DEFAULT_CHUNK_EDGES`] would
    /// over-reserve. Pass the expected delta count, or
    /// `DEFAULT_CHUNK_EDGES` if unknown. `budget` sizes the chunk
    /// via probe-and-halve, exactly as
    /// [`ExternalSorter::with_budget`][crate::index::write::external_sort::ExternalSorter::with_budget]
    /// does.
    ///
    /// Under a pathologically tight budget (below the
    /// [`MIN_CHUNK_EDGES`] floor) this falls back to a
    /// zero-accounted reservation plus an unaccounted minimum
    /// chunk. This preserves the "bytes(0) still works" contract
    /// the step-5.3/5.4b spill paths rely on: the sorter never
    /// fails at construction, it just becomes I/O-bound.
    pub(crate) fn new(capacity_hint: usize, budget: MemoryBudget) -> io::Result<Self> {
        // Cap the initial target at capacity_hint — but never below
        // the structural minimum — so small packs don't pre-reserve
        // a megabyte chunk. If capacity_hint is tiny, budget probing
        // below will still halve down from here to MIN_CHUNK_EDGES.
        let initial_target = DEFAULT_CHUNK_EDGES.min(capacity_hint.max(MIN_CHUNK_EDGES));
        let mut target = initial_target;
        let reservation = loop {
            match budget.reserve(target * IN_RAM_EDGE_SIZE) {
                Ok(r) => break r,
                Err(_) if target > MIN_CHUNK_EDGES => {
                    target = (target / 2).max(MIN_CHUNK_EDGES);
                }
                Err(_) => {
                    // Below the structural minimum. Degrade to an
                    // unaccounted floor: zero-byte reservation
                    // against the shared budget, MIN_CHUNK_EDGES in
                    // RAM. Matches the `with_budget` contract in
                    // external_sort.rs.
                    target = MIN_CHUNK_EDGES;
                    break budget
                        .reserve(0)
                        .expect("zero-byte reservation always succeeds");
                }
            }
        };
        let current_chunk = Vec::with_capacity(target);
        Ok(Self {
            spool: Arc::new(SpoolHandle::new()),
            chunk_capacity: target,
            current_chunk,
            _chunk_reservation: reservation,
            spilled_chunks: Vec::new(),
        })
    }

    /// Record one edge. Cheap; may trigger a spool flush when the
    /// in-RAM chunk fills.
    pub(crate) fn push(&mut self, base_offset: u64, child_idx: u32) -> Result<(), Error> {
        self.current_chunk.push(Edge { base_offset, child_idx });
        if self.current_chunk.len() >= self.chunk_capacity {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Sort and spill the current chunk. Clears the chunk Vec but
    /// retains its allocation (and its reservation). No-op if the
    /// chunk is empty.
    fn flush_chunk(&mut self) -> Result<(), Error> {
        if self.current_chunk.is_empty() {
            return Ok(());
        }
        // Stable sort: edges with equal base_offset retain push
        // order. This in turn makes the final (parent_idx,
        // child_idx) stream emit children in the order they were
        // added to the tree — matching the existing Tree semantics
        // where `children: Vec<u32>` preserves insertion order and
        // is what today's `gix-pack-tests` byte-roundtrip checks
        // against canned .idx fixtures.
        self.current_chunk.sort_by_key(|e| e.base_offset);

        let mut buf = Vec::with_capacity(self.current_chunk.len() * SERIALIZED_EDGE_SIZE);
        for e in &self.current_chunk {
            buf.extend_from_slice(&e.base_offset.to_ne_bytes());
            buf.extend_from_slice(&e.child_idx.to_ne_bytes());
        }

        let spool = self.spool.get_or_create().map_err(Error::SpoolIo)?;
        let offset = spool.append(&buf).map_err(Error::SpoolIo)?;

        self.spilled_chunks.push(ChunkRef {
            offset,
            count: self.current_chunk.len(),
        });
        self.current_chunk.clear();
        Ok(())
    }

    /// Consume the spool and produce an iterator of
    /// `(parent_idx, child_idx)` pairs, sorted by `parent_idx`.
    ///
    /// The supplied `metadata` must describe the items the pushed
    /// edges' `base_offset`s refer to. See module docs for the
    /// invariant that lets us go from sorted-by-`base_offset` to
    /// sorted-by-`parent_idx` via a single linear merge-join.
    pub(crate) fn resolve<'a>(
        mut self,
        metadata: ItemMetadata<'a>,
    ) -> Result<ResolvedEdges<'a>, Error> {
        let stream = if self.spilled_chunks.is_empty() {
            // Fast path: everything in RAM. Sort once, iterate.
            self.current_chunk.sort_by_key(|e| e.base_offset);
            SortedEdgeStream::InMemory {
                edges: self.current_chunk.into_iter(),
            }
        } else {
            // Slow path: k-way merge over spilled chunks plus the
            // tail in-memory chunk (if any).
            self.flush_chunk()?;
            let spool = self.spool.get_or_create().map_err(Error::SpoolIo)?;
            let mut cursors: Vec<EdgeChunkCursor> = self
                .spilled_chunks
                .into_iter()
                .map(|c| EdgeChunkCursor::new(Arc::clone(&spool), c.offset, c.count))
                .collect();

            // Seed the heap with one entry per non-empty cursor.
            let mut heap: BinaryHeap<HeapEdge> = BinaryHeap::with_capacity(cursors.len());
            for (idx, cursor) in cursors.iter_mut().enumerate() {
                if let Some(edge) = cursor.next()? {
                    heap.push(HeapEdge {
                        edge,
                        cursor_idx: idx,
                    });
                }
            }
            SortedEdgeStream::Merged { cursors, heap }
        };

        Ok(ResolvedEdges {
            metadata,
            stream,
            cursor: 0,
        })
    }
}

/// Internal union of the two sorted-edge streams produced by
/// [`EdgeSpool::resolve`]: a vector iterator for the fast path, a
/// k-way heap merge for the slow path.
enum SortedEdgeStream {
    InMemory {
        edges: std::vec::IntoIter<Edge>,
    },
    Merged {
        cursors: Vec<EdgeChunkCursor>,
        heap: BinaryHeap<HeapEdge>,
    },
}

impl SortedEdgeStream {
    fn try_next(&mut self) -> Result<Option<Edge>, Error> {
        match self {
            SortedEdgeStream::InMemory { edges } => Ok(edges.next()),
            SortedEdgeStream::Merged { cursors, heap } => {
                let Some(top) = heap.pop() else {
                    return Ok(None);
                };
                // Refill this cursor before returning, so the heap
                // invariant holds on the next call.
                if let Some(next_edge) = cursors[top.cursor_idx].next()? {
                    heap.push(HeapEdge {
                        edge: next_edge,
                        cursor_idx: top.cursor_idx,
                    });
                }
                Ok(Some(top.edge))
            }
        }
    }
}

/// The `(parent_idx, child_idx)` iterator. Output is sorted by
/// `parent_idx` by construction, making it a direct feed for
/// [`super::item_store::ItemStoreBuilder::finish`].
pub(crate) struct ResolvedEdges<'a> {
    metadata: ItemMetadata<'a>,
    stream: SortedEdgeStream,
    /// Last-seen parent index. The sorted-by-`base_offset` edge
    /// stream and the monotonic-in-index offsets in metadata mean
    /// we never need to rewind. Starts at 0, advances
    /// non-decreasingly as edges arrive.
    cursor: u32,
}

impl<'a> Iterator for ResolvedEdges<'a> {
    type Item = Result<(u32, u32), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let edge = match self.stream.try_next() {
            Ok(Some(e)) => e,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };
        // Advance the metadata cursor to the first index whose
        // offset is ≥ edge.base_offset. Because edges arrive in
        // ascending base_offset order and metadata offsets are
        // strictly increasing in idx, the cursor only moves
        // forward. Total work across the whole iteration is O(N+E).
        let num_items = self.metadata.num_items();
        while self.cursor < num_items && self.metadata.offset(self.cursor) < edge.base_offset {
            self.cursor += 1;
        }
        if self.cursor >= num_items || self.metadata.offset(self.cursor) != edge.base_offset {
            return Some(Err(Error::UnresolvedBaseOffset {
                base_offset: edge.base_offset,
            }));
        }
        Some(Ok((self.cursor, edge.child_idx)))
    }
}

/// One cursor over a single spilled chunk. Reads
/// [`CURSOR_BUFFER_EDGES`] at a time so the spool is touched in
/// blocks rather than per-edge.
struct EdgeChunkCursor {
    spool: Arc<SpoolFile>,
    next_read_offset: u64,
    remaining: usize,
    buffer: VecDeque<Edge>,
}

impl EdgeChunkCursor {
    fn new(spool: Arc<SpoolFile>, start_offset: u64, count: usize) -> Self {
        Self {
            spool,
            next_read_offset: start_offset,
            remaining: count,
            buffer: VecDeque::new(),
        }
    }

    fn next(&mut self) -> Result<Option<Edge>, Error> {
        if self.buffer.is_empty() {
            if self.remaining == 0 {
                return Ok(None);
            }
            let want_edges = self.remaining.min(CURSOR_BUFFER_EDGES);
            let want_bytes = want_edges * SERIALIZED_EDGE_SIZE;
            let bytes = self
                .spool
                .read_exact(self.next_read_offset, want_bytes)
                .map_err(Error::SpoolIo)?;
            self.next_read_offset += want_bytes as u64;
            self.remaining -= want_edges;
            for chunk in bytes.chunks_exact(SERIALIZED_EDGE_SIZE) {
                let base_offset = u64::from_ne_bytes(chunk[0..8].try_into().unwrap());
                let child_idx = u32::from_ne_bytes(chunk[8..12].try_into().unwrap());
                self.buffer.push_back(Edge {
                    base_offset,
                    child_idx,
                });
            }
        }
        Ok(self.buffer.pop_front())
    }
}

/// Heap entry for the k-way merge. [`Ord`] is reversed so the
/// max-heap yields the SMALLEST `base_offset` first. Ties on
/// `base_offset` are broken by `cursor_idx`, which corresponds to
/// chunk-creation order (earlier chunks ≡ earlier pushes). This
/// preserves push-order among edges with equal `base_offset`, which
/// is what the existing `Tree`'s `children: Vec<u32>` semantics
/// relied on (insertion-order) — the same stability property is
/// what makes the 5.5-b edge stream byte-compatible with today's
/// `.idx` output under step 5.5-c.
struct HeapEdge {
    edge: Edge,
    cursor_idx: usize,
}

impl PartialEq for HeapEdge {
    fn eq(&self, other: &Self) -> bool {
        self.edge == other.edge && self.cursor_idx == other.cursor_idx
    }
}
impl Eq for HeapEdge {}
impl Ord for HeapEdge {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reversed so smaller base_offset pops first from the
        // max-heap. Cursor_idx tie-break is also reversed: smaller
        // cursor_idx (earlier chunk) pops first, preserving
        // insertion order for equal-base_offset edges.
        other
            .edge
            .base_offset
            .cmp(&self.edge.base_offset)
            .then_with(|| other.cursor_idx.cmp(&self.cursor_idx))
    }
}
impl PartialOrd for HeapEdge {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::super::item_store::ItemStoreBuilder;
    use super::*;

    fn build_metadata_items(offsets: &[u64]) -> super::super::item_store::ItemStore {
        let mut b = ItemStoreBuilder::new(offsets.len()).unwrap();
        for &off in offsets {
            b.push(off).unwrap();
        }
        let end = offsets.last().copied().unwrap_or(0) + 100;
        b.finish(std::iter::empty(), end).unwrap()
    }

    #[test]
    fn empty_spool_resolves_empty() {
        let budget = MemoryBudget::unlimited();
        let spool = EdgeSpool::new(0, budget).unwrap();
        let store = build_metadata_items(&[100, 200, 300]);
        let mut resolved = spool.resolve(store.metadata()).unwrap();
        assert!(resolved.next().is_none());
    }

    #[test]
    fn resolve_preserves_parent_idx_sort_order() {
        // 10 items at offsets 100, 200, ..., 1000. Push a random-
        // looking assortment of edges; resolved output must be
        // parent-idx-monotonic regardless of push order.
        let offsets: Vec<u64> = (1..=10u64).map(|i| i * 100).collect();
        let store = build_metadata_items(&offsets);

        // Edges pushed in deliberately scrambled base_offset order
        // so the in-memory chunk sort has actual work to do.
        let pushes: &[(u64, u32)] = &[
            (500, 7), // parent_idx=4, child=7
            (100, 2), // parent_idx=0, child=2
            (800, 9), // parent_idx=7, child=9
            (200, 3), // parent_idx=1, child=3
            (500, 5), // parent_idx=4, child=5 (same parent, second)
            (300, 4), // parent_idx=2, child=4
            (100, 1), // parent_idx=0, child=1 (same parent as first push, but pushed later)
            (700, 8), // parent_idx=6, child=8
        ];
        let budget = MemoryBudget::unlimited();
        let mut spool = EdgeSpool::new(pushes.len(), budget).unwrap();
        for &(bo, ci) in pushes {
            spool.push(bo, ci).unwrap();
        }

        let resolved: Vec<(u32, u32)> = spool
            .resolve(store.metadata())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .expect("all edges resolve");

        assert_eq!(
            resolved.len(),
            pushes.len(),
            "every pushed edge must come out"
        );

        // Parent idx must be non-decreasing.
        for pair in resolved.windows(2) {
            assert!(
                pair[0].0 <= pair[1].0,
                "parent_idx must be non-decreasing: {:?} then {:?}",
                pair[0],
                pair[1],
            );
        }

        // For parent_idx=0 the two children were (100, 2) pushed
        // first and (100, 1) pushed later. Stable sort keeps push
        // order → 2 then 1.
        let parent_0: Vec<u32> = resolved.iter().filter(|&&(p, _)| p == 0).map(|&(_, c)| c).collect();
        assert_eq!(
            parent_0,
            vec![2, 1],
            "equal-base_offset edges must preserve push order"
        );
        // For parent_idx=4, (500, 7) was pushed before (500, 5).
        let parent_4: Vec<u32> = resolved.iter().filter(|&&(p, _)| p == 4).map(|&(_, c)| c).collect();
        assert_eq!(parent_4, vec![7, 5]);
    }

    #[test]
    fn resolve_handles_fanout() {
        // One root (parent_idx=0, offset=100) with 1000 children.
        // All edges share the same base_offset, so the only
        // ordering that matters is push order preservation.
        let offsets: Vec<u64> = std::iter::once(100).chain((1..=1000u64).map(|i| 100 + i * 10)).collect();
        assert_eq!(offsets.len(), 1001);
        let store = build_metadata_items(&offsets);

        let budget = MemoryBudget::unlimited();
        let mut spool = EdgeSpool::new(1000, budget).unwrap();
        for i in 1..=1000u32 {
            spool.push(100, i).unwrap();
        }

        let resolved: Vec<(u32, u32)> = spool
            .resolve(store.metadata())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(resolved.len(), 1000);
        for (i, &(p, c)) in resolved.iter().enumerate() {
            assert_eq!(p, 0, "all children have parent_idx 0 (the root)");
            assert_eq!(c, (i as u32) + 1, "children must emerge in push order");
        }
    }

    #[test]
    fn resolve_handles_chain() {
        // Linear delta chain: item i depends on item i-1.
        // Offsets: 100, 200, 300, ..., 500.
        // Edges: (100, 1), (200, 2), (300, 3), (400, 4).
        // Expected resolved: (0, 1), (1, 2), (2, 3), (3, 4).
        let offsets: Vec<u64> = (1..=5u64).map(|i| i * 100).collect();
        let store = build_metadata_items(&offsets);

        let budget = MemoryBudget::unlimited();
        let mut spool = EdgeSpool::new(4, budget).unwrap();
        // Push in non-trivial order to exercise the sort.
        spool.push(400, 4).unwrap();
        spool.push(100, 1).unwrap();
        spool.push(300, 3).unwrap();
        spool.push(200, 2).unwrap();

        let resolved: Vec<(u32, u32)> = spool
            .resolve(store.metadata())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(resolved, vec![(0, 1), (1, 2), (2, 3), (3, 4)]);
    }

    #[test]
    fn resolve_under_tight_budget_spills_and_still_completes() {
        // Budget = 0 triggers the unaccounted-minimum fallback
        // (MIN_CHUNK_EDGES-sized chunk). Push 5×MIN_CHUNK_EDGES
        // edges in scrambled order so multiple chunks spill and
        // the final resolve goes through the k-way merge path.
        let total = MIN_CHUNK_EDGES * 5;
        let offsets: Vec<u64> = (0..total as u64).map(|i| (i + 1) * 10).collect();
        let store = build_metadata_items(&offsets);

        let budget = MemoryBudget::bytes(0);
        let mut spool = EdgeSpool::new(total, budget).unwrap();

        // Push edges (offset, idx) where each item i has itself as
        // a child of the previous item. Scramble by iterating in
        // reverse order, then interleaving halves.
        //
        // Simpler: push in strict reverse. That guarantees the
        // in-memory chunks are reverse-sorted and the sort step
        // has work.
        for i in (0..total).rev() {
            let off = offsets[i];
            spool.push(off, i as u32).unwrap();
        }

        let resolved: Vec<(u32, u32)> = spool
            .resolve(store.metadata())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(resolved.len(), total);
        // parent_idx is non-decreasing (in fact strictly increasing here
        // because every edge has a unique base_offset).
        for (i, &(p, c)) in resolved.iter().enumerate() {
            assert_eq!(p as usize, i, "strictly monotonic parent_idx");
            assert_eq!(c as usize, i, "child_idx from the original push");
        }
    }

    #[test]
    fn unresolved_base_offset_surfaces_as_error() {
        // Belt-and-suspenders: pushing a base_offset that doesn't
        // appear in metadata must surface as
        // Error::UnresolvedBaseOffset, not a silent misalignment.
        // This is a test-only safety net; the real wiring in
        // step 5.5-c only pushes offsets known to exist.
        let store = build_metadata_items(&[100, 200, 300]);
        let budget = MemoryBudget::unlimited();
        let mut spool = EdgeSpool::new(2, budget).unwrap();
        spool.push(100, 1).unwrap();
        spool.push(250, 2).unwrap(); // 250 is between offsets, not at one
        let results: Vec<Result<(u32, u32), Error>> = spool.resolve(store.metadata()).unwrap().collect();
        assert_eq!(results.len(), 2);
        assert!(matches!(results[0], Ok((0, 1))));
        assert!(matches!(
            results[1],
            Err(Error::UnresolvedBaseOffset { base_offset: 250 })
        ));
    }
}
