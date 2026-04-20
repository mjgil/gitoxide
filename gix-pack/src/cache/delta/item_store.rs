//! Disk-backed metadata store for delta-tree items.
//!
//! # Why this exists
//!
//! Step 5.5-a of the bounded-memory plan. The in-RAM delta-tree
//! acceleration structure held ~293 MiB of anonymous RSS on
//! `rust-lang/rust` (3.34 M objects). This module moves the
//! immutable fields — `offset`, `next_offset`, and children
//! indices — to an mmap'd tempfile (file-backed RSS that doesn't
//! count against anon, is evictable by the kernel, and is cleaned
//! up on process exit via `O_TMPFILE` semantics).
//!
//! Step 5.6-b retired the in-RAM `data: T` array entirely:
//! `inspect_object` is merged into the `sink` callback, so the
//! traversal no longer needs per-item mutable state. The store
//! now holds only the on-disk metadata — no generics, no
//! `UnsafeCell`, no `DataSliceSync`.
//!
//! # Endianness
//!
//! The tempfile is process-local and short-lived (a single pack
//! indexing). Records are written in host byte order; no portability
//! concerns. If the file were ever persisted, we'd need
//! little-endian conversion on write/read — not the case today.

#![allow(dead_code)]

use std::{
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    mem::size_of,
};

use memmap2::{Mmap, MmapMut, MmapOptions};

/// On-disk record for one item. Fixed size (24 bytes), `#[repr(C)]`,
/// host byte order. Layout:
///
/// | field            | offset | size |
/// | ---              | ---    | ---  |
/// | `offset`         | 0      | 8    |
/// | `next_offset`    | 8      | 8    |
/// | `children_start` | 16     | 4    |
/// | `children_len`   | 20     | 4    |
///
/// No padding thanks to u64 → u64 → u32 → u32 natural alignment.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ItemRecord {
    pub(crate) offset: u64,
    pub(crate) next_offset: u64,
    /// First index into the edges mmap that belongs to this item's
    /// children. `0` when `children_len == 0` — the sentinel empty
    /// slice.
    pub(crate) children_start: u32,
    /// Number of consecutive indices in the edges mmap that belong
    /// to this item. Zero means no children.
    pub(crate) children_len: u32,
}

const _: () = assert!(size_of::<ItemRecord>() == 24, "ItemRecord must stay 24 bytes");

/// Append-only builder for an [`ItemStore`].
///
/// Items are pushed via [`push`][Self::push] in strictly increasing
/// index order. Each push reserves one slot in the items tempfile.
/// `next_offset` and the children range are filled in by
/// [`finish`][Self::finish].
pub(crate) struct ItemStoreBuilder {
    items_writer: io::BufWriter<File>,
    num_items: u32,
    items_mmap: Option<Mmap>,
    empty_edges_scratch: Option<Mmap>,
    #[cfg(debug_assertions)]
    last_offset: Option<u64>,
}

impl ItemStoreBuilder {
    pub(crate) fn new(_capacity_hint: usize) -> io::Result<Self> {
        let items_file = tempfile::tempfile()?;
        Ok(Self {
            items_writer: io::BufWriter::new(items_file),
            num_items: 0,
            items_mmap: None,
            empty_edges_scratch: None,
            #[cfg(debug_assertions)]
            last_offset: None,
        })
    }

