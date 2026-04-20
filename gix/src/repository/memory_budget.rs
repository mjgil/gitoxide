//! Accessors for the `MemoryBudget` configured on a [`Repository`] or
//! [`ThreadSafeRepository`].
//!
//! The budget is set via [`crate::open::Options::with_memory_budget`] (or
//! [`crate::open::Options::default()`], which yields an unlimited budget).
//! It is stored on the `Options` carried by each repository handle and
//! reachable through the accessors defined here.
//!
//! These accessors are load-bearing as of the commits wiring the
//! decoded-object caches (`gix_pack::cache::lru::MemoryCappedHashmap`,
//! `gix_pack::cache::lru::StaticLinkedList`, and
//! `gix_pack::cache::object::MemoryCappedHashmap`), the pack-index
//! build path (`gix_pack::bundle::write::Options::memory_budget`), and
//! the delta-chain cache inside the traversal (as of step 5.2) to the
//! shared budget. Future commits extend the set of consumers; the
//! accessor surface here stays stable.

use gix_features::budget::MemoryBudget;

impl crate::Repository {
    /// Return a reference to this repository's configured
    /// [`MemoryBudget`]. If none was configured via
    /// [`crate::open::Options::with_memory_budget`], this returns the
    /// default of [`MemoryBudget::unlimited`], so callers can uniformly
    /// call [`MemoryBudget::reserve`] without branching on "is a budget
    /// even set?".
    pub fn memory_budget(&self) -> &MemoryBudget {
        &self.options.memory_budget
    }
}

impl crate::ThreadSafeRepository {
    /// See [`crate::Repository::memory_budget`].
    pub fn memory_budget(&self) -> &MemoryBudget {
        &self.linked_worktree_options.memory_budget
    }
}
