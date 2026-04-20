//! A process- or repository-scoped memory budget.
//!
//! The goal is to let memory-hot gix operations (pack indexing, delta
//! resolution, object caches, pack receive buffers) *cooperate* with a
//! single shared byte cap, so that a caller can say "this clone must not
//! exceed 512 MiB" and get either a successful operation or a clean
//! [`OutOfBudget`] error — never an OOM kill.
//!
//! This module only provides the primitive. Nothing in `gix` or the other
//! `gix-*` crates consults it yet; threading the budget through
//! [`gix::open::Options`] / [`gix::clone::PrepareFetch`] and wiring it
//! into specific allocation sites are separate commits.
//!
//! # Shape
//!
//! A [`MemoryBudget`] wraps an [`Arc`] internally and is [`Clone`]; clones
//! share the same counter. Call [`MemoryBudget::reserve`] to ask for
//! bytes; on success you get a [`Reservation`] whose [`Drop`] impl
//! releases the bytes back. On failure you get [`OutOfBudget`], which
//! carries the requested amount and the amount currently available so
//! callers can make informed retry/fallback decisions.
//!
//! # Example
//!
//! ```ignore
//! use gix_features::budget::MemoryBudget;
//!
//! let budget = MemoryBudget::bytes(100 * 1024 * 1024); // 100 MiB
//!
//! let r1 = budget.reserve(60 * 1024 * 1024).expect("first fits");
//! let r2 = budget.reserve(30 * 1024 * 1024).expect("second fits");
//! // This one exceeds the cap — fails cleanly.
//! assert!(budget.reserve(20 * 1024 * 1024).is_err());
//!
//! drop(r1); // 60 MiB returned to the pool
//! let r3 = budget.reserve(20 * 1024 * 1024).expect("now fits again");
//! drop((r2, r3));
//! ```
//!
//! # Non-goals
//!
//! - This is not a general-purpose allocator. It does not track actual
//!   heap use, only the bytes callers have voluntarily declared via
//!   [`reserve`](MemoryBudget::reserve). Uninstrumented allocations
//!   bypass it entirely. The point is cooperative accounting of the
//!   *largest* allocations; smaller allocations are noise under them.
//! - It does not block. [`reserve`](MemoryBudget::reserve) is
//!   non-blocking and returns immediately, either with a [`Reservation`]
//!   or with [`OutOfBudget`]. Backpressure is the caller's problem.
//! - It is not an RAII allocator in the Rust sense. Dropping a
//!   [`Reservation`] does not free any actual memory; it only returns
//!   the *accounting* bytes. Actual allocations live and die with the
//!   data structures the caller built.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// A shared byte-budget tracker. Cloneable; clones share the same
/// underlying counter.
#[derive(Clone, Debug)]
pub struct MemoryBudget {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Hard upper bound in bytes. `u64::MAX` means "unlimited" — in that
    /// case `used + n` can never exceed the cap for any realistic `n`.
    cap: u64,
    /// Bytes currently reserved. Monotonically increases on `reserve`
    /// and decreases when a [`Reservation`] is dropped.
    used: AtomicU64,
    /// Highest value `used` ever held. Monotonically non-decreasing;
    /// never reset over the lifetime of the budget. Useful for post-
    /// hoc "how close did we come to the cap?" reporting without
    /// polling `used` from outside.
    ///
    /// Updated via [`AtomicU64::fetch_max`] after every successful
    /// [`MemoryBudget::reserve`], so the stored value always reflects
    /// the high-water mark of `used` across all threads. Reads are
    /// eventually consistent; the final value after all reservations
    /// have dropped is correct.
    peak_used: AtomicU64,
}

/// Returned when a reservation would exceed the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfBudget {
    /// Bytes that were requested.
    pub requested: u64,
    /// Bytes that were available at the moment the request was rejected.
    /// Note: this is a point-in-time snapshot; a concurrent release may
    /// have raised the true available number by the time the caller
    /// reads this field. Useful for log messages, not for control flow.
    pub available: u64,
}

impl std::fmt::Display for OutOfBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "memory budget exhausted: requested {} bytes, {} available",
            self.requested, self.available
        )
    }
}