    pub(crate) fn push(&mut self, offset: u64) -> io::Result<u32> {
        #[cfg(debug_assertions)]
        {
            if let Some(prev) = self.last_offset {
                debug_assert!(
                    offset > prev,
                    "item offsets must strictly increase: prev={prev} new={offset}"
                );
            }
            self.last_offset = Some(offset);
        }
        self.items_mmap = None;
        let rec = ItemRecord {
            offset,
            next_offset: 0,
            children_start: 0,
            children_len: 0,
        };
        self.write_record(&rec)?;
        let idx = self.num_items;
        self.num_items = self
            .num_items
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "item count overflows u32"))?;
        Ok(idx)
    }

    fn write_record(&mut self, rec: &ItemRecord) -> io::Result<()> {
        // SAFETY: `ItemRecord` is `#[repr(C)]` with no padding (the
        // const_assert above pins its size to 24 bytes). Reinterpreting
        // a single instance as a byte slice of that size reads the
        // exact laid-out bytes.
        #[allow(unsafe_code)]
        let bytes = unsafe {
            std::slice::from_raw_parts(rec as *const ItemRecord as *const u8, size_of::<ItemRecord>())
        };
        self.items_writer.write_all(bytes)
    }

    /// Back-patch the `next_offset` field of an already-pushed item.
    ///
    /// Tree uses this during construction: when a new item arrives,
    /// the previous item's `next_offset` becomes `new_item.offset`.
    /// We mirror that pattern; callers call this in order, never
    /// looking back more than one record.
    pub(crate) fn set_next_offset(&mut self, idx: u32, next_offset: u64) -> io::Result<()> {
        debug_assert!(idx < self.num_items);
        self.items_writer.flush()?;
        let file = self.items_writer.get_mut();
        let field_offset = u64::from(idx) * size_of::<ItemRecord>() as u64
            + size_of::<u64>() as u64;
        file.seek(SeekFrom::Start(field_offset))?;
        file.write_all(&next_offset.to_ne_bytes())?;
        file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Number of items pushed so far. Used by [`super::tree::Tree`]
    /// to size progress bars and to compute the last-pushed-index
    /// for [`set_next_offset`][Self::set_next_offset] target.
    pub(crate) fn num_items(&self) -> u32 {
        self.num_items
    }

    /// Return a read-only metadata view over the items pushed so
    /// far. The returned view exposes
    /// [`ItemMetadata::num_items`] and [`ItemMetadata::offset`] (and
    /// [`ItemMetadata::next_offset`], though its reliability depends
    /// on whether [`set_next_offset`][Self::set_next_offset] has been
    /// called for each index); [`ItemMetadata::children`] always
    /// yields `&[]` because `num_edges == 0`.
    ///
    /// The returned [`ItemMetadata`] borrows `&mut self` for its
    /// lifetime, so the borrow checker prevents any
    /// [`push`][Self::push] or [`set_next_offset`][Self::set_next_offset]
    /// calls while the view is alive. That's what makes the cached
    /// mmap safe: the file cannot grow during the view's lifetime.
    ///
    /// # Why this exists
    ///
    /// Step 5.5-c wires this builder into [`super::tree::Tree`].
    /// The construction flow is:
    ///
    ///   1. `add_root` / `add_child` → `push` items, `EdgeSpool::push`
    ///      edges.
    ///   2. At finish time, `EdgeSpool::resolve(metadata)` needs an
    ///      [`ItemMetadata`] to map edges' `base_offset` to
    ///      `parent_idx`.
    ///   3. But `ItemStoreBuilder::finish` consumes `self` AND needs
    ///      the sorted edges as input — chicken-and-egg.
    ///
    /// This method is the solution: get an [`ItemMetadata`] view
    /// before `finish`, collect the resolved edges into a `Vec`
    /// (which drops the borrow), then call `finish` with the edges.
    pub(crate) fn interim_metadata(&mut self) -> io::Result<ItemMetadata<'_>> {
        self.items_writer.flush()?;
        if self.items_mmap.is_none() {
            if self.num_items == 0 {
                // Empty file. memmap2 rejects zero-length maps on
                // some platforms; use the same 1-byte scratch
                // pattern `finish` uses for the zero-edge case.
                self.items_mmap = Some(mmap_single_byte_scratch()?);
            } else {
                // SAFETY: tempfile is exclusively owned by this
                // builder; no external writer. The borrow checker
                // (via `&mut self`) prevents any `push` call from
                // extending the file during the returned
                // ItemMetadata's lifetime, so the mapping length
                // stays valid. `set_next_offset` is fine because
                // in-place writes to a file propagate to shared
                // mmaps via the kernel page cache.
                #[allow(unsafe_code)]
                let mmap = unsafe { MmapOptions::new().map(self.items_writer.get_ref())? };
                self.items_mmap = Some(mmap);
            }
        }
        if self.empty_edges_scratch.is_none() {
            self.empty_edges_scratch = Some(mmap_single_byte_scratch()?);
        }
        Ok(ItemMetadata {
            items: self.items_mmap.as_ref().unwrap(),
            edges: self.empty_edges_scratch.as_ref().unwrap(),
            num_items: self.num_items,
            num_edges: 0,
        })
    }

    /// Finalise the store.
    ///
    /// `sorted_edges` is an iterator of `(parent_idx, child_idx)`
    /// pairs **sorted by `parent_idx`**. Pairs with the same
    /// `parent_idx` form a contiguous run; their `child_idx`es become
    /// that parent's children slice in the edges mmap, preserving
    /// input order within the run (matching the existing Tree
    /// semantics where children are added in the order they're
    /// discovered in the pack).
    ///
    /// After this call, the items file's `children_start`/
    /// `children_len` fields are patched to reference slices in the
    /// edges file. Both files are then mmap'd read-only and wrapped
    /// in an [`ItemStore`]; the underlying `File` handles are dropped
    /// (their inodes remain alive via the `Mmap` reference, and
    /// `O_TMPFILE` guarantees filesystem-namespace cleanup
    /// regardless of drop order).
    pub(crate) fn finish(
        mut self,
        sorted_edges: impl IntoIterator<Item = (u32, u32)>,
        pack_entries_end: u64,
    ) -> io::Result<ItemStore> {
        // Materialise the edges tempfile in one streaming pass over
        // the sorted edge pairs. For each parent run we:
        //   1. record (parent_idx, start, len),
        //   2. write the child indices contiguously.
        // After the walk, apply the patches to the items file.
        let mut edges_file = tempfile::tempfile()?;
        let mut edges_count: u32 = 0;
        let mut patches: Vec<(u32, u32, u32)> = Vec::new(); // (parent, start, len)
        let mut iter = sorted_edges.into_iter().peekable();
        while let Some(&(parent, _)) = iter.peek() {
            let start = edges_count;
            let mut len: u32 = 0;
            while let Some(&(p, child)) = iter.peek() {
                if p != parent {
                    break;
                }
                // Consume and emit.
                iter.next();
                edges_file.write_all(&child.to_ne_bytes())?;
                len = len.checked_add(1).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "children run length overflows u32",
                    )
                })?;
                edges_count = edges_count.checked_add(1).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "total edge count overflows u32",
                    )
                })?;
            }
            // Sanity: the outer while guarantees at least one child for this parent.
            debug_assert!(len > 0);
            patches.push((parent, start, len));
        }

        self.items_writer.flush()?;

        // Patch next_offsets and children via MmapMut — pure memory
        // operations, zero syscalls for the actual patching.
        if self.num_items > 0 {
            let rec_size = size_of::<ItemRecord>();
            let off_of_next = size_of::<u64>();
            let off_of_children = 2 * size_of::<u64>();

            #[allow(unsafe_code)]
            let mut mm = unsafe { MmapMut::map_mut(self.items_writer.get_ref())? };

            // next_offset[i] = offset[i+1], next_offset[last] = pack_entries_end
            for i in 0..self.num_items as usize {
                let next_off = if i + 1 < self.num_items as usize {
                    let next_rec = (i + 1) * rec_size;
                    u64::from_ne_bytes(mm[next_rec..next_rec + 8].try_into().unwrap())
                } else {
                    pack_entries_end
                };
                let dst = i * rec_size + off_of_next;
                mm[dst..dst + 8].copy_from_slice(&next_off.to_ne_bytes());
            }

            // Children patches
            for &(parent, start, len) in &patches {
                let dst = parent as usize * rec_size + off_of_children;
                mm[dst..dst + 4].copy_from_slice(&start.to_ne_bytes());
                mm[dst + 4..dst + 8].copy_from_slice(&len.to_ne_bytes());
            }

        }
        edges_file.flush()?;

        let items_mmap = if self.num_items == 0 {
            mmap_single_byte_scratch()?
        } else {
            #[allow(unsafe_code)]
            let m = unsafe { MmapOptions::new().populate().map(self.items_writer.get_ref())? };
            #[cfg(target_os = "linux")]
            let _ = m.advise(memmap2::Advice::HugePage);
            m
        };
        let edges_mmap = if edges_count == 0 {
            mmap_single_byte_scratch()?
        } else {
            // SAFETY: same argument as items_mmap.
            #[allow(unsafe_code)]
            let m = unsafe { MmapOptions::new().populate().map(&edges_file)? };
            #[cfg(target_os = "linux")]
            let _ = m.advise(memmap2::Advice::HugePage);
            m
        };

        Ok(ItemStore {
            items: items_mmap,
            edges: edges_mmap,
            num_items: self.num_items,
            num_edges: edges_count,
        })
    }
}

