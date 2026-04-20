# Step 5.5 — Bounding `cache::delta::Tree<Item<T>>`

Design note. Not yet implemented; this file captures the plan so the
next session can start with context instead of re-deriving it.

## The allocation we're hunting

`gix-pack/src/cache/delta/tree.rs` defines:

```rust
pub struct Item<T> {
    pub offset: Offset,         // u64
    pub next_offset: Offset,    // u64
    pub data: T,
    children: Vec<u32>,
}

pub struct Tree<T> {
    root_items:  Vec<Item<T>>,
    child_items: Vec<Item<T>>,
    last_seen:   Option<NodeKind>,
    future_child_offsets: Vec<(Offset, usize)>,
}
```

With `T = TreeEntry { id: [u8; 20], crc32: u32 }` = 24 bytes at SHA-1:

| Component | Per-object bytes |
| --- | ---: |
| `offset` + `next_offset` | 16 |
| `T` | 24 |
| `Vec<u32>` header (ptr + len + cap) | 24 |
| `Vec<u32>` contents (avg 1 edge/obj at typical delta rates) | ~4 |
| alignment padding | ~4 |
| `Item<T>` total | **~72** |

At 3.34 M objects: ~240 MiB — matches the measured 293 MiB anonymous
RSS at `--budget-mb ∞` within rounding (remaining ~50 MiB is library
ephemera, jemalloc overhead, thread stacks).

In-tree size assertions confirm the shape:
`size_of::<[Item<()>; 7.5M]>() ≈ 300 MB` (40 bytes/Item baseline without `T`)
and `size_of::<[Item<EntryWithDefault>; 7.5M]>() ≈ 840 MB` (112
bytes/Item with a larger `T`). These are already `assert!`d tests in
`tree.rs::tests::size`.

## Design target

