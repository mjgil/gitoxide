//! Unit tests for `gix_features::budget::MemoryBudget`.
//!
//! These exist because the budget is a load-bearing primitive for the
//! bounded-memory work in this fork, and it's exactly the kind of
//! lock-free code that's easy to get subtly wrong. Cover: basic success
//! path, exhaustion, release-on-drop, clone-shares-state, overflow
//! guard, concurrent reservations actually serialize through the CAS.

use gix_features::budget::{MemoryBudget, OutOfBudget};

mod budget {
    use super::*;

    #[test]
    fn unlimited_always_fits() {
        let b = MemoryBudget::unlimited();
        // Cap is u64::MAX; any realistic request succeeds.
        let _r1 = b.reserve(1_000_000_000).unwrap();
        let _r2 = b.reserve(1_000_000_000).unwrap();
        let _r3 = b.reserve(1_000_000_000).unwrap();
        assert_eq!(b.cap(), u64::MAX);
    }

    #[test]
    fn exact_fit_succeeds() {
        let b = MemoryBudget::bytes(100);
        let r = b.reserve(100).unwrap();
        assert_eq!(b.used(), 100);
        assert_eq!(b.available(), 0);
        drop(r);
        assert_eq!(b.used(), 0);
        assert_eq!(b.available(), 100);
    }

