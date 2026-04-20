use gix_features::budget::MemoryBudget;

use super::{
    edge_spool::{self, EdgeSpool},
    item_store::{ItemStore, ItemStoreBuilder},
    traverse, Error,
};

enum NodeKind {
    Root,
    Child,
}

/// A tree that allows one-time iteration over all nodes and their children, consuming it in the process,
/// while being shareable among threads without a lock.
/// It does this by making the guarantee that iteration only happens once.
///
/// # Storage shape
///
/// One disk-backed [`ItemStoreBuilder`] collects every item
/// (root or child) in the order they were pushed — which, by
/// `assert_is_incrementing`'s invariant,
/// means strict-increasing pack offset. One [`EdgeSpool`]
/// collects `(base_offset, child_global_idx)` edges where
/// `child_global_idx` is the push index within the single
/// builder. A parallel `is_root: Vec<bool>` marks each global
/// idx as root or child.
///
/// Post-5.6-b the tree carries no per-item data (`T` generic
/// removed). The store holds only mmap-backed `(offset,
/// next_offset, children)` metadata; the ~80 MiB data array
/// that pre-5.6-b allocated for rust-lang/rust is retired.
/// Consumers receive per-object context through the merged
/// `sink` callback in `Tree::traverse`.
pub struct Tree {
    builder: Option<ItemStoreBuilder>,
    edges: Option<EdgeSpool>,
    is_root: Vec<bool>,
    last_seen: Option<NodeKind>,
    last_pushed_idx: Option<u32>,
    last_pushed_offset: Option<u64>,
    store: Option<ItemStore>,
}

impl Tree {
    pub fn with_capacity(num_objects: usize, memory_budget: MemoryBudget) -> Result<Self, Error> {
        let builder = ItemStoreBuilder::new(num_objects).map_err(|err| Error::SpoolIo {
            source: err,
            message: "create items tempfile",
        })?;
        let edges = EdgeSpool::new(num_objects, memory_budget).map_err(|err| Error::SpoolIo {
            source: err,
            message: "create edge-spool tempfile",
        })?;
        let mut is_root = Vec::new();
        let _ = is_root.try_reserve_exact(num_objects);
        Ok(Tree {
            builder: Some(builder),
            edges: Some(edges),
            is_root,
            last_seen: None,
            last_pushed_idx: None,
            last_pushed_offset: None,
            store: None,
        })
    }

    pub fn with_capacity_unlimited(num_objects: usize) -> Result<Self, Error> {
        Self::with_capacity(num_objects, MemoryBudget::unlimited())
    }

    pub(super) fn num_items(&self) -> usize {
        self.is_root.len()
    }

    pub(super) fn take_store_and_is_root(mut self) -> (ItemStore, Vec<bool>) {
        let store = self
            .store
            .take()
            .expect("set_pack_entries_end_and_resolve_ref_offsets must run before take_store_and_is_root");
        let is_root = std::mem::take(&mut self.is_root);
        debug_assert_eq!(is_root.len(), store.num_items() as usize);
        (store, is_root)
    }

    pub(super) fn assert_is_incrementing(
        &self,
        offset: crate::data::Offset,
    ) -> Result<(), Error> {
        if let Some(last_offset) = self.last_pushed_offset {
            if offset <= last_offset {
                return Err(Error::InvariantIncreasingPackOffset {
                    last_pack_offset: last_offset,
                    pack_offset: offset,
                });
            }
        }
        Ok(())
    }

    pub(super) fn set_pack_entries_end_and_resolve_ref_offsets(
        &mut self,
        pack_entries_end: crate::data::Offset,
    ) -> Result<(), traverse::Error> {
        self.assert_is_incrementing(pack_entries_end)
            .expect("BUG: pack_entries_end is smaller than all previously seen entries");

        let edges = self.edges.take().expect("edges present until resolve");
        let builder = self
            .builder
            .as_mut()
            .expect("builder present during resolve");
        let metadata = builder.interim_metadata().map_err(traverse::Error::SpoolIo)?;

        let resolved = edges.resolve(metadata).map_err(|err| match err {
            edge_spool::Error::SpoolIo(io_err) => traverse::Error::SpoolIo(io_err),
            edge_spool::Error::OutOfBudget(e) => traverse::Error::OutOfBudget(e),
            edge_spool::Error::UnresolvedBaseOffset { .. } => {
                unreachable!("EdgeSpool::resolve setup cannot fail with UnresolvedBaseOffset")
            }
        })?;

        let mut pairs: Vec<(u32, u32)> = Vec::new();
        for pair in resolved {
            match pair {
                Ok(p) => pairs.push(p),
                Err(edge_spool::Error::UnresolvedBaseOffset { base_offset }) => {
                    return Err(traverse::Error::OutOfPackRefDelta {
                        base_pack_offset: base_offset,
                    });
                }
                Err(edge_spool::Error::SpoolIo(err)) => return Err(traverse::Error::SpoolIo(err)),
                Err(edge_spool::Error::OutOfBudget(e)) => return Err(traverse::Error::OutOfBudget(e)),
            }
        }

        let builder = self.builder.take().expect("builder present during resolve");
        let store = builder
            .finish(pairs, pack_entries_end)
            .map_err(traverse::Error::SpoolIo)?;
        self.store = Some(store);
        Ok(())
    }

