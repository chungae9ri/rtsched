use rtsched::test_support::waitq;

#[test]
fn wait_entities_order_by_ticks_then_event() {
    waitq::wait_entities_order_by_ticks_then_event();
}

#[test]
fn remaining_time_uses_absolute_wake_deadline() {
    waitq::remaining_time_uses_absolute_wake_deadline();
}

#[test]
fn pop_expired_wait_thread_only_pops_zero_tick_threads() {
    waitq::pop_expired_wait_thread_only_pops_zero_tick_threads();
}

#[test]
fn remove_wait_thread_detaches_entity() {
    waitq::remove_wait_thread_detaches_entity();
}
