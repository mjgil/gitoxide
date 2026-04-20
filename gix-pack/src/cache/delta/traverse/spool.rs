//! Spill-to-disk backing for the delta-chain cache used during pack
//! index construction.
//!
//! # Why this exists
//!
//! Step 5.2 of the bounded-memory plan made the delta-chain cache
//! (`decompressed_bytes_by_pack_offset` in [`super::resolve`]) reserve
//! bytes against the shared [`MemoryBudget`][gix_features::budget::MemoryBudget]
//! before inserting each cached intermediate-delta. When the budget
//! was exhausted the whole traversal aborted with
//! [`Error::OutOfBudget`][super::Error::OutOfBudget]. That prevented
//! OOM kills, but it also meant a tight budget plus a real-world
//! (honest, not hostile) pack would fail the clone.
//!
//! Step 5.3 replaces the abort with this spool: when the budget
//! can't accommodate a cached entry's bytes, the bytes are written
//! to an anonymous tempfile instead and the cache entry records
//! `(offset, len)` within the spool rather than holding the
//! `Vec<u8>` in RAM. On lookup, the bytes are read back from disk.
//! Peak RAM stays near the cap; peak disk grows with the pack's
//! delta geometry; the operation completes.
//!
//! # Lifecycle and cleanup
//!
//! The spool is created lazily on first spill via
//! [`SpoolHandle::get_or_create`]. Unlimited-budget traversals — the
//! default everywhere — never call reserve-that-fails and therefore
//! never touch disk: this module's cost is one `Mutex<Option<_>>`
//! field per [`super::resolve::State`] in that case.
//!
//! The underlying file is opened via [`tempfile::tempfile`], which on
//! Linux returns an `O_TMPFILE` inode (create-without-name) and
//! elsewhere falls back to create+unlink. Either way, no path is
//! visible in the filesystem namespace and the inode is reclaimed by
//! the kernel on process exit — including `SIGKILL` or panic — not
//! just on orderly [`Drop`].
//!
//! # Concurrency
//!
//! [`SpoolFile`] uses interior mutability ([`std::sync::Mutex`]) so
//! many workers can safely share the same spool across
//! [`super::resolve::deltas_mt`]. Contention is expected to be low:
//! spills happen only under budget pressure, and real workloads
//! that hit the cap do so on a small fraction of entries. The lock
//! is held only for the syscall pair `seek`+`write_all` (append) or
//! `seek`+`read_exact` (lookup); no decoding or allocation happens
//! under the lock beyond allocating the read buffer.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

/// An append-only, random-access scratch file used by the delta-chain
/// cache for bytes that don't fit in the in-RAM budget. Constructed on
/// demand through [`SpoolHandle::get_or_create`]; never exposed to
/// callers outside [`super::resolve`].
///
/// The file is owned (via [`tempfile::tempfile`]) and closed when the
/// last [`Arc`] to the `SpoolFile` is dropped. On Linux the
/// underlying inode has already been unlinked at construction time;
/// close simply releases the last reference and the kernel reclaims
/// the blocks.
pub(crate) struct SpoolFile {
    inner: Mutex<SpoolInner>,
}

struct SpoolInner {
    file: File,
    /// Current append position, in bytes from the start of the file.
    /// Monotonically increasing — the spool is never truncated or
    /// rewritten during a traversal. Read-back always lands inside
    /// `[0, next_offset)` because cache entries remember their own
    /// offset and length at insert time.
    next_offset: u64,
}

impl SpoolFile {
    /// Open a fresh anonymous tempfile. Fails if the OS cannot
    /// create one — typically "disk full" or "permission denied on
    /// `$TMPDIR`". This error propagates through
    /// [`super::resolve::deltas`] as
    /// [`super::Error::SpoolIo`][super::Error::SpoolIo].
    pub(crate) fn new() -> io::Result<Self> {
        let file = tempfile::tempfile()?;
        Ok(Self {
            inner: Mutex::new(SpoolInner { file, next_offset: 0 }),
        })
    }

    /// Append `data` to the spool and return the byte offset at which
    /// it begins. The returned `(offset, data.len())` pair is the
    /// coordinate the cache entry must remember in order to recover
    /// the bytes later via [`Self::read_exact`].
    pub(crate) fn append(&self, data: &[u8]) -> io::Result<u64> {
        let mut inner = self.inner.lock().expect("spool file mutex poisoned");
        let offset = inner.next_offset;
        inner.file.seek(SeekFrom::Start(offset))?;
        inner.file.write_all(data)?;
        inner.next_offset = offset
            .checked_add(data.len() as u64)
            .expect("spool file offset overflowed u64 — pack is impossibly large");
        Ok(offset)
    }