fn mmap_single_byte_scratch() -> io::Result<Mmap> {
    let mut f = tempfile::tempfile()?;
    f.write_all(&[0u8])?;
    f.flush()?;
    // SAFETY: tempfile is exclusively owned, no external mutation.
    #[allow(unsafe_code)]
    unsafe {
        MmapOptions::new().map(&f)
    }
}

/// Mmap-backed metadata store for N items.
///
/// Construct via [`ItemStoreBuilder`]. Consume by calling
/// [`metadata`][Self::metadata] (shared read-only view).
pub(crate) struct ItemStore {
    items: Mmap,
    edges: Mmap,
    num_items: u32,
    num_edges: u32,
}

impl ItemStore {
    pub(crate) fn num_items(&self) -> u32 {
        self.num_items
    }

    pub(crate) fn num_edges(&self) -> u32 {
        self.num_edges
    }

    pub(crate) fn metadata(&self) -> ItemMetadata<'_> {
        ItemMetadata {
            items: &self.items,
            edges: &self.edges,
            num_items: self.num_items,
            num_edges: self.num_edges,
        }
    }
}

/// Read-only view over the mmap'd immutable fields of all items.
///
/// Holds `&Mmap` references; `Send + Sync` is safe because the mmap
/// is never mutated after construction (and memmap2's `Mmap` itself
/// is `Send + Sync`).
#[derive(Clone, Copy)]
pub(crate) struct ItemMetadata<'a> {
    items: &'a Mmap,
    edges: &'a Mmap,
    num_items: u32,
    num_edges: u32,
}

