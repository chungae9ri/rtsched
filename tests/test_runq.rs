use rtsched::test_support::runq;

#[test]
fn sched_entities_order_by_vruntime() {
    runq::sched_entities_order_by_vruntime();
}

#[test]
fn enqueue_thread_updates_priority_sum_and_traversal_order() {
    runq::enqueue_thread_updates_priority_sum_and_traversal_order();
}

#[test]
fn lower_numeric_priority_accumulates_vruntime_more_slowly() {
    runq::lower_numeric_priority_accumulates_vruntime_more_slowly();
}

#[test]
fn traverse_run_queue_includes_running_cfs_thread_first() {
    runq::traverse_run_queue_includes_running_cfs_thread_first();
}

#[test]
fn dequeue_thread_removes_ready_thread_and_saturates_priority_sum() {
    runq::dequeue_thread_removes_ready_thread_and_saturates_priority_sum();
}

#[test]
fn dequeue_runq_to_waitq_moves_thread_between_queues() {
    runq::dequeue_runq_to_waitq_moves_thread_between_queues();
}

#[test]
fn update_from_leftmost_aligns_detached_entity() {
    runq::update_from_leftmost_aligns_detached_entity();
}