    /// Add a new root node at the given pack `offset`.
    pub fn add_root(&mut self, offset: crate::data::Offset) -> Result<(), Error> {
        self.assert_is_incrementing(offset)?;
        let builder = self
            .builder
            .as_mut()
            .expect("builder present during construction");
        let global_idx = builder.push(offset).map_err(|err| Error::SpoolIo {
            source: err,
            message: "push root item",
        })?;
        self.is_root.push(true);
        self.last_seen = Some(NodeKind::Root);
        self.last_pushed_idx = Some(global_idx);
        self.last_pushed_offset = Some(offset);
        Ok(())
    }

    /// Add a child of the item at `base_offset` which itself
    /// resides at pack `offset`.
    pub fn add_child(
        &mut self,
        base_offset: crate::data::Offset,
        offset: crate::data::Offset,
    ) -> Result<(), Error> {
        self.assert_is_incrementing(offset)?;
        let builder = self
            .builder
            .as_mut()
            .expect("builder present during construction");
        let child_global_idx = builder.push(offset).map_err(|err| Error::SpoolIo {
            source: err,
            message: "push child item",
        })?;
        self.is_root.push(false);
        self.last_pushed_offset = Some(offset);
        self.edges
            .as_mut()
            .expect("edges present during construction")
            .push(base_offset, child_global_idx)
            .map_err(|err| match err {
                edge_spool::Error::SpoolIo(io_err) => Error::SpoolIo {
                    source: io_err,
                    message: "record delta edge",
                },
                edge_spool::Error::OutOfBudget(e) => Error::OutOfBudget(e),
                edge_spool::Error::UnresolvedBaseOffset { .. } => {
                    unreachable!("EdgeSpool::push cannot fail with UnresolvedBaseOffset")
                }
            })?;
        self.last_seen = Some(NodeKind::Child);
        self.last_pushed_idx = Some(child_global_idx);
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    mod from_offsets_in_pack {
        use std::sync::atomic::AtomicBool;

        use crate as pack;

        const SMALL_PACK_INDEX: &str = "objects/pack/pack-a2bf8e71d8c18879e499335762dd95119d93d9f1.idx";
        const SMALL_PACK: &str = "objects/pack/pack-a2bf8e71d8c18879e499335762dd95119d93d9f1.pack";

        const INDEX_V1: &str = "objects/pack/pack-c0438c19fb16422b6bbcce24387b3264416d485b.idx";
        const PACK_FOR_INDEX_V1: &str = "objects/pack/pack-c0438c19fb16422b6bbcce24387b3264416d485b.pack";

        use gix_testtools::fixture_path;

        #[test]
        fn v1() -> Result<(), Box<dyn std::error::Error>> {
            tree(INDEX_V1, PACK_FOR_INDEX_V1)
        }

        #[test]
        fn v2() -> Result<(), Box<dyn std::error::Error>> {
            tree(SMALL_PACK_INDEX, SMALL_PACK)
        }

        fn tree(index_path: &str, pack_path: &str) -> Result<(), Box<dyn std::error::Error>> {
            let idx = pack::index::File::at(fixture_path(index_path), gix_hash::Kind::Sha1)?;
            crate::cache::delta::Tree::from_offsets_in_pack(
                &fixture_path(pack_path),
                idx.sorted_offsets().into_iter(),
                &|ofs| *ofs,
                &|id| idx.lookup(id).map(|index| idx.pack_offset_at_index(index)),
                &mut gix_features::progress::Discard,
                &AtomicBool::new(false),
                gix_hash::Kind::Sha1,
            )?;
            Ok(())
        }
    }
}
