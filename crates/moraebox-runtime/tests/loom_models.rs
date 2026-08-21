use loom::{
    model,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

#[test]
fn session_lease_never_has_two_writers() {
    model(|| {
        let leased = Arc::new(AtomicBool::new(false));
        let active_writers = Arc::new(AtomicUsize::new(0));
        let acquisitions = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();

        for _ in 0..2 {
            let leased = Arc::clone(&leased);
            let active_writers = Arc::clone(&active_writers);
            let acquisitions = Arc::clone(&acquisitions);
            workers.push(thread::spawn(move || {
                if leased
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    assert_eq!(active_writers.fetch_add(1, Ordering::AcqRel), 0);
                    acquisitions.fetch_add(1, Ordering::Relaxed);
                    thread::yield_now();
                    assert_eq!(active_writers.fetch_sub(1, Ordering::AcqRel), 1);
                    leased.store(false, Ordering::Release);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(active_writers.load(Ordering::Acquire), 0);
        assert!(acquisitions.load(Ordering::Acquire) >= 1);
    });
}

#[test]
fn prepared_slot_is_consumed_at_most_once() {
    model(|| {
        const READY: usize = 0;
        const LEASED: usize = 1;
        const CONSUMED: usize = 2;

        let slot = Arc::new(AtomicUsize::new(READY));
        let consumers = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let slot = Arc::clone(&slot);
            let consumers = Arc::clone(&consumers);
            workers.push(thread::spawn(move || {
                if slot
                    .compare_exchange(READY, LEASED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    consumers.fetch_add(1, Ordering::Relaxed);
                    slot.store(CONSUMED, Ordering::Release);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(slot.load(Ordering::Acquire), CONSUMED);
        assert_eq!(consumers.load(Ordering::Acquire), 1);
    });
}
