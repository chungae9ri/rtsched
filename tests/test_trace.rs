use std::sync::Mutex;
use std::vec::Vec;

use rtsched::test_support;
use rtsched::{
    TraceEvent, TraceThread, clear_trace_fn, reset_trace_counters, set_trace_fn, trace_counters,
};

#[cfg(feature = "sched-isr-timing")]
use rtsched::{
    reset_sched_tick_to_pendsv_max_ticks, reset_sched_tick_to_pendsv_min_max_ticks,
    reset_sched_tick_to_pendsv_min_ticks, sched_tick_to_pendsv_timing,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static EVENTS: Mutex<Vec<TraceEvent>> = Mutex::new(Vec::new());

fn capture_event(event: TraceEvent) {
    EVENTS.lock().unwrap().push(event);
}

#[test]
fn trace_records_counters_and_callback_events() {
    let _guard = TEST_LOCK.lock().unwrap();
    let from = test_support::trace_thread(1, "from", true);
    let to = test_support::trace_thread(2, "to", false);

    EVENTS.lock().unwrap().clear();
    reset_trace_counters();
    set_trace_fn(capture_event);

    test_support::record_context_switch_for_test(&from, &to);
    test_support::record_deadline_miss_for_test(&to, 37, 40);
    test_support::record_wakeup_for_test(&from);
    test_support::record_yield_for_test(&to, 5);

    clear_trace_fn();

    let counters = trace_counters();
    assert!(counters.context_switches >= 1);
    assert!(counters.deadline_misses >= 1);
    assert!(counters.wakeups >= 1);
    assert!(counters.yields >= 1);

    let from = TraceThread {
        id: 1,
        name: "from",
        is_cfs: true,
    };
    let to = TraceThread {
        id: 2,
        name: "to",
        is_cfs: false,
    };
    let events = EVENTS.lock().unwrap();
    assert!(events.contains(&TraceEvent::ContextSwitch {
        from: Some(from),
        to
    }));
    assert!(events.contains(&TraceEvent::DeadlineMiss {
        thread: to,
        runtime_ticks: 37,
        relative_deadline_ticks: 40,
    }));
    assert!(events.contains(&TraceEvent::Wakeup { thread: from }));
    assert!(events.contains(&TraceEvent::Yield {
        thread: to,
        elapsed_ticks: 5,
    }));
}

#[cfg(feature = "sched-isr-timing")]
#[test]
fn reset_sched_tick_to_pendsv_max_ticks_clears_only_total_max() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        test_support::set_sched_tick_timing_for_test(12, 23, 34, 56, 78, 90);
    }

    reset_sched_tick_to_pendsv_max_ticks();

    let timing = sched_tick_to_pendsv_timing();
    assert_eq!(timing.last_ticks, 12);
    assert_eq!(timing.min_ticks, 23);
    assert_eq!(timing.max_ticks, 0);
    assert_eq!(timing.samples, 56);
    assert_eq!(timing.advance_ktimers_max_ticks, 78);
    assert_eq!(timing.dispatch_expired_ktimer_max_ticks, 90);
}

#[cfg(feature = "sched-isr-timing")]
#[test]
fn reset_sched_tick_to_pendsv_min_ticks_clears_only_total_min() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        test_support::set_sched_tick_timing_for_test(12, 23, 34, 56, 78, 90);
    }

    reset_sched_tick_to_pendsv_min_ticks();

    let timing = sched_tick_to_pendsv_timing();
    assert_eq!(timing.last_ticks, 12);
    assert_eq!(timing.min_ticks, 0);
    assert_eq!(timing.max_ticks, 34);
    assert_eq!(timing.samples, 56);
    assert_eq!(timing.advance_ktimers_max_ticks, 78);
    assert_eq!(timing.dispatch_expired_ktimer_max_ticks, 90);
}

#[cfg(feature = "sched-isr-timing")]
#[test]
fn reset_sched_tick_to_pendsv_min_max_ticks_clears_total_extrema() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        test_support::set_sched_tick_timing_for_test(12, 23, 34, 56, 78, 90);
    }

    reset_sched_tick_to_pendsv_min_max_ticks();

    let timing = sched_tick_to_pendsv_timing();
    assert_eq!(timing.last_ticks, 12);
    assert_eq!(timing.min_ticks, 0);
    assert_eq!(timing.max_ticks, 0);
    assert_eq!(timing.samples, 56);
    assert_eq!(timing.advance_ktimers_max_ticks, 78);
    assert_eq!(timing.dispatch_expired_ktimer_max_ticks, 90);
}