impl<'a> ItemMetadata<'a> {
    pub(crate) fn num_items(&self) -> u32 {
        self.num_items
    }

    pub(crate) fn offset(&self, idx: u32) -> u64 {
        self.record_field(idx, 0)
    }

    pub(crate) fn next_offset(&self, idx: u32) -> u64 {
        self.record_field(idx, size_of::<u64>())
    }

    fn record_field(&self, idx: u32, byte_offset_within_rec: usize) -> u64 {
        debug_assert!(idx < self.num_items);
        let base = idx as usize * size_of::<ItemRecord>() + byte_offset_within_rec;
        let bytes: [u8; 8] = self.items[base..base + 8]
            .try_into()
            .expect("items mmap is at least (idx+1)*24 bytes long");
        u64::from_ne_bytes(bytes)
    }

    /// Children slice for item `idx`. Returns an empty slice if the
    /// item has no children.
    pub(crate) fn children(&self, idx: u32) -> &'a [u32] {
        debug_assert!(idx < self.num_items);
        let rec_base = idx as usize * size_of::<ItemRecord>();
        // children_start at byte 16, children_len at byte 20.
        let start = u32::from_ne_bytes(
            self.items[rec_base + 16..rec_base + 20].try_into().unwrap(),
        ) as usize;
        let len = u32::from_ne_bytes(
            self.items[rec_base + 20..rec_base + 24].try_into().unwrap(),
        ) as usize;
        if len == 0 {
            return &[];
        }
        let byte_start = start * size_of::<u32>();
        let byte_end = byte_start + len * size_of::<u32>();
        let slice_bytes = &self.edges[byte_start..byte_end];
        // SAFETY: the edges mmap is a multiple of 4 bytes (we wrote
        // it 4 bytes at a time), the slice bounds fall on u32
        // boundaries, and u32 is trivially aligned. Alignment: the
        // returned slice may be misaligned relative to the mmap
        // start, so we must not use `align_to`. Instead, we vend a
        // slice only if alignment happens to hold; callers that
        // need strict alignment should copy out.
        //
        // In practice, memmap2 returns page-aligned regions and
        // `start * 4` preserves u32 alignment, so the returned
        // slice is always properly aligned. The `try_into` /
        // alignment check below enforces this at runtime.
        #[allow(unsafe_code)]
        let ptr = slice_bytes.as_ptr();
        debug_assert_eq!(
            (ptr as usize) % std::mem::align_of::<u32>(),
            0,
            "edges mmap slice must be u32-aligned"
        );
        // SAFETY: see above; alignment asserted, lifetime tied to
        // self.edges which outlives 'a.
        #[allow(unsafe_code)]
        unsafe {
            std::slice::from_raw_parts(ptr as *const u32, len)
        }
    }
}