    #[test]
    fn overshoot_by_one_fails() {
        let b = MemoryBudget::bytes(100);
        let err = b.reserve(101).unwrap_err();
        assert_eq!(
            err,
            OutOfBudget {
                requested: 101,
                available: 100
            }
        );
        // Used is unchanged — failed reservation must not consume bytes.
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn drop_releases_bytes() {
        let b = MemoryBudget::bytes(100);
        let r1 = b.reserve(60).unwrap();
        let r2 = b.reserve(30).unwrap();
        assert_eq!(b.used(), 90);
        // Next reservation would exceed.
        assert!(b.reserve(20).is_err());

        drop(r1); // 60 back
        assert_eq!(b.used(), 30);

        let r3 = b.reserve(60).unwrap();
        drop((r2, r3));
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn clone_shares_counter() {
        let b = MemoryBudget::bytes(100);
        let b2 = b.clone();
        let _r = b.reserve(50).unwrap();
        // Clone sees the same used state.
        assert_eq!(b2.used(), 50);
        assert_eq!(b2.available(), 50);
        // And reservations via the clone apply to the same budget.
        let _r2 = b2.reserve(50).unwrap();
        assert_eq!(b.used(), 100);
        assert!(b.reserve(1).is_err());
    }

    #[test]
    fn reservation_outlives_budget_handle_without_panic() {
        // A Reservation holds its own Arc to the inner state. Dropping
        // the originating MemoryBudget first must not cause the
        // Reservation's Drop to misbehave.
        let r = {
            let b = MemoryBudget::bytes(100);
            b.reserve(50).unwrap()
        };
        drop(r); // should not panic, should not leak
    }

    #[test]
    fn overflow_request_is_rejected_not_wrapped() {
        // Cap is modest; request is u64::MAX so current + n would
        // overflow. Must be rejected, not silently wrap to a small
        // value that fits.
        let b = MemoryBudget::bytes(1_000);
        // usize::MAX on 64-bit systems is u64::MAX; on 32-bit it's
        // u32::MAX. Either way this exceeds the 1000-byte cap and
        // the arithmetic must not wrap.
        let err = b.reserve(usize::MAX).unwrap_err();
        assert_eq!(err.requested, usize::MAX as u64);
        assert_eq!(err.available, 1_000);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn zero_reservation_is_allowed() {
        // Defensive: a zero-byte reservation should succeed without
        // touching the counter. This can happen when callers instrument
        // sites that sometimes compute `n = 0` for edge cases.
        let b = MemoryBudget::bytes(0);
        let r = b.reserve(0).unwrap();
        assert_eq!(b.used(), 0);
        drop(r);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn concurrent_reservations_cannot_exceed_cap() {
        // Spawn many threads all trying to reserve over the cap in
        // aggregate. Sum of held reservations must never exceed cap at
        // any point — enforced by the CAS loop.
        use std::sync::Arc;
        use std::thread;

        const CAP: u64 = 10_000;
        const PER_RESERVE: usize = 100;
        const THREADS: usize = 16;
        const ATTEMPTS_PER_THREAD: usize = 1_000;

        let b = Arc::new(MemoryBudget::bytes(CAP));
        let succeeded = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let b = Arc::clone(&b);
                let succeeded = Arc::clone(&succeeded);
                thread::spawn(move || {
                    for _ in 0..ATTEMPTS_PER_THREAD {
                        if let Ok(r) = b.reserve(PER_RESERVE) {
                            succeeded.fetch_add(
                                1,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            // Hold briefly to create overlap, then release.
                            drop(r);
                        }
                        // Peek at used — must never exceed cap.
                        assert!(b.used() <= CAP, "used exceeded cap");
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // At this point all reservations have been dropped.
        assert_eq!(b.used(), 0);
        // We can't make a precise assertion about how many succeeded
        // (it depends on scheduling), but at least *some* should have.
        let s = succeeded.load(std::sync::atomic::Ordering::Relaxed);
        assert!(s > 0, "at least some reservations should succeed");
    }

    /// `peak_used` starts at 0 and never decreases, even after the
    /// reservations that caused the peak are dropped. This is the
    /// contract that lets the mem-harness report "how close did the
    /// operation come to the cap?" in its final summary line.
    #[test]
    fn peak_used_reflects_high_water_mark_and_does_not_decay() {
        let b = MemoryBudget::bytes(1_000);
        assert_eq!(b.peak_used(), 0, "fresh budget has zero peak");

        let r1 = b.reserve(100).unwrap();
        assert_eq!(b.peak_used(), 100);
        let r2 = b.reserve(200).unwrap();
        assert_eq!(b.peak_used(), 300);
        drop(r1);
        assert_eq!(
            b.peak_used(),
            300,
            "peak must persist after releases"
        );
        drop(r2);
        assert_eq!(
            b.used(),
            0,
            "all reservations released"
        );
        assert_eq!(
            b.peak_used(),
            300,
            "peak must still be 300 after every reservation is dropped"
        );

        // A subsequent smaller reservation cycle must not lower the
        // reported peak.
        let r3 = b.reserve(50).unwrap();
        drop(r3);
        assert_eq!(b.peak_used(), 300);

        // A larger one does push it up.
        let r4 = b.reserve(500).unwrap();
        assert_eq!(b.peak_used(), 500);
        drop(r4);
        assert_eq!(b.peak_used(), 500);
    }

    /// Peak is tracked even when the reservation eventually fails.
    /// The CAS loop only calls `fetch_max` on successful Ok returns,
    /// so a rejection must leave the peak untouched.
    #[test]
    fn peak_used_not_bumped_by_rejected_reservation() {
        let b = MemoryBudget::bytes(100);
        let r = b.reserve(60).unwrap();
        assert_eq!(b.peak_used(), 60);
        // This request would push used to 120, exceeding the cap.
        assert!(b.reserve(60).is_err());
        assert_eq!(
            b.peak_used(),
            60,
            "failed reservation must not move the peak"
        );
        drop(r);
        assert_eq!(b.peak_used(), 60);
    }

    /// Cloning the budget handle shares the same peak counter.
    #[test]
    fn peak_used_is_shared_across_clones() {
        let b = MemoryBudget::bytes(1_000);
        let c = b.clone();
        let r = c.reserve(500).unwrap();
        assert_eq!(b.peak_used(), 500, "original handle sees the peak");
        assert_eq!(c.peak_used(), 500, "clone handle sees the peak");
        drop(r);
        assert_eq!(b.peak_used(), 500);
        assert_eq!(c.peak_used(), 500);
    }
}