impl std::error::Error for OutOfBudget {}

/// An active reservation against a [`MemoryBudget`]. Releases its bytes
/// back to the budget when dropped.
///
/// Holding a [`Reservation`] does not prevent the [`MemoryBudget`] from
/// being dropped: both hold [`Arc`]s to the same inner state, and the
/// last drop wins. Reservations from a dropped budget release harmlessly
/// into state no one else observes.
#[must_use = "dropping a Reservation immediately releases its bytes back to the budget"]
pub struct Reservation {
    inner: Arc<Inner>,
    bytes: u64,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl MemoryBudget {
    /// Create a budget with the given hard cap in bytes.
    pub fn bytes(cap: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                cap,
                used: AtomicU64::new(0),
                peak_used: AtomicU64::new(0),
            }),
        }
    }

    /// Create a budget with no effective cap. Every [`reserve`] call
    /// will succeed. Useful as the default when no budget was configured
    /// so that enforcement code can be written uniformly without
    /// branching on "is there a budget?".
    ///
    /// [`reserve`]: Self::reserve
    pub fn unlimited() -> Self {
        Self::bytes(u64::MAX)
    }

    /// Attempt to reserve `n` bytes. Returns a [`Reservation`] whose
    /// [`Drop`] releases them, or [`OutOfBudget`] if the reservation
    /// would exceed the cap.
    ///
    /// Uses a compare-exchange loop so concurrent callers cannot both
    /// observe "room to fit" and then both succeed past the cap.
    pub fn reserve(&self, n: usize) -> Result<Reservation, OutOfBudget> {
        let n = n as u64;
        let mut current = self.inner.used.load(Ordering::Acquire);
        loop {
            // Overflow-safe: if `current + n` would wrap, treat as
            // "not enough budget" rather than silently succeeding.
            let next = match current.checked_add(n) {
                Some(v) if v <= self.inner.cap => v,
                _ => {
                    return Err(OutOfBudget {
                        requested: n,
                        available: self.inner.cap.saturating_sub(current),
                    });
                }
            };
            match self.inner.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successful reservation. Bump the high-water
                    // mark if `next` beats the current peak. This is
                    // lock-free and wait-free: `fetch_max` is a
                    // single atomic instruction on every target we
                    // support, and the value is monotonically non-
                    // decreasing so racing updates converge to the
                    // correct maximum regardless of interleaving.
                    self.inner.peak_used.fetch_max(next, Ordering::AcqRel);
                    return Ok(Reservation {
                        inner: Arc::clone(&self.inner),
                        bytes: n,
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Report the configured cap. For `MemoryBudget::unlimited()`, this
    /// is [`u64::MAX`].
    pub fn cap(&self) -> u64 {
        self.inner.cap
    }

    /// Report bytes currently reserved. Snapshot; may be stale by the
    /// time the caller reads it.
    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Acquire)
    }

    /// Report bytes currently available for reservation. Snapshot.
    pub fn available(&self) -> u64 {
        self.inner.cap.saturating_sub(self.used())
    }

    /// Report the high-water mark of `used` since this budget was
    /// constructed. Monotonically non-decreasing across the budget's
    /// lifetime; never resets.
    ///
    /// Useful for post-operation observability: "how much of the cap
    /// did this operation actually touch?" A value well below `cap`
    /// means the budget never bound and the operation's memory was
    /// governed by something else (unbounded allocators, ambient
    /// runtime overhead, or simply having less data than the cap).
    /// A value equal to `cap` means the budget was the binding
    /// constraint at some point.
    pub fn peak_used(&self) -> u64 {
        self.inner.peak_used.load(Ordering::Acquire)
    }
}

impl Default for MemoryBudget {
    /// The default budget is [unlimited](Self::unlimited). This matches the
    /// semantics used by the existing gix call sites before this primitive
    /// was introduced — uninstrumented callers who don't care about memory
    /// caps continue to behave as before.
    fn default() -> Self {
        Self::unlimited()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Release is monotonic; no CAS needed. A concurrent reserver
        // racing with this release will observe the smaller used value
        // via its next compare_exchange_weak iteration.
        self.inner.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
