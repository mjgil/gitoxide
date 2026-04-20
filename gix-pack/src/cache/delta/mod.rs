/// Returned when using various methods on a [`Tree`]
#[derive(thiserror::Error, Debug)]
#[allow(missing_docs)]
pub enum Error {
    #[error("Pack offsets must only increment. The previous pack offset was {last_pack_offset}, the current one is {pack_offset}")]
    InvariantIncreasingPackOffset {
        /// The last seen pack offset
        last_pack_offset: crate::data::Offset,
        /// The invariant violating offset
        pack_offset: crate::data::Offset,
    },
    /// The tree's underlying disk-backed store (step 5.5-a's
    /// `ItemStoreBuilder` or step 5.5-b's `EdgeSpool`) failed an
    /// I/O operation — typically "no space left on device" or
    /// "permission denied on `$TMPDIR`". Hard failure; no
    /// fallback exists.
    ///
    /// Step 5.5-c of the bounded-memory plan. Before 5.5-c, the
    /// tree's state lived entirely in RAM and could not surface
    /// I/O errors.
    #[error("{message}")]
    SpoolIo {
        source: std::io::Error,
        message: &'static str,
    },
    /// An [`EdgeSpool`][super::edge_spool::EdgeSpool] reservation
    /// exceeded the shared
    /// [`MemoryBudget`][gix_features::budget::MemoryBudget].
    /// Callers retry with a wider budget or with
    /// [`MemoryBudget::unlimited`][gix_features::budget::MemoryBudget::unlimited].
    ///
    /// Step 5.5-c. Note: in practice `EdgeSpool::new` gracefully
    /// degrades to an unaccounted structural minimum instead of
    /// returning this variant — see its `with_budget`-style probe-
    /// and-halve contract. This variant remains defensible for
    /// future wirings that may have stricter budget semantics.
    #[error(transparent)]
    OutOfBudget(#[from] gix_features::budget::OutOfBudget),
}

///
pub mod traverse;

///
pub mod from_offsets;

/// Tree datastructure
// kept in separate module to encapsulate unsafety (it has field invariants)
mod tree;

/// Disk-backed replacement for `Vec<Item<T>>` (step 5.5-a, not yet
/// wired into [`Tree`]; see `STEP_5_5_DESIGN.md` at the repo root).
pub(crate) mod item_store;

/// External-sort-based edge resolution for delta-tree children
/// assembly (step 5.5-b, not yet wired into [`Tree`]; see
/// `STEP_5_5_DESIGN.md` at the repo root).
pub(crate) mod edge_spool;

pub use tree::Tree;
