use rtsched::test_support::ktimer;

#[test]
fn reload_from_ticks_converts_to_scheduler_timer_reload() {
    ktimer::reload_from_ticks_converts_to_scheduler_timer_reload();
}

#[test]
fn writable_reload_clamps_raw_zero_to_minimum_reload() {
    ktimer::writable_reload_clamps_raw_zero_to_minimum_reload();
}

#[test]
fn insert_orders_timers_by_deadline() {
    ktimer::insert_orders_timers_by_deadline();
}

#[test]
fn equal_deadlines_keep_strict_entity_ordering() {
    ktimer::equal_deadlines_keep_strict_entity_ordering();
}

#[test]
#[should_panic(expected = "entity is already linked into this tree")]
fn ktimer_queue_rejects_duplicate_timer_insertions() {
    ktimer::ktimer_queue_rejects_duplicate_timer_insertions();
}

#[test]
fn advance_time_updates_queue_clock_without_rewriting_deadlines() {
    ktimer::advance_time_updates_queue_clock_without_rewriting_deadlines();
}

#[test]
fn next_reload_clamps_immediate_deadlines_to_minimum_reload() {
    ktimer::next_reload_clamps_immediate_deadlines_to_minimum_reload();
}

#[test]
fn next_reload_clamps_far_deadlines_to_maximum_reload() {
    ktimer::next_reload_clamps_far_deadlines_to_maximum_reload();
}

#[test]
fn cfs_reload_honors_boosted_deadline_before_execution_slice() {
    ktimer::cfs_reload_honors_boosted_deadline_before_execution_slice();
}

#[test]
fn long_rt_deadline_is_dispatched_after_multiple_scheduler_timer_chunks() {
    ktimer::long_rt_deadline_is_dispatched_after_multiple_scheduler_timer_chunks();
}

#[test]
fn first_active_skips_inactive_timers() {
    ktimer::first_active_skips_inactive_timers();
}

#[test]
fn active_timers_remain_sorted_by_absolute_deadline() {
    ktimer::active_timers_remain_sorted_by_absolute_deadline();
}

#[test]
fn remove_detaches_timer_from_queue() {
    ktimer::remove_detaches_timer_from_queue();
}

#[test]
fn left_most_cache_tracks_queue_mutations() {
    ktimer::left_most_cache_tracks_queue_mutations();
}

#[test]
fn first_active_cache_tracks_inactive_frontier_mutations() {
    ktimer::first_active_cache_tracks_inactive_frontier_mutations();
}

#[test]
fn refresh_next_ktimer_leaves_inactive_cfs_timer_inactive() {
    ktimer::refresh_next_ktimer_leaves_inactive_cfs_timer_inactive();
}

#[test]
fn dispatch_expired_returns_null_when_no_timer_is_active() {
    ktimer::dispatch_expired_returns_null_when_no_timer_is_active();
}

#[test]
fn pop_first_returns_timers_in_deadline_order() {
    ktimer::pop_first_returns_timers_in_deadline_order();
}

#[test]
fn rt_timing_constructor_separates_period_deadline_and_budget() {
    ktimer::rt_timing_constructor_separates_period_deadline_and_budget();
}

#[test]
fn rt_yield_marks_timer_inactive_and_preserves_remaining_period() {
    ktimer::rt_yield_marks_timer_inactive_and_preserves_remaining_period();
}

#[test]
fn rt_yield_with_reset_runtime_finishes_current_job_window() {
    ktimer::rt_yield_with_reset_runtime_finishes_current_job_window();
}

#[test]
fn rt_yield_without_active_timers_returns_null() {
    ktimer::rt_yield_without_active_timers_returns_null();
}

#[test]
fn rt_yield_uses_period_for_next_release_not_relative_deadline() {
    ktimer::rt_yield_uses_period_for_next_release_not_relative_deadline();
}

#[test]
fn rt_yield_records_budget_overrun_independent_of_deadline() {
    ktimer::rt_yield_records_budget_overrun_independent_of_deadline();
}

#[test]
fn wake_wait_thread_charges_wait_elapsed_to_rt_deadline() {
    ktimer::wake_wait_thread_charges_wait_elapsed_to_rt_deadline();
}

#[test]
fn wake_wait_thread_uses_relative_deadline_not_period() {
    ktimer::wake_wait_thread_uses_relative_deadline_not_period();
}

#[test]
fn rt_only_initialization_does_not_enqueue_cfs_ktimer() {
    ktimer::rt_only_initialization_does_not_enqueue_cfs_ktimer();
}

#[test]
fn rt_waiting_thread_is_moved_from_ktimer_queue_to_wait_queue() {
    ktimer::rt_waiting_thread_is_moved_from_ktimer_queue_to_wait_queue();
}

#[test]
fn dispatch_expired_active_rt_timer_records_deadline_miss() {
    ktimer::dispatch_expired_active_rt_timer_records_deadline_miss();
}

#[test]
fn dispatch_expired_active_rt_timer_uses_relative_deadline() {
    ktimer::dispatch_expired_active_rt_timer_uses_relative_deadline();
}

#[test]
fn dispatch_expired_active_rt_timer_without_runtime_over_deadline_is_not_miss() {
    ktimer::dispatch_expired_active_rt_timer_without_runtime_over_deadline_is_not_miss();
}

#[test]
fn dispatch_expired_inactive_rt_timer_reactivates_without_miss() {
    ktimer::dispatch_expired_inactive_rt_timer_reactivates_without_miss();
}

#[test]
fn dispatch_expired_inactive_rt_timer_uses_relative_deadline() {
    ktimer::dispatch_expired_inactive_rt_timer_uses_relative_deadline();
}
