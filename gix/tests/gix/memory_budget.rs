//! Round-trip tests for the `MemoryBudget` plumbing and enforcement.
//!
//! The first three tests (added in step 3 of the bounded-memory plan)
//! verify that a budget configured via `open::Options::with_memory_budget`
//! survives onto the resulting `Repository` and is reachable through
//! `Repository::memory_budget()` and the matching accessor on
//! `ThreadSafeRepository`. They cover plumbing only.
//!
//! The fourth test (added in step 4) verifies the first *enforcement*
//! site: populating the object cache via the public `object_cache_size`
//! API actually consumes bytes from the repository's shared budget. If
//! someone in a later step rewires `object_cache_size` or
//! `setup_objects` and forgets to pass the budget into the new
//! `MemoryCappedHashmap::with_memory_budget` constructor, this test
//! fails fast.

use gix::budget::MemoryBudget;
use gix_testtools::tempfile;

#[test]
fn default_options_yield_unlimited_budget() -> crate::Result {
    let tmp = tempfile::tempdir()?;
    let repo = gix::init_bare(tmp.path())?;
    assert_eq!(
        repo.memory_budget().cap(),
        u64::MAX,
        "default budget must be unlimited so uninstrumented callers see no change"
    );
    Ok(())
}

#[test]
fn configured_budget_survives_to_repository() -> crate::Result {
    let tmp = tempfile::tempdir()?;
    let git_dir = tmp.path().join("bare.git");

    // First create a bare repo the normal way, then re-open it with a
    // configured budget. `init_bare` uses default options internally;
    // `open_opts` is where a user attaches the budget.
    gix::init_bare(&git_dir)?;

    let budget = MemoryBudget::bytes(64 * 1024 * 1024);
    let opts = gix::open::Options::isolated().with_memory_budget(budget);

    let repo = gix::ThreadSafeRepository::open_opts(&git_dir, opts)?;
    assert_eq!(
        repo.memory_budget().cap(),
        64 * 1024 * 1024,
        "configured budget must round-trip through open_opts to ThreadSafeRepository"
    );

    // And through to the thread-local Repository handle.
    let repo = repo.to_thread_local();
    assert_eq!(
        repo.memory_budget().cap(),
        64 * 1024 * 1024,
        "budget must also survive to_thread_local"
    );

    Ok(())
}

#[test]
fn clone_of_repository_handle_shares_budget_counter() -> crate::Result {
    // MemoryBudget is Arc-backed and Clone, so cloning the Repository
    // (which clones its internal options) should still give a handle
    // pointing at the same shared counter. This matters because step 4+
    // code paths may clone the Repository across threads.
    let tmp = tempfile::tempdir()?;
    let git_dir = tmp.path().join("bare.git");
    gix::init_bare(&git_dir)?;

    let budget = MemoryBudget::bytes(100);
    let opts = gix::open::Options::isolated().with_memory_budget(budget.clone());
    let repo = gix::ThreadSafeRepository::open_opts(&git_dir, opts)?.to_thread_local();

    // Reserve via the budget handle we still own.
    let _r = budget.reserve(40).expect("fits");

    // The Repository's budget must see the same 40 bytes as used.
    assert_eq!(
        repo.memory_budget().used(),
        40,
        "Repository budget and externally held MemoryBudget handle must share state"
    );
    Ok(())
}

#[test]
fn object_cache_populated_via_public_api_consumes_budget() -> crate::Result {
    // End-to-end enforcement check for step 4: opening a Repository with a
    // configured MemoryBudget, installing an object cache via the public
    // `object_cache_size` setter, and then reading an object through the
    // normal `find_object` path must cause the repository's shared budget
    // to report non-zero `used()`. A failure here means either:
    //   (a) `object_cache_size` stopped propagating `self.options.
    //       memory_budget` into its closure, or
    //   (b) `MemoryCappedHashmap::with_memory_budget` stopped accounting
    //       puts against the budget it was given.
    let tmp = tempfile::tempdir()?;
    let git_dir = tmp.path().join("bare.git");
    gix::init_bare(&git_dir)?;

    // Budget comfortably larger than any single blob we'll write, so we
    // expect the put to succeed and the counter to grow (not stay at 0
    // because of a budget rejection).
    let budget_cap = 1024 * 1024u64;
    let opts = gix::open::Options::isolated().with_memory_budget(MemoryBudget::bytes(budget_cap));
    let mut repo = gix::open_opts(&git_dir, opts)?;

    // Install a budget-aware object cache. Object cache defaults to
    // unset on a fresh repo; this setter exercises the exact wiring
    // `gix/src/repository/cache.rs::object_cache_size` was updated for.
    repo.object_cache_size(Some(64 * 1024));

    assert_eq!(
        repo.memory_budget().used(),
        0,
        "nothing in the cache yet, so no bytes reserved against the budget"
    );

    // Write a blob, then read it back. The read path decodes through the
    // ODB handle, which populates the object cache we just installed.
    let payload = vec![0xABu8; 256];
    let oid = repo.write_blob(&payload)?;

    // Force a find through the object-cache code path. `find_object`
    // consults the pack-object cache on every miss/hit, which for a
    // loose-object read here will result in a `put` on the configured
    // `MemoryCappedHashmap`.
    let _obj = repo.find_object(oid)?;

    let used = repo.memory_budget().used();
    assert!(
        used > 0,
        "populating the object cache via the public API must consume budget; got {} used",
        used
    );
    assert!(
        used <= budget_cap,
        "used ({}) must not exceed the configured cap ({})",
        used,
        budget_cap
    );

    // Note: we don't assert the drop path here. `_obj` borrows `repo`
    // for the rest of the function, so dropping `repo` early would
    // conflict with the borrow; and a Repository-level drop assertion
    // would only re-prove what the per-cache
    // `dropping_cache_releases_all_reservations` gix-pack unit tests
    // already cover at the primitive level.
    Ok(())
}
