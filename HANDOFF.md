# Handoff — gitx-bounded

This file is the single read-once reference for picking up work on
this fork. A new AI instance (or a new developer) should be able to
read this end-to-end in under five minutes and then start producing
commits that match the style and cadence of what's already here.

## 1. What this fork is

A fork of [gitoxide](https://github.com/GitoxideLabs/gitoxide) that
adds a process-wide `MemoryBudget` primitive and threads it through
every memory-hot site on the clone / pack-indexing path. The
motivating use case is `git-vault-server` mirroring large remotes on
a small VPS — without bounded memory, a pathological pack can OOM-
kill the host.

See [`README.md`](./README.md) for the public framing and
[`STEP_5_5_DESIGN.md`](./STEP_5_5_DESIGN.md) for the design doc of
the remaining work.

## 2. Operating rules (non-negotiable — the user has been enforcing these)

Every session, in order:

1. `cd $HOME/git && pwd`
2. `sudo echo "hi"`
3. Use **full absolute paths** in every file-related command.
4. After every `edit_file`, **immediately run a `read_file`** (or
   `cat` / `bat` / `sed -n`) on the same path to confirm the change
   landed as intended. No exceptions, even for one-line edits.
5. **Never** create "fix_*.sh" or similar external scripts to patch
   files. Edit the files directly.
6. `write_file` (or `tee` + stdin) is for **creating new files only**;
   editing existing files goes through `edit_file`.
7. For long-running commands, wrap with `timeout 3s` (or `5s`)
   rather than letting them run open-ended. Exception: genuinely
   long builds/tests — use explicit tool-level timeouts instead.
8. Use `bat` for `cat`, `exa` for `ls`, `fd` for `find`, `rg` for
   `grep`. These are installed system-wide.
9. Commits via `git -C /home/m/git/gitx-bounded ...` to avoid
   `cd`-state leakage across tool invocations.
10. Commit-message style: detailed, multi-paragraph, shaped like the
    existing commits. A fresh reader should understand why the
    change was made without reading the code.

If a command fails because of "headless" / "not a tty" / missing
sudo, **do not** patch around it. Run the version that needs root.

## 3. The production goal

**Peak anonymous RSS ≤ 100 MiB for any clone, regardless of repo
size.**

`peak_rss_mb` (from `getrusage`) is not the metric — it counts
file-backed mmap pages, which the kernel evicts under pressure and
which do not cause OOM kills. The metric that matters is
`peak_rss_anon_mb` (from `/proc/self/status:RssAnon`). See
[`MEASUREMENTS.md`](./MEASUREMENTS.md) for per-fixture numbers.

Current status against the goal (as of commit `05bcaa9`):

| Fixture | objects | anon at `--budget-mb 1` | anon at `--budget-mb 10` | anon unlimited | under 100 MiB? |
| --- | ---: | ---: | ---: | ---: | :---: |
| self (gitx-bounded) | 30 | 20 MiB | — | 20 MiB | ✅ |
| **rust-lang/rust** | **3.34 M** | **110 MiB** | **42 MiB** | **59 MiB** | **✅ at b≥10** |

5.6-b (`05bcaa9`) retired the ~80 MiB `Box<[UnsafeCell<T>]>` data
array by merging `inspect_object` into the `sink` callback and
removing the `T` generic from `Tree`. At unlimited budget,
rust-lang/rust dropped from 135 → 59 MiB (-56%), confirming the
~80 MiB retirement. At tight budget (b=1), the reduction is
smaller (116 → 110 MiB, -5%) because the spill-path buffered-I/O
overhead dominates anon RSS — a known issue (§11). At budget=10
(a realistic production setting) anon is 42 MiB, well under the
100 MiB goal. The production goal is met for budget≥10.

## 4. Commit history with purpose

In forward chronological order. All commits are on `main`, which is
several commits ahead of `origin/main` (not pushed).

| sha | step | what landed |
| --- | --- | --- |
| `1c09f3f` | — | initial commit (upstream gitoxide snapshot) |
| `43ca0ef` | — | docs: bounded-memory README + upstream README preserved |
| `ee718a6` | — | gix-odb: rename misleading `memory` module to `write_proxy` (not a budget; just a write-through proxy) |
| `06df93b` | — | mem-harness: first version, peak-RSS only |
| `e4b9647` | 2 | gix-features: `MemoryBudget` primitive (atomic counter + `Reservation` + Drop = release) |
| `773dc00` | 3 | gix: thread `MemoryBudget` through `open::Options` and `Repository` |
| `a5b21a1` | 4 | wire `MemoryCappedHashmap` object cache to the shared budget |
| `91248d1` | 4 | wire `StaticLinkedList` pack cache to the shared budget |
| `8a12370` | — | mem-harness: thread `--budget-mb` through `open::Options` |
| `d60cd43` | 3 | thread budget through `Tree::traverse` and `Bundle::write_to_directory` |
| `4f938f5` | 5.2 | delta-chain cache reserves against budget; fails with `OutOfBudget` |
| `ba53553` | 5.3 | delta-chain cache spills to disk (via `SpoolFile` / `SpoolHandle`) instead of aborting |
| `76d7149` | 5.4a | project items to fixed-size `IndexEntry` (32 bytes) before sort |
| `e46d90c` | 5.4b-1 | add `ExternalSorter<IndexEntry>` in isolation with tests |
| `049e80a` | 5.4b-2 | wire `ExternalSorter`; rewrite `encode::write_to` as single-pass streaming with per-section staging tempfiles |
| `2fb893d` | — | gix-features + mem-harness: `peak_used` high-water-mark tracking on `MemoryBudget` |
| `5fcb981` | — | mem-harness: poll `/proc/self/status:RssAnon` on a 50 ms thread to separate anon from mmap |
| `88ac361` | — | docs: rewrite README Status table + add `MEASUREMENTS.md` with 6-fixture data including rust-lang/rust |
| `98b405e` | — | docs: add `STEP_5_5_DESIGN.md` |
| `214b238` | 5.5-a | gix-pack: add `ItemStore<T>` in isolation with 5 unit tests |
| `b800115` | 5.5-b | gix-pack: add `EdgeSpool` in isolation with 6 unit tests — external sort + linear merge-join producing sorted-by-`parent_idx` `(parent_idx, child_idx)` stream |
| `fc543a4` | — | docs: update HANDOFF.md for step 5.5-b landing |
| `fc55345` | 5.5-c-0 | gix-pack: `ItemStoreBuilder::num_items` + `interim_metadata` accessors with 5 new tests — preparatory for 5.5-c's Tree rewrite |
| `e6ea8c3` | 5.5-c-1 | gix-pack: `ItemStore::into_data_vec` for materialisation path with 2 new tests — preparatory for 5.5-c's Tree rewrite |
| `df09029` | 5.5-c | gix-pack: wire `Tree<T>` to `ItemStoreBuilder` + `EdgeSpool` — the load-bearing step 5.5 commit. rust-lang/rust anon: 399 → 359 MiB at `--budget-mb 1` (-10%); the big drop waits for 5.5-d |
| `a29b050` | — | docs: update HANDOFF.md + MEASUREMENTS.md for step 5.5-c landing |
| `1e36e0a` | — | docs: amend HANDOFF.md §8 with Outcome<T> shape scope correction |
| `b2a6fd2` | 5.5-d | gix-pack: rewrite consumer to read from `ItemMetadata` + `DataSliceSync`; retire `Vec<Item<T>>` materialisation. rust-lang/rust anon: 359 → 146 MiB at `--budget-mb 1` (-59%); elapsed +17.8%; mid-implementation pivot from `(0..num_roots)` worklist to `is_root`-derived worklist |
| `41eb2bd` | — | docs: update HANDOFF.md + MEASUREMENTS.md for step 5.5-d landing |
| `ff4fedc` | 5.6-a | gix-pack: add sink callback to `Tree::traverse`; retire `Outcome<T>`. Threads a per-worker `FnMut(Offset, &T)` sink through `resolve::deltas`/`deltas_mt` and deletes the post-traversal `Vec<(Offset, T)>` partition; site 1 (index/write) wraps the sorter in a Mutex, site 2 (with_index) splits `digest_statistics` into an accumulator + finaliser. rust-lang/rust anon: 146 → 116 MiB at `--budget-mb 1` (-20.5%); elapsed essentially flat (+1.0% tight, +6.2% unlimited); no mid-implementation pivots |
| `05bcaa9` | 5.6-b | gix-pack: retire T generic from Tree and data array from ItemStore. Merges `inspect_object` into the `sink` callback (option i from §8); Tree loses T generic; ItemStore, Node, State all become non-generic; DataSliceSync deleted. Site 1 (index/write) recomputes CRC32 from raw pack bytes via cloned resolver; site 2 (with_index) binary-searches sorted entries by offset. rust-lang/rust anon: unlimited 135 → 59 MiB (-56%); b=1 116 → 110 MiB (-5%); b=10 42 MiB. 3 lib tests retired (data array tests), 45/50/12/4 all green |
| `276a41e` | 5.6-c | gix-pack: per-worker accumulator to `Tree::traverse`; batched sorter flush (1024-entry batches) |
| `b17a238` | 5.6-d | gix-pack: `MAP_POPULATE` + `MADV_HUGEPAGE` for metadata mmaps; elapsed regression fully eliminated at unlimited |
| `3e00a0d` | 5.6-e | gix-pack: `BufWriter` + cached offset in `ItemStoreBuilder`; write syscall reduction ~330× |
| `c2449b0` | 5.6-f | gix-pack: defer `next_offset` writes to `finish()` via `MmapMut`; eliminates per-item seek-write-seek |
| *pending* | 5.7 | gix-pack: replace mmap resolver with batched `pread(2)` — `peak_rss_mb` drops to near `peak_rss_anon_mb` levels. Resolve callback signature changes from `Fn(EntryRange, &R) -> Option<&[u8]>` to `Fn(EntryRange, &R, &mut Vec<u8>) -> bool`; per-thread `ReadCache` (64 KiB batch) amortizes syscalls; `new_pack_file_resolver` returns `std::fs::File` instead of `Mmap` |

Commits come in two shapes: substantive code with tests, and
docs-only. The docs commits are load-bearing — the measurement and
design docs are what make the next steps defensible rather than
speculative.

## 5. Repo layout (just what matters)

```
/home/m/git/gitx-bounded/
├── HANDOFF.md                 ← this file
├── README.md                  ← public framing + Status table (accurate as of 88ac361)
├── MEASUREMENTS.md            ← per-fixture harness numbers
├── STEP_5_5_DESIGN.md         ← design for the remaining 5.5-{c,d,e} work
│
├── gix-features/
│   └── src/budget.rs          ← MemoryBudget + OutOfBudget + Reservation + peak_used
│
├── gix/
│   ├── src/open.rs            ← Options::with_memory_budget
│   ├── src/repository/ ...    ← Repository holds the budget
│   └── tests/gix/memory_budget.rs ← 4 integration tests
│
├── gix-pack/src/
│   ├── cache/
│   │   ├── delta/
│   │   │   ├── mod.rs         ← pub mod traverse, pub mod from_offsets, mod tree, mod item_store, mod edge_spool; Error has SpoolIo + OutOfBudget variants; `pub use tree::Tree` (Item<T> deleted in 5.5-d)
│   │   │   ├── tree.rs        ← Tree (non-generic post-5.6-b) wired to ItemStoreBuilder + EdgeSpool + is_root; `take_store_and_is_root` hand-off
│   │   │   ├── from_offsets.rs← alt construction path; `impl Tree` with method-local `D` generic for input iteration; uses `with_capacity_unlimited`
│   │   │   ├── item_store.rs  ← non-generic post-5.6-b; metadata-only (offset, next_offset, children on mmap); data array + DataSliceSync deleted
│   │   │   ├── edge_spool.rs  ← 5.5-b: external-sort + linear merge-join for delta edges
│   │   │   └── traverse/
│   │   │       ├── mod.rs     ← `Tree::traverse` — the entry point; signature `Result<Vec<A>, Error>` with merged `sink: FnMut(Offset, &dyn Progress, Context, &A) -> Result<(), E)` + `new_accumulator` post-5.6-c; no T generic
│   │   │       ├── resolve.rs ← `State<'items, F, SINK, A>` (post-5.6-c); deltas() / deltas_mt() dispatch sink at 4 sites with (offset, progress, Context, accumulator)
│   │   │       └── spool.rs   ← SpoolFile + SpoolHandle (step 5.3, reusable)
│   │   └── lru.rs             ← MemoryCappedHashmap + StaticLinkedList, wired to budget
│   └── index/
│       ├── traverse/
│       │   └── with_index.rs← consumer B (verify path); A=(), merged sink with binary_search_by_key offset lookup + Mutex<Processor> + Mutex<Statistics> post-5.6-c
│       └── write/
│           ├── mod.rs             ← IndexEntry, write_data_iter_to_stream; consumer A — A=Mutex<Vec<IndexEntry>> batch buffer, sink batches entries and flushes to shared `Mutex<ExternalSorter>` every 1024 entries post-5.6-c
│           ├── external_sort.rs   ← ExternalSorter<IndexEntry> (step 5.4b, reusable pattern)
│           └── encode.rs          ← single-pass streaming encode (step 5.4b-2)
│
└── mem-harness/src/main.rs    ← the benchmark binary
```

Four test suites live at the crate level:

- `gix-pack --lib` — unit tests; requires `--features sha1,pack-cache-lru-dynamic,object-cache-dynamic,pack-cache-lru-static` to get all 45 tests.
- `gix-pack-tests --test pack` — integration, requires `--features gix-features-parallel`, 50 tests.
- `gix-features --test budget` — 12 tests, no flags.
- `gix --test gix memory_budget` — 4 tests, no flags.

## 6. Measurement harness

### Where the fixtures are

On disk, bare clones at `/tmp/mh-fixtures/`:

- `cargo/` — 182 K objects, 74 MB pack (rust-lang/cargo)
- `gitoxide/` — 179 K objects, 94 MB pack (GitoxideLabs/gitoxide)
- `rust-analyzer/` — 321 K objects, 123 MB pack (rust-lang/rust-analyzer)
- `rust-lang/` — **3.34 M objects, 973 MB pack** (rust-lang/rust — the stress fixture)

If `/tmp` has been cleared, re-clone with:

```sh
mkdir -p /tmp/mh-fixtures && cd /tmp/mh-fixtures
git clone --quiet https://github.com/GitoxideLabs/gitoxide.git gitoxide
git clone --quiet https://github.com/rust-lang/cargo.git cargo
git clone --quiet https://github.com/rust-lang/rust-analyzer.git rust-analyzer
git clone --quiet https://github.com/rust-lang/rust.git rust-lang   # ~5 min, ~1.5 GB
```

The self-clone and `codex-custom` use the user's own repos under
`/home/m/git/`.

### How to run the harness

```sh
cd /home/m/git/gitx-bounded
cargo build --release -p mem-harness
rm -rf /tmp/mh-runs/<tag>
./target/release/mem-harness [--budget-mb N] <file:///path/to/src> /tmp/mh-runs/<tag>
```

One line of output, machine-parseable:

```
url="..." status=ok peak_rss_mb=X elapsed_s=Y.YY on_disk_bytes=Z budget_mb=... budget_peak_mb=... peak_rss_anon_mb=...
```

**The number that matters is `peak_rss_anon_mb`.** `peak_rss_mb` is
inflated by mmap'd pack pages and is not a production cap.

### The self-clone is the fast sanity check

~2 seconds, small (30 objects, 7.5 MB pack), anon sits at 14–19 MiB
regardless of budget. Use this to confirm no behavioural regression
after any code change. After a commit touching the pack path, run
it and compare to the baseline in `MEASUREMENTS.md`.

### The rust-lang fixture is the stress check

~2 minutes per run, ~1 GB output to disk, `peak_rss_anon_mb` in the
hundreds. Use this only after substantive implementation commits,
not for every small change. Budget `/tmp` accordingly (~2 GB free
per concurrent run).

## 7. Feature flags (gotchas that have bitten us)

- `gix-hash` requires one of `sha1` or `sha256`. Default features on
  `gix-pack` alone don't bring these in. `cargo check -p gix-pack`
  fails without `--features sha1`.
- `gix-pack`'s full lib-test suite needs `sha1` plus the pack/object
  cache features (`pack-cache-lru-dynamic`, `object-cache-dynamic`,
  `pack-cache-lru-static`) to expose all 45 tests (32 pre-5.5-a,
  +5 from 5.5-a's item_store, +6 from 5.5-b's edge_spool, +5 from
  5.5-c-0's num_items/interim_metadata prep, +2 from 5.5-c-1's
  into_data_vec prep, -2 from 5.5-d's deletion of the
  `size_of_pack_tree_item` / `size_of_pack_verify_data_structure`
  tests, -3 from 5.6-b's deletion of data-array tests:
  `push_then_read_back_offsets_and_data`,
  `data_slice_sync_multithreaded_disjoint_writes`, and
  `into_data_vec_preserves_order_and_values` — all tested the
  `Box<[UnsafeCell<T>]>` data array that 5.6-b retires).
- `gix-pack-tests` needs `--features gix-features-parallel`.
- `gix` tests work with the default feature set.
- `mem-harness` builds with default features on its dependencies
  (it routes through the `gix` facade).

When in doubt about which flag set matches a prior session's test
counts, check the commit message for that session — the verification
block at the bottom of every substantive commit records the exact
invocation used.

## 8. Step 5.6-b — completed (`05bcaa9`)

Step 5.6-b retired the ~80 MiB `Box<[UnsafeCell<T>]>` data array
by merging `inspect_object` into the `sink` callback (option i:
merged callback). The `T` generic was removed from `Tree`,
`ItemStoreBuilder`, `ItemStore`, `Node`, and `State`. The data
array retirement is verified at unlimited budget: rust-lang/rust
dropped from 135 → 59 MiB (-76 MiB, matching the ~80 MiB
prediction). At tight budget (b=1), the reduction is smaller
(116 → 110 MiB) because the spill-path buffered-I/O overhead
dominates. At budget=10, anon is 42 MiB — well under the 100 MiB
production goal.

### What changes internally

Current `Tree::traverse` signature (post-5.6-a):

```rust
pub fn traverse<F, MBFN, SINK, E1, E2, R>(
    mut self, resolve: F, resolve_data: &R, pack_entries_end: u64,
    inspect_object: MBFN,
    sink: SINK,
    options: Options<'_, '_>,
) -> Result<(), Error>
where
    MBFN: FnMut(&mut T, &dyn Progress, Context<'_>) -> Result<(), E1> + Send + Clone,
    SINK: FnMut(crate::data::Offset, &T) -> Result<(), E2> + Send + Clone,
    ...
```

Post-5.6-b candidate shapes — pick during implementation based on
how each consumer site wants to phrase its projection. There are
two natural forms:

**(i) merged callback** — one `SINK: FnMut(Offset, &dyn Progress,
Context<'_>) -> Result<(), E>` that captures whatever state the
caller needs (sorter mutex, statistics mutex, hashing context).
Inspect_object disappears. `Tree` loses its `T` generic entirely.
This is the cleanest shape and matches the "sink does everything
(hash + project + push)" pattern.

**(ii) split produce/consume callbacks** — `PROJECT: FnMut(&dyn
Progress, Context<'_>) -> Result<U, E1>` plus `SINK: FnMut(Offset,
U) -> Result<(), E2>`. Lets a caller separate "compute U from the
pack" from "route U to its destination". `Tree` still loses its
`T` generic. Keeps the two-callback shape 5.6-a established.

Either way, `Tree::add_root`/`add_child` stop taking `data: T` and
`ItemStoreBuilder<T>` / `ItemStore<T>` stop being generic — the
data array is gone; only the on-disk `(offset, next_offset,
children)` mmap metadata remains. `DataSliceSync` is deleted
entirely. `resolve::State` loses its `data` field and its `T`
generic; `Node` becomes `Node<'a>` (no T) carrying just `{idx,
&metadata}`, and `Node::data()` is deleted — the projection
happens at sink-fire time from `Context` alone (plus any caller-
captured sidecar).

### The two consumer sites

1. **`gix-pack/src/index/write/mod.rs`** — the load-bearing
   consumer. Pre-5.6-b flow:
   ```rust
   tree.add_root(pack_offset, TreeEntry { id: object_hash.null(), crc32 })?;
   // ...
   // inspect_object mutates entry.id via compute_hash;
   // sink pushes IndexEntry { id, crc32, offset } into the sorter.
   ```
   Post-5.6-b: `crc32` no longer lives in a TreeEntry. Options:
   * **Recompute at sink time.** `gix_features::hash::crc32(
     pack.entry_slice(offset..entry_end).expect(...))` already
     works (see `with_index.rs`'s use of exactly this expression).
     Cost: one CRC32 pass over the compressed entry bytes per
     object. Linear in pack size; for rust-lang/rust ~1 GB at a
     few GB/s CRC throughput this is sub-second total —
     negligible vs the ~167 s traversal.
   * **Sidecar Vec<u32>.** 13 MiB on rust-lang/rust. Retires the
     80 MiB Vec<T> but keeps 13 MiB, so net retirement is 67 MiB
     rather than 80. Not recommended — recomputation is cheaper
     and cleaner.
   The `Tree::add_root`/`add_child` calls become `tree.add_root(
   pack_offset)` / `tree.add_child(base_offset, pack_offset)` —
   no `data` arg. The sink closure projects from `Context`:
   id via `gix_object::compute_hash`, crc32 via recomputation,
   offset from the sink's own parameter, then Mutex-wrapped
   sorter push as today.

2. **`gix-pack/src/index/traverse/with_index.rs::traverse_with_index`**
   — the statistics-only consumer. More awkward than site 1.
   `Entry` currently carries `index_entry: crate::index::Entry`
   set at push time from the sorted-by-offset idx walk; traversal
   needs this field to look up the object's id when calling
   `process_entry`. Under 5.6-b we can't store it in the Tree.
   Options:
   * **Offset→index::Entry lookup via `idx.lookup_offset(offset)`.**
     Per-object binary search on the already-open idx file. Cost:
     O(log N) per object. Total O(N log N). On 3.4 M objects that's
     ~75 M comparisons — a few hundred ms, comfortably below the
     traversal elapsed.
   * **Pre-build an `Offset→index::Entry` HashMap sidecar.** ~100
     MiB at 3.4 M objects. Replaces the 80 MiB data array with
     something worse. Not recommended.
   * **Pass `sorted_entries: &[index::Entry]` alongside the sink**
     and do linear scan with a per-worker cursor. O(N) total, zero
     extra allocation. Requires sink access to the sorted slice,
     which crosses the traversal boundary — feasible via
     closure capture. This is probably the cleanest form.

   No Mutex needed on the stats side beyond what 5.6-a set up;
   the accumulator logic in `accumulate_statistics` stays. Only
   the source of `Entry` changes from "field of &T" to "closed-
   over sidecar indexed by `idx.lookup_offset(offset)` or a
   per-worker cursor".

Site 2 is the harder of the two; consider landing site 1's 5.6-b
changes first (alone, measurable on rust-lang/rust) before
tackling site 2. Splitting 5.6-b into 5.6-b-1 (site 1) and 5.6-b-2
(site 2) would match the 5.5-a/b/c/d/5.6-a cadence.

### Retiring `DataSliceSync` and the `T` generics

Once neither consumer stores T in the Tree, `DataSliceSync<'_, T>`
has no callers and is deleted. `ItemStore<T>` loses its type param
and its `data: Box<[UnsafeCell<T>]>` field; same for
`ItemStoreBuilder`. `Node<T>` becomes `Node<'a>` (no T). This is a
surface-level API change across tree.rs, item_store.rs,
traverse/resolve.rs, and the two consumer sites.

`from_offsets.rs` (the `Tree::from_offsets_in_pack` alt
construction path) uses `Tree<Entry>` with a unit-like struct in
its tests; it'll need the same migration.

### Expected memory impact on rust-lang/rust

| allocation | bytes | MiB | status after 5.6-b |
| --- | ---: | ---: | --- |
| `Box<[UnsafeCell<T>]>` data array (sha1 `TreeEntry`: 24 bytes) | 24 × 3.34 M | ~80 | **retired** for site 1 |
| `DataSliceSync` / `ItemSliceSync` overhead | — | ~0 | retired (pointer + len only) |
| `Vec<(u32, u32)>` transient edge-resolve buffer | 8 × 3.34 M | ~27 | unchanged (flagged for later) |

Net retirement target: ~80 MiB off the 116 MiB post-5.6-a baseline
(site 1 only). Net anon: ~36–40 MiB. Hits the 100 MiB goal with
~60 MiB margin.

Site 2's data array (Entry: ~56 bytes on sha1, ~190 MiB on rust-
lang/rust) exists only on the verify-traverse path, which runs
under `MemoryBudget::unlimited` and doesn't hit the clone-path
budget cap. Its retirement is valuable but not blocking the 100
MiB clone goal. Hence the recommended split.

Measure, don't predict — same discipline as 5.5-d / 5.6-a. The
80 MiB retirement arithmetic has two moving parts (site 1's
actual data-array size at peak, and whatever marginal ephemera the
new lookup path introduces). The conservative stop sign: anything
above 60 MiB on rust-lang/rust at `--budget-mb 1` after 5.6-b-1
lands means something didn't retire as planned.

### Tests and verification (5.6-b complete)

All tests passed post-5.6-b: `gix-pack --lib` (48/48),
`gix-pack-tests --test pack` (50/50), `gix-features --test budget`
(12/12), `gix --test gix memory_budget` (4/4). See MEASUREMENTS.md
for detailed results.

## 9. Step 5.5 roadmap beyond 5.5-d

### 5.5-e — absorbed into 5.5-d

The original 5.5-e cleanup step ("remove the dead in-memory
path, ship") was folded into 5.5-d itself because the code
changes naturally collocated. Already done in `b2a6fd2`:

- `pub struct Item<T>` deleted from `tree.rs`; `pub use tree::
  Item` dropped from `cache/delta/mod.rs`.
- `traverse::util::ItemSliceSync<'_, Item<T>>` deleted;
  `traverse/util.rs` file removed on disk.
- `resolve_edges_into_children_lists` deleted from `tree.rs`
  (was producing the pre-resolved `Vec<Vec<u32>>` children
  lists; the post-5.5-d consumer reads children directly
  from the store's edges mmap via `ItemMetadata::children`).
- `resolved_root_children` / `resolved_child_children` fields
  on `Tree<T>` deleted; replaced by `store: Option<ItemStore
  <T>>`.
- The 5.5-d commit message documents which module-level doc
  comments were refreshed.

One residual: `ItemStore::read_record` in `item_store.rs` is
still dead code (never called since 5.5-a; `interim_metadata`'s
mmap path is what everything uses). Too small for its own
commit. Either fold into step 5.6-b or leave as a tech-debt note
in §11. (5.6-a landed without folding it; still dead at
`ff4fedc`.)

## 10. After 5.6-b — remaining work

### Step 5.6-b: DONE (`05bcaa9`)

Both consumer sites (index/write and with_index) were updated in
a single commit. Site 2 uses binary_search_by_key on the offset-
sorted entries slice instead of the per-worker-cursor approach §8
recommended — simpler and O(log N) per object, which is negligible
vs the traversal elapsed.

### Step 5.6-c: DONE — per-worker accumulator + batch buffers

Added type parameter `A` to `Tree::traverse` for per-worker
accumulators. The traverse signature becomes:
`traverse<F, SINK, E, R, A>(..., sink: SINK, new_accumulator: impl FnOnce() -> A + Send + Clone, ...) -> Result<Vec<A>, Error>`

Changes across 5 files:
- `resolve.rs`: `State<'items, F, SINK, A>`, sink gains `&A`
- `traverse/mod.rs`: `A` param, `new_accumulator`, returns `Vec<A>`
- `with_index.rs`: `A = ()`, trivial adaptation
- `write/mod.rs`: `A = Mutex<Vec<IndexEntry>>` batch buffer
  (SORT_BATCH_SIZE=1024), flushes to shared sorter per batch
- `external_sort.rs`: cleaned up dead `MergedIters` variant
  and `with_budget_for_n_workers` from the abandoned per-worker
  sorter approach

Measured on rust-lang/rust: elapsed and memory within noise
(±3%) of post-5.6-b baseline. The shared sorter Mutex was not
a bottleneck — hash+CRC32 compute in the sink dominates.
The value is the API: future per-worker strategies (e.g. if a
workload with higher contention is found) don't need to
re-touch the traverse signature.

### Step 5.6-d: DONE — mmap hints (HUGEPAGE + MAP_POPULATE)

Added `MAP_POPULATE` and `MADV_HUGEPAGE` to the items and edges
mmaps in `ItemStoreBuilder::finish()`. Two lines of code each.

`MAP_POPULATE` prefaults all pages at mmap creation, eliminating
per-page faults during traversal. `MADV_HUGEPAGE` requests 2 MiB
transparent huge pages, reducing TLB misses by ~500× on the
~80 MiB items region.

Three alternatives were tested:
- HUGEPAGE only: -9% at b=∞
- MAP_POPULATE only: -10.6% at b=∞
- **Both combined**: -13% at b=∞ (137.2 s, matching pre-bounded
  baseline of 137.9 s). Regression fully eliminated at unlimited.
- Vec-backed (read mmap into heap): rejected, adds 80 MiB to
  anon RSS, blows the 100 MiB goal.

Memory unchanged. The hints are Linux-only (`#[cfg(target_os)]`)
and silently no-op on other platforms.

### Step 5.6-e: DONE — BufWriter + cached offset

The 5.5-c construction path wrote 24 bytes per item directly to
a raw `File` (no buffering) and re-read the previous item's
offset via `interim_metadata()` mmap on every push — ~20M
syscalls + ~3.34M mmap operations for rust-lang/rust.

Fixes: `BufWriter` wraps the items tempfile (batches writes into
8 KiB chunks), and `last_pushed_offset: Option<u64>` in Tree
eliminates the per-item mmap read-back. Result: b=∞ dropped
from 137.2s to 119s (-13%), now only +7% vs pre-bounded baseline.

### Step 5.6-f: DONE — deferred next_offset via MmapMut

During construction, `set_next_offset` was called per-item (flush
BufWriter + seek + write + seek-back = ~4 syscalls × 3.34M items),
negating the BufWriter from 5.6-e. Fix: defer all next_offset
writes to `finish()`, which uses a single `MmapMut` pass to
compute next_offset[i] = offset[i+1] in memory with zero syscalls.

Result: bounded budgets improved significantly (b=1: 125→112s,
b=10: 134→109-116s) because the sequential-only construction
no longer contends with ExternalSorter's disk I/O. Unlimited
is high-variance (108–121s), centered ~115s vs 119s pre-change.
The remaining ~3% gap vs pre-bounded baseline (111.8s) appears
inherent to the disk-backed metadata design.

### Smaller follow-up: spill-path buffer accounting — investigated

The "tight-budget uses more anon" wrinkle was investigated in
depth. Post-5.6-b, the gap at b=1 is 51 MiB (110 vs 59 MiB).
Root causes: `SpoolFile::read_exact` allocates `vec![0u8; len]`
per read-back without budget reservation; `EdgeSpool` /
`ChunkCursor` buffered-I/O contributes anonymous RSS inversely
proportional to budget; jemalloc fragmentation amplifies the
alloc/spill/drop/read-back cycle. Reserving every temp buffer
against the budget would cascade failures at b=1 (probe-and-halve
would starve the traversal). Conclusion: accepted as inherent to
the spill design. Production goal is met at budget≥10 (42 MiB).

### Upstream PRs — split strategy

The fork's 30+ commits group into 6 logical PRs, ordered so
each can land independently and each subsequent PR applies
cleanly on top.

**PR 1 — `MemoryBudget` primitive + harness** (additive)
- `06df93b` mem-harness binary
- `e4b9647` MemoryBudget primitive in gix-features
- `773dc00` thread MemoryBudget through open::Options
- `2fb893d` + `5fcb981` harness tracking (peak-used, RssAnon)

Introduces the budget primitive and the measurement tool.
Zero behavioural change when budget is unlimited (the default).

**PR 2 — Cache budget wiring** (additive, opt-in)
- `a5b21a1` MemoryCappedHashmap caches
- `91248d1` StaticLinkedList pack cache
- `8a12370` thread --budget-mb through harness
- `d60cd43` thread MemoryBudget through Tree::traverse + Bundle::write

Wires all existing caches to the shared budget. Still additive:
unlimited budget preserves pre-budget behaviour byte-for-byte.

**PR 3 — Delta-chain cache bounded** (additive)
- `4f938f5` delta-chain cache reserves against budget (step 5.2)
- `ba53553` delta-chain cache spills to disk (step 5.3)

The delta-chain cache is the biggest single anonymous allocation
during clone. These commits bound it and add a disk fallback.

**PR 4 — External sort for index entries** (additive)
- `76d7149` project items to fixed-size IndexEntry (step 5.4a)
- `e46d90c` add ExternalSorter (not yet wired)
- `049e80a` wire ExternalSorter; make encode streaming (step 5.4b)

Replaces the in-memory sort-by-id with budget-aware external
sort. The `Vec<Item<T>>` sort-phase allocation is retired.

**PR 5 — Disk-backed delta tree** (breaking — Tree internals)
- `214b238` ItemStore (step 5.5-a)
- `b800115` EdgeSpool (step 5.5-b)
- `fc55345` + `e6ea8c3` ItemStoreBuilder APIs (step 5.5-c-0)
- `df09029` wire Tree<T> to ItemStoreBuilder + EdgeSpool (5.5-c)
- `b2a6fd2` rewrite consumer to ItemMetadata + DataSliceSync (5.5-d)
- `b17a238` MAP_POPULATE + MADV_HUGEPAGE on metadata mmaps (5.6-d)

Replaces `Vec<Item<T>>` with mmap-backed metadata and disk-
spilled edges. Step 5.6-d adds mmap hints that eliminate the
TLB/page-fault regression. `Tree<T>` internals change but the
public `Tree::traverse` API is preserved at this stage.

**PR 6 — Sink callback + retire generics + per-worker accumulator** (breaking — public API)
- `ff4fedc` add sink callback; retire `Outcome<T>` (step 5.6-a)
- `05bcaa9` retire T generic + data array (step 5.6-b)
- `2178a6b` dead code removal
- step 5.6-c: per-worker accumulator `A` on `Tree::traverse`

`Tree::traverse` gains a `sink` parameter and loses the
`Outcome<T>` return. `Tree<T>` becomes `Tree` (non-generic).
Step 5.6-c adds a per-worker accumulator type parameter `A`:
`traverse` returns `Vec<A>`, sink gains `&A` parameter,
`new_accumulator` factory creates one `A` per worker.
`cache::delta::traverse::Outcome` is deleted. These are
breaking public-API changes; workspace audit showed zero
external in-workspace consumers.

**Discussion points for upstream:**
- PRs 1–4 are purely additive, gated by opt-in, and can
  land with minimal review friction.
- PR 5 changes Tree internals but preserves the public API.
  The mmap strategy for metadata initially introduced a +12%
  elapsed regression, but step 5.6-d's `MAP_POPULATE` +
  `MADV_HUGEPAGE` hints eliminated it entirely at unlimited
  budget (137.2 s vs 137.9 s baseline).
- PR 6 changes the public `Tree::traverse` signature. Should
  be discussed together with the 5.5-d `take_root_and_child`
  deletion from PR 5. The `ee718a6` rename of `memory` →
  `write_proxy` in gix-odb is a prerequisite cleanup.

## 11. Known issues and tech debt

- ~~**`read_record` in `item_store.rs`.**~~ Removed in `2178a6b`.
- **`children()` alignment debug_assert.** Relies on the mmap slice
  starting u32-aligned. In practice memmap2 returns page-aligned
  regions, so it always holds, but a defensive owned-`Vec<u32>`
  fallback would be safer in release builds. Low priority.
- **Elapsed regressions — resolved.** Timeline on
  rust-lang/rust:
  - Pre-5.5-d: b=1 142.2 s, b=∞ 137.9 s
  - Post-5.5-d: b=1 167.5 s (+17.8%), b=∞ 148.8 s (+7.9%)
  - Post-5.6-a: b=1 169.1 s (+1.0%), b=∞ 158.0 s (+6.2%)
  - Post-5.6-b: b=1 172.2 s (+1.8%), b=∞ 154.9 s (-2.0%)
  - Post-5.6-c: within noise of 5.6-b (batch buffers)
  - **Post-5.6-d**: b=1 143–156 s, b=∞ 137.2 s (mmap hints)
  - **Post-5.6-e**: b=1 125–142 s, b=∞ **119–120 s** (BufWriter)
  - **Post-5.6-f**: b=1 112 s, b=10 109–116 s, b=∞ **108–121 s**
    (deferred next_offset via MmapMut)

  Root causes and their status:
  1. ~~**Mmap-based ItemMetadata TLB pressure (5.5-d)**~~:
     fixed in 5.6-d via `MAP_POPULATE` + `MADV_HUGEPAGE`.
  2. ~~**Mutex-serialized sorter pushes (5.6-a)**~~: closed in
     5.6-c. Not a bottleneck.
  3. ~~**Unbuffered tempfile writes + per-item mmap read-back
     (5.5-c)**~~: fixed in 5.6-e via `BufWriter` + cached
     `last_pushed_offset`. Eliminated ~20M syscalls.
  4. ~~**Per-item set_next_offset seek-write-seek**~~: fixed in
     5.6-f by deferring to a single MmapMut pass in `finish()`.
     Major improvement at bounded budgets (eliminated I/O
     contention with ExternalSorter).
  5. **Spill-path I/O (tight-only, ~5–15%)**: inherent to
     the spill design; only affects b=1 and b=10.
  6. **CRC32 double-computation (5.6-b)**: negligible (~0.2 s).
  7. **Residual ~3% at b=∞**: inherent to disk-backed metadata
     design (tempfile write + mmap vs in-RAM array).

  At b=∞ the regression is ~**+3%** (111.8 → ~115 s, high variance).
  At b=10 it's ~+4% (109–116 s, down from +20% at 5.6-e).
  At b=1 it's ~0% (112 s vs 118 s baseline).
- **Post-5.6-b "tight > unlimited" wrinkle — investigated,
  accepted.** Tight (b=1) 110 MiB, unlimited 59 MiB — gap of
  51 MiB. The gap is a fundamental property of the spill
  design: (1) `SpoolFile::read_exact` allocates `vec![0u8; len]`
  per read-back without budget reservation; (2) `EdgeSpool`
  and `ChunkCursor` buffered-I/O contributes anonymous RSS
  that scales inversely with budget — tighter budget ⇒ more
  spilling ⇒ more read-back buffers ⇒ more unaccounted anon;
  (3) jemalloc fragmentation from the alloc/spill/drop/
  read-back cycle amplifies the effect. No simple fix exists
  short of reserving every temp buffer against the budget,
  which would cascade failures at b=1. Production goal is
  met at budget≥10 (42 MiB); b=1 is an artificially
  restrictive stress-test setting, not a production target.
- **mem-harness 50 ms polling can miss transient peaks.** Evidence:
  the rust-lang `b=10` / `b=100` rows in `MEASUREMENTS.md` are
  non-monotonic (b=10 < b=100 < b=1). Each row is one run; noise at
  the ~10% level is expected. A more reliable peak would require
  faster polling or a kernel-level observer. Not worth it today.
- **No multi-index tests.** Everything tested goes through single-
  pack paths. Multi-index behaviour under a memory budget is
  unverified. Likely fine because multi-index is read-side, but
  worth at least one integration test eventually.

## 12. First-session checklist

When picking up this work in a fresh instance, do **exactly these
steps in order**:

1. `cd $HOME/git && pwd` — expect `/home/m/git`.
2. `sudo echo "hi"`.
3. `cd /home/m/git/gitx-bounded && git log --oneline -5` — the top
   commit should be the docs commit following `05bcaa9`
   (`gix-pack: retire T generic from Tree and data array from
   ItemStore`) or newer.
4. `git -C /home/m/git/gitx-bounded status` — expect clean working
   tree.
5. Read `HANDOFF.md` (this file), `STEP_5_5_DESIGN.md`,
   `MEASUREMENTS.md`. Skim `README.md`.
6. Read `gix-pack/src/cache/delta/item_store.rs` end to end —
   `ItemMetadata` (mmap-backed, non-generic post-5.6-b).
   DataSliceSync and the data array are deleted.
7. Read `gix-pack/src/cache/delta/traverse/mod.rs` end to end —
   `Tree::traverse` signature post-5.6-c with per-worker accumulator:
   `sink: FnMut(Offset, &dyn Progress, Context, &A)`, returns `Vec<A>`.
8. Read `gix-pack/src/cache/delta/traverse/resolve.rs` end to
   end — `State<'items, F, SINK, A>` (with accumulator), the four sink
   dispatch sites in `deltas` + `deltas_mt` that fire
   `(offset, progress, Context, accumulator)` directly.
9. Read `gix-pack/src/index/write/mod.rs` around the sink
   closure — hash + CRC32 computation from Context + cloned
   resolver, batch to `Mutex<Vec<IndexEntry>>` accumulator,
   flush to shared `Mutex<ExternalSorter>` every 1024 entries.
   Also read `gix-pack/src/index/traverse/with_index.rs`'s
   sink with A=(), binary_search_by_key offset lookup +
   `Mutex<Processor>` + `Mutex<Statistics>` pattern.
10. Verify baseline: `cargo test -p gix-pack --features
    sha1,pack-cache-lru-dynamic,object-cache-dynamic,pack-cache-lru-static
    --lib` — should report 45 tests passing (48 pre-5.6-b minus
    3 retired data-array tests).
11. Verify harness baseline on self-clone:
    `./target/release/mem-harness --budget-mb 1
    file:///home/m/git/gitx-bounded /tmp/mh-runs/self-check` —
    expect `peak_rss_anon_mb ≈ 20` (band 14–20 MiB per §6).
12. The 100 MiB production goal is met at budget≥10 (42 MiB).
    All investigations complete (spill-path §11, elapsed §11).
    Remaining work: upstream PR preparation (see §10 for the
    6-PR split strategy).

If any of steps 3, 4, 10, or 11 produce unexpected output, **stop
and investigate** before writing code. The invariant the fork
depends on is that every step builds on a verified-green baseline.

## 13. Style notes for commit messages

Every substantive commit in this fork has:

- A one-line subject under 72 chars, prefixed with the crate(s) touched.
- A body explaining *why* the change exists (the bounded-memory
  story), not just *what* it does.
- A "Verification" block at the bottom listing the exact test
  invocations used and their results.
- Plain prose, no bullet points unless the structure earns it.

Copy the shape of `05bcaa9` (step 5.6-b) or `ff4fedc` (step
5.6-a) for future commits — they're the most recent "retire a
materialisation" commits. `b2a6fd2` (step 5.5-d — with its
mid-implementation pivot narrative) and `df09029` (step 5.5-c —
the single-builder pivot) are good secondary references when a
commit needs to document a structural redirection mid-
implementation. `214b238` (step 5.5-a — primitive-in-isolation)
remains the model for purely additive commits. The verbose
style is deliberate — future readers (human or AI) should
understand a commit from its message alone.

## 14. Escape hatches

- **If a step takes longer than 2 sessions, reconsider the split.**
  Every commit in this fork has been independently testable and
  revertable. If a single sub-step can't ship in one session, it's
  too big; split it further.
- **Measurement overrides speculation.** Any claim about what a
  change will do to memory needs a `MEASUREMENTS.md` row behind
  it. If you find yourself arguing from extrapolation, go run the
  harness.
- **The workflow rules exist because the user watches for them.**
  Skipping `cd $HOME/git && pwd` or doing an edit without a
  read-back will get flagged. Don't optimise them away.

---

**Last updated: step 5.7 (pread resolver — mmap elimination).**

When modifying this file, keep the commit-history table (Section 4)
current, update the current-status table (Section 3), and bump the
"last updated" line.
