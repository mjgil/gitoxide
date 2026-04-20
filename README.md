# gitx-bounded

A fork of [gitoxide](https://github.com/GitoxideLabs/gitoxide) whose goal is to
make every memory-relevant operation **hard-bounded** by an explicit byte
budget, rather than growing with input size.

Upstream's original README is preserved at [`README.upstream.md`](./README.upstream.md).

## Why this fork exists

gitoxide (the `gix` crate family) is fast and correct, but it has no
process-wide memory cap. On the paths that matter most for a self-hosted
mirror (clone, fetch, pack indexing, delta resolution), a sufficiently
large or adversarial remote can allocate enough memory to OOM the host.

The concrete case that motivated this:
[`git-vault-server`](https://github.com/mjgil/git-vault) uses `gix` to
mirror remote repositories on a small VPS. On a 1 GB RAM host, mirroring
a single large repository (kernel-sized, monorepo-sized, or just a repo
with unusually deep delta chains) can trigger the Linux OOM killer
mid-clone. The server code around `gix` is already memory-bounded — the
unbounded edge is inside `gix` itself.

The goal here is a drop-in replacement where you can write something like:

```rust
let opts = gix::open::Options::isolated()
    .with_memory_budget(gix::MemoryBudget::bytes(512 * 1024 * 1024));
```

and have any operation that would exceed 512 MB return a clean
`Error::OutOfBudget` instead of allocating through the ceiling. The
operation fails cleanly; the process stays up; the caller decides what to
do next (retry smaller, skip repo, alert operator).

## Scope — what we will and will not change

**Target crates (the actual work happens here):**

- `gix-pack` — pack indexing, delta resolution, decoded-object cache.
  Biggest single source of memory pressure.
- `gix-odb` — object database access and pack cache wiring.
- `gix-protocol` and `gix-transport` — streaming pack receive; want
  bounded receive buffers and bounded-size spool-to-disk on large packs.
- `gix` — the umbrella crate. Thread a `MemoryBudget` through
  `open::Options`, `clone::PrepareFetch`, and the `Repository` handle.
- `gix-features` — may need a small `Budget` primitive here (atomic
  counter + `try_reserve`) that other crates import, since it already
  owns the cross-cutting threading/parallelism types.

**Not modified, only carried along for the build:** the other ~60
sub-crates in the workspace. Modifying `gix-pack` in isolation wouldn't
compile — it pulls in `gix-hash`, `gix-object`, `gix-chunk`, etc. — and
the only way to verify memory-boundedness is end-to-end through a real
clone via `gix`, which pulls in essentially the whole tree. So we carry
everything and only edit the hot path.

## Prior art already in the tree

Worth flagging, so this doesn't read as "invent from scratch":

- `gix-pack/src/cache/lru.rs` already contains `MemoryCappedHashmap`, a
  byte-weighted LRU (via `clru::WeightScale`) for decoded pack entries.
  This is the right shape of primitive — it just isn't wired up as a
  global budget, it's a per-cache instance with no shared accounting.
- `gix-pack/src/cache/delta/traverse/resolve.rs` is where delta chains
  are materialized; today it allocates per-chain without an overall cap.
- `gix-odb/src/write_proxy.rs` (renamed from upstream's misleading
  `memory.rs`) is an in-memory write-through *proxy*, not a memory
  cap. Unrelated to the bounded-memory effort; renamed here so the
  filename matches the function.

The design work is: make these per-cache primitives cooperate with a
single process-wide (or `Repository`-scoped) `MemoryBudget`, and replace
unbounded allocation sites with `budget.reserve(n)?` calls.

## Approach

For each memory-hot site the loop is:

1. **Measure.** Before touching anything, write a stress test that clones
   a known-large repo (or a synthetic packfile designed to stress delta
   resolution), runs under `memory-bounded-harness` (TBD), and records
   peak RSS. This establishes the before-number.
2. **Identify the largest single allocation.** For pack indexing, it's
   typically the fully-resolved delta tree in RAM. For fetch, it's the
   received packfile buffer. For object access, it's the decoded-object
   cache. The goal is to name the worst offender and attack it first.
3. **Replace with a bounded alternative.** Streaming decode (spool to
   disk when over budget), fixed-size LRU, incremental index build. In
   each case the API grows an `Error::OutOfBudget` variant.
4. **Verify.** Re-run the stress test with an explicit small budget; the
   operation must either succeed within the budget or return
   `OutOfBudget` cleanly. "Hit the OOM killer" counts as a test failure.
5. **Benchmark.** Compare wall-clock runtime at the old (unbounded)
   defaults against the new bounded defaults. There will be a slowdown
   from spooling; we want it characterized, not discovered in production.

## Status

As of commit `5fcb981`, the `MemoryBudget` primitive, plumbing, and
three of the four major memory-hot sites are bounded. The remaining
unbounded site is the in-RAM delta-tree acceleration structure
(`gix-pack/src/cache/delta`) held during pack traversal. Stress
measurement on `rust-lang/rust` (3.34 M objects) puts anonymous RSS
at 399 MiB under a 1 MiB budget — the `MemoryBudget` counter is
binding, but the 399 MiB is held outside it. Bounding the delta-tree
is step 5.5 and is required to meet the production goal on large
repos. See [`MEASUREMENTS.md`](./MEASUREMENTS.md) for the full
per-fixture numbers.

### Memory-hot sites

| site | crate | status | commit(s) |
| --- | --- | --- | --- |
| `MemoryBudget` primitive | `gix-features` | landed, tested | `e4b9647` |
| `open::Options` / `Repository` threading | `gix` | landed | `773dc00` |
| `MemoryCappedHashmap` object cache | `gix-pack`, `gix` | wired to shared budget | `a5b21a1` |
| `StaticLinkedList` pack cache | `gix-pack`, `gix` | wired to shared budget | `91248d1` |
| `clone::PrepareFetch` / `Bundle::write_to_directory` | `gix`, `gitoxide-core` | threaded | `8a12370`, `d60cd43` |
| delta-chain cache (resolve) | `gix-pack` | bounded, spills to disk on pressure | `4f938f5`, `ba53553` |
| pack-index build — sort phase | `gix-pack` | streaming external merge sort over fixed-size records | `76d7149`, `e46d90c`, `049e80a` |
| pack-index build — encode phase | `gix-pack` | single-pass, per-section staging tempfiles | `049e80a` |
| pack receive buffer | `gix-pack` (bundle/write) | already streamed to tempfile upstream (not our work) | — |
| delta-tree during traversal (`cache::delta::Tree`) | `gix-pack` | **unbounded, O(N) nodes** | pending (see gating below) |
| harness: peak-RSS + anonymous-RSS + budget-peak reporting | `mem-harness` | landed | `06df93b`, `2fb893d`, `5fcb981` |

### API surface, landed

The API below is implemented as of `773dc00`, with `with_memory_budget`
available on `gix::open::Options` (which `PrepareFetch` accepts). Callers
who go through `PrepareFetch` attach the budget via the `Options` passed
to `PrepareFetch::new`.

```rust
// gix_features::budget, re-exported as gix::budget::MemoryBudget
pub struct MemoryBudget { /* Arc<AtomicU64>: used + peak + cap */ }

impl MemoryBudget {
    pub fn bytes(n: u64) -> Self;
    pub fn unlimited() -> Self;
    pub fn reserve(&self, n: usize) -> Result<Reservation, OutOfBudget>;
    pub fn peak_used(&self) -> u64;  // high-water mark, for observability
}

impl gix::open::Options {
    pub fn with_memory_budget(self, b: MemoryBudget) -> Self;
}
```

Current tests: `gix-features::budget` 12/12, `gix::memory_budget` 4/4,
`gix-pack` lib 32/32 (includes 6 `external_sort` tests + traversal and
spool coverage), `gix-pack-tests` 50/50 (includes byte-for-byte index
equality under `MemoryBudget::bytes(0)`).

### Gating for the remaining work (delta-tree)

The in-RAM delta-tree (`cache::delta::Tree<Item<TreeEntry>>`) is the
last unbounded O(N) site. Bounding it is a multi-commit refactor—disk-
backed by pack offset, bounded in-RAM frontier, streaming traversal.

Measurement done. On the six fixtures in
[`MEASUREMENTS.md`](./MEASUREMENTS.md), peak anonymous RSS at
`--budget-mb 1` stays below 40 MiB up through 321 K objects
(`rust-lang/rust-analyzer`), then jumps to **399 MiB at 3.34 M objects**
(`rust-lang/rust`). The `MemoryBudget` counter holds at 1 MiB the whole
time, so the ~400 MiB is held entirely outside the budget—the delta-
tree and a smaller amount of spill-path ephemera. This is the signal
that step 5.5 is required to meet the 100 MiB-for-any-repo goal on
real large repos. Object count alone does not predict Tree cost
(cargo and gitoxide at ~180 K objects each differ by ~2× in anon);
pack *geometry* does.

### Upstream story

If any of these changes are clean enough and gate-able behind an opt-in,
they're candidates for upstream PRs. Gitoxide has shown willingness to
accept memory/perf-bounding work (the `MemoryCappedHashmap` itself is an
example). Fork exists because iteration in a fork is faster than a
multi-round upstream review; merging back is the explicit goal once the
design settles.

## Building

Same as upstream. `cargo check --workspace` succeeds cleanly. The
`mem-harness` binary can be built separately with `cargo build
--release -p mem-harness` and invoked as:

```
./target/release/mem-harness [--budget-mb N] <url> <dest>
```

It reports a single machine-parseable line with `peak_rss_mb`,
`peak_rss_anon_mb` (anonymous only, excludes mmap'd pack pages),
`budget_peak_mb` (high-water of the `MemoryBudget` counter),
`elapsed_s`, `on_disk_bytes`, and the binding `budget_mb`.

## License

Inherits gitoxide's dual MIT / Apache-2.0 license. See `LICENSE-MIT` and
`LICENSE-APACHE`.