Move everything that is **immutable after construction** to mmap'd
disk storage (file-backed RSS — doesn't count against anon):

- `offset`, `next_offset`, and the per-item `(children_start,
  children_len)` tuple go in a flat record array.
- The adjacency (the u32 child indices themselves) goes in a second
  flat packed array.

Keep in RAM only what traversal actually mutates:

- A single `Vec<T>` of length `num_objects`. For SHA-1 pack indexing
  that's 24 bytes × N.

Projected anon for rust-lang/rust (3.34 M objects, SHA-1):

| Site | Bytes | MiB |
| --- | ---: | ---: |
| `Vec<T>` | 24 × 3.34 M | ~80 |
| library ephemera, jemalloc, thread stacks | — | ~50 |
| **target anon total** | | **~130** |

Still above 100 MiB on rust-lang/rust but a 55% reduction over the
current 293 MiB. A follow-up that streams `T` out during traversal
(see "After 5.5" below) would close the remaining gap.

## The consumer contract to preserve

`gix-pack/src/cache/delta/traverse/mod.rs::Tree::traverse`:

1. `self.set_pack_entries_end_and_resolve_ref_offsets(pack_end)` —
   drains `future_child_offsets` into parent `children` Vecs.
2. `self.num_items()` — used to size progress bar.
3. `self.take_root_and_child() -> (Vec<Item<T>>, Vec<Item<T>>)` —
   consumes `self`; roots and children are traversed separately.
4. `root_items` is walked sequentially by `in_parallel_with_slice`,
   one root per work unit.
5. `child_items` is shared via `util::ItemSliceSync<'a, Item<T>>` —
   an `unsafe` wrapper holding `*mut Item<T>` that lets workers
   mutate non-overlapping indices concurrently.
6. Inside `resolve::deltas`, `node.children()` returns `&[u32]` —
   indices into `child_items`, used for recursive descent.
7. Workers mutate `Item<T>::data` (the `T` field) in place.

**Observation that simplifies the rewrite:** the public API exposed
by `resolve::root::Node<'a, T>` is the only surface the traversal
touches `Item` through, and that surface exposes only:

- `offset() -> u64` (read)
- `entry_slice() -> EntryRange` (reads `offset`, `next_offset`)
- `has_children() -> bool` (reads `children`)
- `into_child_iter()` (iterates `children`; calls `ItemSliceSync::get_mut`)
- `data() -> &mut T` (mutate)

A `grep` of `traverse/resolve.rs` for field-level writes confirms
that `offset`, `next_offset`, and `children` are **read-only**
throughout the consumer. Only `T` is written. This means the
representation swap does **not** need to produce a real `&mut
Item<T>`; it can produce `&mut T` plus a separate read-only view
of the immutable fields. That drops the trickiest part of the
sketch below (synthesizing `Item<T>` per access).

## Revised consumer surface

`ItemSliceSync<Item<T>>` is replaced with a pair:

```rust
// Read-only, mmap-backed, share-by-reference across threads.
struct ItemMetadata<'a> {
    items: &'a memmap2::Mmap,  // [ItemRecord; N]
    edges: &'a memmap2::Mmap,  // [u32; total_edges]
}

impl ItemMetadata<'_> {
    fn offset(&self, idx: usize) -> u64 { ... }
    fn next_offset(&self, idx: usize) -> u64 { ... }
    fn children(&self, idx: usize) -> &[u32] { ... }
}

// Writable, in-RAM, same *mut T pattern as today.
struct DataSliceSync<'a, T: Send> {
    data: *mut T,
    #[cfg(debug_assertions)] len: usize,
    phantom: PhantomData<&'a mut T>,
}

impl<'a, T: Send> DataSliceSync<'a, T> {
    unsafe fn get_mut(&self, idx: usize) -> &'a mut T { ... }
}

// Replacement for resolve::root::Node<'a, T>.
struct Node<'a, T: Send> {
    idx: usize,
    metadata: &'a ItemMetadata<'a>,
    data: DataSliceSync<'a, T>,
}
```

`Node`'s public methods map one-for-one to today's:
`offset()` -> `metadata.offset(idx)`, `entry_slice()` -> from
`(offset, next_offset)`, `data()` -> `unsafe {
data.get_mut(idx) }`, etc. No `Item<T>` struct is synthesized.

## Representation

Two new modules inside `gix-pack/src/cache/delta/`:

```rust
// gix-pack/src/cache/delta/item_store.rs
struct ItemRecord {
    offset:         u64,
    next_offset:    u64,
    children_start: u32,  // index into edges[]
    children_len:   u32,  // count
}  // 24 bytes, #[repr(C)], little-endian on wire

pub(super) struct ItemStore<T: Send> {
    // Mmap-backed, read-only after finish(); file is a tempfile
    // opened via `gix_tempfile` so it unlinks on drop / crash.
    items:  memmap2::Mmap,        // [ItemRecord; num_items]
    edges:  memmap2::Mmap,        // [u32; total_edges]
    // In-RAM mutable data, one per item. Uses UnsafeCell under
    // the hood for DataSliceSync to hand out disjoint &mut T
    // concurrently without violating Rust's aliasing rules.
    data:   Box<[UnsafeCell<T>]>, // length == num_items
    num_roots: usize,             // roots are indices 0..num_roots
}

impl<T: Send> ItemStore<T> {
    pub fn metadata(&self) -> ItemMetadata<'_> { ... }
    pub fn data_slice_sync(&mut self) -> DataSliceSync<'_, T> { ... }
    pub fn num_items(&self) -> usize { ... }
    pub fn num_roots(&self) -> usize { ... }
}
```

```rust
// gix-pack/src/cache/delta/edge_spool.rs
// Append-only writer collecting (base_offset, child_idx) pairs
// during construction. Sorted by base_offset at finish() time via
// external sort (reusing ExternalSorter infrastructure from step
// 5.4b-1, parameterized over a new record type). Output is a
// sorted iterator used to materialize the final edges mmap +
// children_start/len per item.
struct EdgeSpool { ... }
```

## Construction path

The existing `Tree<T>::add_root` and `Tree<T>::add_child` become
thin wrappers that:

1. Append an `ItemRecord` with `children_start = 0, children_len = 0`
   to an append-only writer for the items file.
2. Append `T` to an in-RAM `Vec<T>`.
3. For `add_child`, append `(base_offset, new_idx)` to the edge
   spool.
4. For `add_root`, nothing extra — roots have no parent edge.

At `finish_construction()` (replacing
`set_pack_entries_end_and_resolve_ref_offsets`):

1. Seal the items writer; mmap it read-only.
2. External-sort the edge spool by `base_offset`.
3. For each parent offset in the sorted edge iterator:
   - Binary-search the items mmap for that offset (items are already
     sorted by offset thanks to the existing `assert_is_incrementing`
     invariant — no explicit sort needed).
   - Accumulate consecutive edges with the same base_offset.
   - Emit a contiguous run of `u32` child indices to the edges
     writer.
   - Back-patch the parent's `ItemRecord.children_start/len` via a
     second writer pass — OR collect `(parent_idx, edge_offset,
     count)` tuples and do one final pass over the items file to
     patch in place. The second approach avoids two item writers.

The `future_child_offsets` queue disappears — it was there to defer
lookups during single-pass construction; our sorted-edges approach
defers *all* lookups uniformly.

## Consumption path

`take_root_and_child()` is replaced by `take_metadata_and_data()`
returning the pair `(ItemMetadata, DataSliceSync<T>)` described in
the "Revised consumer surface" section above. The traversal code is
rewritten to use `Node<'a, T>` (backed by metadata + data) instead
of `Node<'a, T>` backed by `&mut Item<T>`. Method signatures and
behaviour on `Node` stay identical; only the internals change.

Roots are iterated sequentially by index from 0..num_roots by
`in_parallel_with_slice`. The per-worker state just needs to know
its root index and hold a clone of the shared `ItemMetadata` +
`DataSliceSync`.

No separate allocation analogous to today's `Vec<Item<T>>` for
roots: roots share the same `ItemRecord` mmap as children, just at
a different index range. Building `ItemStore` that way keeps the
storage linear and avoids doubled accounting.

## Commit plan

1. **5.5-a** — `item_store` module in isolation. `ItemStore<T>` type,
   unit tests covering: roundtrip insert + iterate, mmap lifecycle,
   children_start/len patching, concurrent `data_mut` on disjoint
   indices. No wiring. Analogous to step 5.4b-1 (`ExternalSorter` in
   isolation before wiring).

2. **5.5-b** — Edge spool + external-sort-based children
   construction. Covered by unit tests that verify: for a synthetic
   pack of N items with a known delta graph, the final
   `children_start/len` per item matches the existing in-RAM
   `Tree`'s `children` Vec byte-for-byte. No wiring.

3. **5.5-c** — Swap `Tree<T>` internals to `ItemStore<T>` +
   `EdgeSpool`. Preserve the `add_root` / `add_child` /
   `assert_is_incrementing` / `num_items` / `take_root_and_child`
   API. Tests to keep green: `gix-pack` lib 32/32 (includes the
   `size_of_pack_tree_item` assertions — those are expected to
   change and need updating to the new sizes), `gix-pack-tests`
   50/50.

4. **5.5-d** — Swap `resolve::root::Node<'a, T>` and the
   `in_parallel_with_slice` call in `traverse/mod.rs` to use
   `ItemMetadata` + `DataSliceSync` instead of `&mut Item<T>` +
   `ItemSliceSync<Item<T>>`. Delete `ItemSliceSync` (or keep as
   deprecated alias if any external crate depends on it; `grep` the
   workspace). Tests: the traverse byte-for-byte roundtrip in
   `gix-pack-tests` is the load-bearing check.

5. **5.5-e** — Remove the in-memory code path if step 5.5-d proved
   stable. Ship.

Each commit is independently testable and revert-able. If 5.5-c or
5.5-d prove harder than sketched, 5.5-a and 5.5-b stand alone and
can be used for future work.

## After 5.5 — streaming T

The 5.5 design still holds `Vec<T>` = 80 MiB in anon for
rust-lang/rust. A follow-up step would change the traversal contract
so callers provide a `FnMut(TraverseIndex, Context<'_>) -> Result<T,
E>` sink instead of mutating `Item<T>::data` in place. For pack
indexing, the sink would be `ExternalSorter::push` (from step 5.4b)
— each resolved object's `(id, crc32, offset)` tuple streams
directly into the external sorter and never occupies `Vec<T>`. This
collapses traversal + sort into a fused pipeline and drops anon
toward the library-floor ~50 MiB, below the 100 MiB target for any
repo size.

This is a bigger API change than 5.5 proper and belongs in its own
multi-commit arc. Call it step 5.6 when the time comes.

## Risks

1. **`ItemSliceSync` invariant.** The existing `unsafe` block in
   `traverse/mod.rs` relies on the fact that each worker's subtree
   touches disjoint child indices. The disk-backed rewrite must
   preserve this — same indexing semantics, same non-overlap
   invariant. Any change to how children are partitioned is a
   correctness hazard.
2. **Mmap + tempfile lifetime on crash.** If the process panics
   mid-traversal, the mmap'd items/edges tempfiles must unlink
   cleanly (`gix_tempfile`'s registry handles this). Test with a
   `should_interrupt`-induced abort.
3. **Byte-identity of produced `.idx`.** `gix-pack-tests` compares
   produced index bytes to canned fixtures. Any accidental
   reordering inside the children construction (edge sort must be
   stable by `(base_offset, child_idx)`, same as the current
   binary-search insertion order) will fail this test. Good — use
   that test as the primary load-bearing check.
4. **Performance regression.** Mmap read + indirection per
   `Item::children()` call vs direct Vec access. Expected overhead
   is minimal since the items/edges pages are hot, but measure with
   `elapsed_s` on the harness after each commit. A > 20% regression
   is a stop-ship.

## Resume pointers

- Current Tree code: `gix-pack/src/cache/delta/tree.rs`,
  `cache/delta/from_offsets.rs`, `cache/delta/traverse/mod.rs`,
  `cache/delta/traverse/resolve.rs`, `cache/delta/traverse/util.rs`.
- Reusable spool: `cache/delta/traverse/spool.rs` (SpoolHandle,
  SpoolFile) — step 5.3 infrastructure. May or may not be
  directly reusable for the items/edges writers; evaluate when
  writing 5.5-a.
- Reusable external sort: `index/write/external_sort.rs` — step
  5.4b-1. Parameterize over the edge record type (`(u64, u32)` or
  a wider record if alignment forces) for step 5.5-b.
- Harness: `mem-harness/src/main.rs`. After each 5.5 commit, re-run
  against rust-lang/rust at `--budget-mb 1` and append the new row
  to `MEASUREMENTS.md`.