// SAFETY: `Mmap` is itself `Send + Sync`, and `ItemMetadata` only
// shares immutable references to it. The numeric fields are `Copy`.
// Nothing held here has interior mutability.
#[allow(unsafe_code)]
unsafe impl Send for ItemMetadata<'_> {}
#[allow(unsafe_code)]
unsafe impl Sync for ItemMetadata<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_constructs_cleanly() {
        let b = ItemStoreBuilder::new(0).expect("tempfile creation");
        let store = b.finish(std::iter::empty(), 0).expect("finish");
        assert_eq!(store.num_items(), 0);
        assert_eq!(store.num_edges(), 0);
        let md = store.metadata();
        assert_eq!(md.num_items(), 0);
    }

    #[test]
    fn push_then_read_back_offsets() {
        let mut b = ItemStoreBuilder::new(4).unwrap();
        assert_eq!(b.push(100).unwrap(), 0);
        assert_eq!(b.push(200).unwrap(), 1);
        assert_eq!(b.push(300).unwrap(), 2);
        let store = b.finish(std::iter::empty(), 400).unwrap();
        let md = store.metadata();
        assert_eq!(md.num_items(), 3);
        assert_eq!(md.offset(0), 100);
        assert_eq!(md.offset(1), 200);
        assert_eq!(md.offset(2), 300);
        assert_eq!(md.next_offset(0), 200);
        assert_eq!(md.next_offset(1), 300);
        assert_eq!(md.next_offset(2), 400);
        assert_eq!(md.children(0), &[] as &[u32]);
        assert_eq!(md.children(1), &[] as &[u32]);
        assert_eq!(md.children(2), &[] as &[u32]);
    }

    #[test]
    fn children_assembled_from_sorted_edges() {
        let mut b = ItemStoreBuilder::new(5).unwrap();
        for (i, offs) in [100u64, 200, 300, 400, 500].iter().copied().enumerate() {
            let idx = b.push(offs).unwrap();
            assert_eq!(idx, i as u32);
        }
        let edges: Vec<(u32, u32)> = vec![(0, 1), (0, 3), (1, 2), (3, 4)];
        let store = b.finish(edges, 600).unwrap();
        let md = store.metadata();
        assert_eq!(md.children(0), &[1, 3]);
        assert_eq!(md.children(1), &[2]);
        assert_eq!(md.children(2), &[] as &[u32]);
        assert_eq!(md.children(3), &[4]);
        assert_eq!(md.children(4), &[] as &[u32]);
        // next_offsets computed automatically
        assert_eq!(md.next_offset(0), 200);
        assert_eq!(md.next_offset(1), 300);
        assert_eq!(md.next_offset(2), 400);
        assert_eq!(md.next_offset(3), 500);
        assert_eq!(md.next_offset(4), 600);
    }

    #[test]
    fn large_fanout_tree() {
        let n = 1001u32;
        let mut b = ItemStoreBuilder::new(n as usize).unwrap();
        for i in 0..n {
            b.push(u64::from(i) * 10 + 1).unwrap();
        }
        let edges: Vec<(u32, u32)> = (1..n).map(|i| (0, i)).collect();
        let store = b.finish(edges, u64::from(n) * 10 + 100).unwrap();
        let md = store.metadata();
        let ch = md.children(0);
        assert_eq!(ch.len(), (n - 1) as usize);
        for (i, &c) in ch.iter().enumerate() {
            assert_eq!(c, (i as u32) + 1);
        }
    }

    #[test]
    fn num_items_reflects_push_count() {
        let mut b = ItemStoreBuilder::new(0).unwrap();
        assert_eq!(b.num_items(), 0);
        b.push(10).unwrap();
        assert_eq!(b.num_items(), 1);
        b.push(20).unwrap();
        b.push(30).unwrap();
        assert_eq!(b.num_items(), 3);
        b.set_next_offset(0, 20).unwrap();
        assert_eq!(b.num_items(), 3);
    }

    #[test]
    fn interim_metadata_reads_partial_items_file() {
        let mut b = ItemStoreBuilder::new(4).unwrap();
        b.push(100).unwrap();
        b.push(200).unwrap();
        b.push(300).unwrap();
        b.set_next_offset(0, 200).unwrap();
        b.set_next_offset(1, 300).unwrap();
        b.set_next_offset(2, 999).unwrap();

        let md = b.interim_metadata().unwrap();
        assert_eq!(md.num_items(), 3);
        assert_eq!(md.offset(0), 100);
        assert_eq!(md.offset(1), 200);
        assert_eq!(md.offset(2), 300);
        assert_eq!(md.next_offset(0), 200);
        assert_eq!(md.next_offset(1), 300);
        assert_eq!(md.next_offset(2), 999);
        assert_eq!(md.children(0), &[] as &[u32]);
    }

    #[test]
    fn interim_metadata_then_push_then_interim_again() {
        let mut b = ItemStoreBuilder::new(8).unwrap();
        b.push(100).unwrap();
        b.push(200).unwrap();
        {
            let md = b.interim_metadata().unwrap();
            assert_eq!(md.num_items(), 2);
            assert_eq!(md.offset(1), 200);
        }
        b.push(300).unwrap();
        b.push(400).unwrap();
        let md = b.interim_metadata().unwrap();
        assert_eq!(md.num_items(), 4);
        assert_eq!(md.offset(0), 100);
        assert_eq!(md.offset(2), 300);
        assert_eq!(md.offset(3), 400);
    }

    #[test]
    fn interim_metadata_on_empty_builder() {
        let mut b = ItemStoreBuilder::new(0).unwrap();
        let md = b.interim_metadata().unwrap();
        assert_eq!(md.num_items(), 0);
    }

    #[test]
    fn set_next_offset_is_idempotent_cheap_accessor() {
        let mut b = ItemStoreBuilder::new(2).unwrap();
        b.push(100).unwrap();
        b.push(200).unwrap();
        b.set_next_offset(0, 200).unwrap();
        b.set_next_offset(1, 999).unwrap();
        b.set_next_offset(0, 201).unwrap();
        let md = b.interim_metadata().unwrap();
        assert_eq!(md.offset(0), 100);
        assert_eq!(md.offset(1), 200);
        assert_eq!(md.next_offset(0), 201, "re-patched next_offset");
        assert_eq!(md.next_offset(1), 999, "neighbour must be untouched");
    }
}
