use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use rtsched::test_support;

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn host_critical_section_allows_nested_calls() {
    let _guard = TEST_LOCK.lock().unwrap();

    let value = test_support::critical_section(|| test_support::critical_section(|| 42));

    assert_eq!(value, 42);
}

#[test]
fn host_critical_section_serializes_parallel_threads() {
    let _guard = TEST_LOCK.lock().unwrap();

    let active = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    for _ in 0..4 {
        let active = Arc::clone(&active);
        let overlaps = Arc::clone(&overlaps);
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                test_support::critical_section(|| {
                    if active.swap(true, Ordering::SeqCst) {
                        overlaps.fetch_add(1, Ordering::SeqCst);
                    }
                    thread::yield_now();
                    active.store(false, Ordering::SeqCst);
                });
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(overlaps.load(Ordering::SeqCst), 0);
}