    /// Read `len` bytes starting at `offset`. Both parameters come
    /// from a prior successful [`Self::append`] call and are
    /// guaranteed to lie within the spool.
    pub(crate) fn read_exact(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut inner = self.inner.lock().expect("spool file mutex poisoned");
        inner.file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        inner.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Total bytes written to the spool so far. Useful for
    /// observability and for tests that want to assert spilling
    /// actually occurred (or didn't).
    #[allow(dead_code)] // used by tests and reserved for future diagnostics
    pub(crate) fn bytes_written(&self) -> u64 {
        self.inner
            .lock()
            .expect("spool file mutex poisoned")
            .next_offset
    }
}

/// Lazy, shared owner of the spool. Each worker thread in
/// [`super::resolve::deltas_mt`] holds an [`Arc<SpoolHandle>`] via its
/// [`super::resolve::State`]; the underlying [`SpoolFile`] is opened
/// once on first spill and reused from then on. Under
/// [`MemoryBudget::unlimited`][gix_features::budget::MemoryBudget::unlimited]
/// no spill ever happens and the inner `Option` stays `None` for the
/// entire traversal — the handle costs exactly one lock and one empty
/// `Option` per worker.
pub(crate) struct SpoolHandle {
    inner: Mutex<Option<Arc<SpoolFile>>>,
}

impl SpoolHandle {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Return a reference-counted handle to the spool, creating the
    /// underlying tempfile on first call. Thread-safe; the creation
    /// is serialized and subsequent callers see the already-created
    /// handle.
    pub(crate) fn get_or_create(&self) -> io::Result<Arc<SpoolFile>> {
        let mut guard = self.inner.lock().expect("spool handle mutex poisoned");
        if let Some(ref spool) = *guard {
            return Ok(Arc::clone(spool));
        }
        let spool = Arc::new(SpoolFile::new()?);
        *guard = Some(Arc::clone(&spool));
        Ok(spool)
    }

    /// Total bytes written to the spool so far, or 0 if the spool was
    /// never created (i.e. the budget accommodated every cached
    /// entry).
    #[allow(dead_code)] // used by tests and reserved for future diagnostics
    pub(crate) fn bytes_written(&self) -> u64 {
        self.inner
            .lock()
            .expect("spool handle mutex poisoned")
            .as_ref()
            .map(|s| s.bytes_written())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read_roundtrip_single_thread() {
        let spool = SpoolFile::new().unwrap();
        let a = spool.append(b"hello").unwrap();
        let b = spool.append(b", ").unwrap();
        let c = spool.append(b"world").unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, 5);
        assert_eq!(c, 7);
        assert_eq!(spool.read_exact(a, 5).unwrap(), b"hello");
        assert_eq!(spool.read_exact(c, 5).unwrap(), b"world");
        assert_eq!(spool.read_exact(b, 2).unwrap(), b", ");
        assert_eq!(spool.bytes_written(), 12);
    }

    #[test]
    fn handle_is_lazy_until_first_create() {
        let h = SpoolHandle::new();
        assert_eq!(h.bytes_written(), 0, "no file yet, nothing written");
        let s1 = h.get_or_create().unwrap();
        let s2 = h.get_or_create().unwrap();
        assert!(
            Arc::ptr_eq(&s1, &s2),
            "SpoolHandle must return the same SpoolFile across calls"
        );
        s1.append(b"x").unwrap();
        assert_eq!(h.bytes_written(), 1, "handle reflects underlying file state");
    }

    #[test]
    fn concurrent_append_and_read_is_consistent() {
        use std::thread;
        let spool = Arc::new(SpoolFile::new().unwrap());
        let mut handles = Vec::new();
        for tid in 0u8..8 {
            let spool = spool.clone();
            handles.push(thread::spawn(move || {
                let mut records = Vec::new();
                for i in 0..100u32 {
                    let payload = format!("t{tid}-i{i:03}").into_bytes();
                    let offset = spool.append(&payload).unwrap();
                    records.push((offset, payload));
                }
                for (offset, expected) in records {
                    let got = spool.read_exact(offset, expected.len()).unwrap();
                    assert_eq!(got, expected, "roundtrip mismatch");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
